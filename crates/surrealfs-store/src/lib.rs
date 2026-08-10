//! SurrealDB store adapter.
//!
//! The only crate that talks to the database, and it does so exclusively through the
//! public `surrealdb` Rust SDK with `kv-mem`/`kv-surrealkv`. It owns migrations,
//! deterministic record-id encoding, the publication transaction, expected-head and
//! idempotency checks, and conversion from engine errors to typed domain errors.
//! Filesystem policy lives in the kernel, never here.

pub mod archive;
pub mod bodies;
mod branches;
pub mod cipher;
mod gc;
pub mod plan;
mod publish;
mod read;
mod resident;

use std::path::PathBuf;

use surrealdb::engine::local::{Db, Mem, SurrealKv};
use surrealdb::types::{RecordId, SurrealValue};
use surrealdb::Surreal;
use surrealfs_types::{
    BranchName, ChunkDigest, CommitId, RepositoryId, RequestId, SfsError, StateNodeId, StateRootId,
    HASH_VERSION, ROOT_FORMAT_VERSION, SCHEMA_VERSION,
};

pub use archive::{read_archive, ArchiveContents, ArchiveStats};
pub use branches::{BranchInfo, SnapshotInfo};
pub use gc::{GcReport, DEFAULT_GRACE_SECONDS};
pub use plan::{CommitPlan, CommitReceipt, ReceiptOutcome};
pub use publish::HeadInfo;
pub use read::{CommitInfo, Provenance, ToolCallInfo, ToolStats};
pub use resident::{CacheStats, ResidentNodes, DEFAULT_CAPACITY};

/// Storage selection: in-process memory for tests/ephemeral repositories, SurrealKV for
/// persistent ones. No other backend exists.
#[derive(Debug, Clone)]
pub enum StoreEngine {
    Memory,
    SurrealKv(PathBuf),
}

/// How to open a store, beyond which engine backs it.
///
/// Separate from [`StoreEngine`] because the engine says where bytes live and this says how they
/// are written. Defaulting to no key keeps every existing caller and test unchanged.
#[derive(Debug)]
pub struct StoreConfig {
    /// Encrypt chunk bodies with this key. Absent means bodies are stored in the clear.
    pub key: Option<cipher::ChunkKey>,
    /// Apply pending migrations on open. Defaults to true.
    ///
    /// Turning it off exists so a migration can be *inspected* before it runs: opening a
    /// repository is how migrations are normally applied, which leaves no moment to ask what
    /// would happen. `surrealfs migrate status` opens this way.
    pub apply_migrations: bool,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            key: None,
            apply_migrations: true,
        }
    }
}

impl StoreConfig {
    pub fn with_key(key: cipher::ChunkKey) -> Self {
        StoreConfig {
            key: Some(key),
            ..StoreConfig::default()
        }
    }

    /// Open without applying anything, for inspection.
    pub fn read_only() -> Self {
        StoreConfig {
            apply_migrations: false,
            ..StoreConfig::default()
        }
    }
}

const NS: &str = "surrealfs";
const DB: &str = "main";

/// Migration list: (id, DDL). Append-only; never edit an applied migration.
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001-core",
        include_str!("../../../schema/migrations/0001-core.surql"),
    ),
    (
        "0002-snapshots",
        include_str!("../../../schema/migrations/0002-snapshots.surql"),
    ),
    (
        "0003-tool-stats",
        include_str!("../../../schema/migrations/0003-tool-stats.surql"),
    ),
    (
        "0004-encryption",
        include_str!("../../../schema/migrations/0004-encryption.surql"),
    ),
];

/// A migration's state in one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationState {
    /// Applied, and its source still hashes to what was recorded.
    Applied(String),
    /// Known to this build and not yet applied here.
    Pending(String),
    /// Started and never finished. Writes are refused until this is resolved by hand.
    Interrupted(String),
    /// Applied, but the source has changed since — migrations are append-only, so this means
    /// somebody edited one that had already run.
    Changed(String),
}

impl MigrationState {
    pub fn id(&self) -> &str {
        match self {
            MigrationState::Applied(id)
            | MigrationState::Pending(id)
            | MigrationState::Interrupted(id)
            | MigrationState::Changed(id) => id,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            MigrationState::Applied(_) => "applied",
            MigrationState::Pending(_) => "pending",
            MigrationState::Interrupted(_) => "INTERRUPTED",
            MigrationState::Changed(_) => "CHANGED SINCE APPLIED",
        }
    }

    /// Whether this state blocks opening the repository for writes.
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            MigrationState::Interrupted(_) | MigrationState::Changed(_)
        )
    }
}

pub struct Store {
    db: Surreal<Db>,
    /// Process-lifetime cache of tree nodes, keyed by content digest. A cache, never truth:
    /// nodes are immutable, so an entry cannot go stale and this needs no invalidation.
    resident: resident::ResidentNodes,
    /// Single semantic writer: publications are serialized in-process. The in-transaction
    /// expected-head check remains as defense in depth and for cross-restart correctness.
    publish_lock: tokio::sync::Mutex<()>,
    /// Seals chunk bodies on the way in and opens them on the way out. Absent means bodies are
    /// stored in the clear; the repository record says which, so the two can never disagree
    /// silently.
    cipher: Option<cipher::ChunkCipher>,
}

pub(crate) fn map_db_err(context: &str, err: surrealdb::Error) -> SfsError {
    SfsError::Storage(format!("{context}: {err}"))
}

// Deterministic record-id encoding. Composite keys join with '/' — safe because
// repository ids and branch names are validated slugs and digests are hex.
pub(crate) fn rid_tenant() -> RecordId {
    RecordId::new("tenant", "default")
}

pub(crate) fn rid_principal() -> RecordId {
    RecordId::new("principal", "embedded")
}

pub(crate) fn rid_repo(repo: &RepositoryId) -> RecordId {
    RecordId::new("repository", repo.as_str())
}

pub(crate) fn rid_branch(repo: &RepositoryId, branch: &BranchName) -> RecordId {
    RecordId::new("branch", format!("{repo}/{branch}"))
}

pub(crate) fn rid_commit(repo: &RepositoryId, commit: &CommitId) -> RecordId {
    RecordId::new("commit", format!("{repo}/{commit}"))
}

pub(crate) fn rid_state_node(repo: &RepositoryId, node: &StateNodeId) -> RecordId {
    RecordId::new("state_node", format!("{repo}/{node}"))
}

pub(crate) fn rid_state_root(repo: &RepositoryId, root: &StateRootId) -> RecordId {
    RecordId::new("state_root", format!("{repo}/{root}"))
}

pub(crate) fn rid_chunk(repo: &RepositoryId, chunk: &ChunkDigest) -> RecordId {
    RecordId::new("chunk", format!("{repo}/{chunk}"))
}

pub(crate) fn rid_receipt(repo: &RepositoryId, request: &RequestId) -> RecordId {
    RecordId::new("request_receipt", format!("{repo}/{request}"))
}

impl Store {
    /// The chunk cipher, if this store was opened with a key.
    pub(crate) fn cipher(&self) -> Option<&cipher::ChunkCipher> {
        self.cipher.as_ref()
    }

    /// Whether this store holds a key at all.
    pub fn is_encrypting(&self) -> bool {
        self.cipher.is_some()
    }

    /// Open the selected engine through the public SDK and apply pending migrations.
    pub async fn open(engine: StoreEngine) -> Result<Self, SfsError> {
        Store::open_with(engine, StoreConfig::default()).await
    }

    /// Open with encryption configured.
    pub async fn open_with(engine: StoreEngine, config: StoreConfig) -> Result<Self, SfsError> {
        let db = match engine {
            StoreEngine::Memory => Surreal::new::<Mem>(())
                .await
                .map_err(|e| map_db_err("open memory engine", e))?,
            StoreEngine::SurrealKv(path) => Surreal::new::<SurrealKv>(path.clone())
                .await
                .map_err(|e| map_db_err("open surrealkv engine", e))?,
        };
        db.use_ns(NS)
            .use_db(DB)
            .await
            .map_err(|e| map_db_err("select namespace", e))?;
        let store = Store {
            db,
            resident: resident::ResidentNodes::default(),
            publish_lock: tokio::sync::Mutex::new(()),
            cipher: config.key.as_ref().map(cipher::ChunkCipher::new),
        };
        if config.apply_migrations {
            store.apply_migrations().await?;
        }
        Ok(store)
    }

    /// What each known migration's state is, without changing anything.
    ///
    /// Opening a repository is how migrations normally get applied, which leaves no moment to
    /// ask what an upgrade would do. Pair this with `StoreConfig::read_only`.
    pub async fn migration_status(&self) -> Result<Vec<MigrationState>, SfsError> {
        let mut out = Vec::with_capacity(MIGRATIONS.len());
        for (id, ddl) in MIGRATIONS {
            let checksum = surrealfs_types::canonical::digest("migration", ddl.as_bytes());
            // A repository so old it predates the ledger tables reads as everything pending,
            // which is exactly right.
            let existing: Option<MigrationRow> = match self
                .db
                .query("SELECT checksum, status FROM ONLY $rid")
                .bind(("rid", RecordId::new("migration_receipt", *id)))
                .await
            {
                Ok(mut response) => response.take(0).unwrap_or(None),
                Err(_) => None,
            };
            let state = match existing {
                None => MigrationState::Pending(id.to_string()),
                Some(row) if row.status != "APPLIED" => MigrationState::Interrupted(id.to_string()),
                Some(row) if row.checksum != checksum.as_str() => {
                    MigrationState::Changed(id.to_string())
                }
                Some(_) => MigrationState::Applied(id.to_string()),
            };
            out.push(state);
        }
        Ok(out)
    }

    /// Apply pending migrations with a checksum ledger. A checksum mismatch on an applied
    /// migration is a refuse-to-write integrity failure, not a re-apply.
    async fn apply_migrations(&self) -> Result<(), SfsError> {
        // The ledger tables themselves are bootstrapped idempotently so the very first
        // open can read them before migration 0001 exists.
        self.db()
            .query(
                "DEFINE TABLE IF NOT EXISTS migration_receipt TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
                 DEFINE FIELD IF NOT EXISTS migration_id ON TABLE migration_receipt TYPE string;
                 DEFINE FIELD IF NOT EXISTS checksum ON TABLE migration_receipt TYPE string;
                 DEFINE FIELD IF NOT EXISTS status ON TABLE migration_receipt TYPE string
                     ASSERT $value IN ['STARTED', 'APPLIED'];
                 DEFINE FIELD IF NOT EXISTS started_at ON TABLE migration_receipt TYPE datetime;
                 DEFINE FIELD IF NOT EXISTS finished_at ON TABLE migration_receipt TYPE option<datetime>;
                 DEFINE TABLE IF NOT EXISTS schema_manifest TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;
                 DEFINE FIELD IF NOT EXISTS schema_version ON TABLE schema_manifest TYPE int ASSERT $value >= 0;
                 DEFINE FIELD IF NOT EXISTS export_version ON TABLE schema_manifest TYPE int ASSERT $value >= 0;
                 DEFINE FIELD IF NOT EXISTS hash_version ON TABLE schema_manifest TYPE int ASSERT $value > 0;
                 DEFINE FIELD IF NOT EXISTS root_format_version ON TABLE schema_manifest TYPE int ASSERT $value > 0;
                 DEFINE FIELD IF NOT EXISTS engine_revision ON TABLE schema_manifest TYPE string;
                 DEFINE FIELD IF NOT EXISTS applied_at ON TABLE schema_manifest TYPE datetime;",
            )
            .await
            .map_err(|e| map_db_err("bootstrap migration ledger", e))?
            .check()
            .map_err(|e| map_db_err("bootstrap migration ledger", e))?;
        for (id, ddl) in MIGRATIONS {
            let checksum = surrealfs_types::canonical::digest("migration", ddl.as_bytes());
            let existing: Option<MigrationRow> = self
                .db
                .query("SELECT checksum, status FROM ONLY $rid")
                .bind(("rid", RecordId::new("migration_receipt", *id)))
                .await
                .map_err(|e| map_db_err("read migration receipt", e))?
                .take(0)
                .map_err(|e| map_db_err("decode migration receipt", e))?;
            match existing {
                Some(row) if row.status == "APPLIED" => {
                    if row.checksum != checksum.as_str() {
                        return Err(SfsError::Migration(format!(
                            "migration {id} was applied with checksum {} but source now hashes to {checksum}",
                            row.checksum
                        )));
                    }
                    continue;
                }
                Some(_) => {
                    return Err(SfsError::Migration(format!(
                        "migration {id} previously STARTED but never finished; refusing writes"
                    )))
                }
                None => {}
            }
            self.db
                .query("CREATE $rid SET migration_id = $id, checksum = $checksum, status = 'STARTED', started_at = time::now(), finished_at = NONE")
                .bind(("rid", RecordId::new("migration_receipt", *id)))
                .bind(("id", id.to_string()))
                .bind(("checksum", checksum.as_str().to_string()))
                .await
                .map_err(|e| map_db_err("start migration", e))?
                .check()
                .map_err(|e| map_db_err("start migration", e))?;
            self.db
                .query(*ddl)
                .await
                .map_err(|e| SfsError::Migration(format!("apply {id}: {e}")))?
                .check()
                .map_err(|e| SfsError::Migration(format!("apply {id}: {e}")))?;
            self.db
                .query("UPDATE $rid SET status = 'APPLIED', finished_at = time::now()")
                .bind(("rid", RecordId::new("migration_receipt", *id)))
                .await
                .map_err(|e| map_db_err("finish migration", e))?
                .check()
                .map_err(|e| map_db_err("finish migration", e))?;
        }
        // Record the manifest for this schema version (idempotent).
        self.db
            .query(
                "UPSERT $rid SET schema_version = $sv, export_version = 0, hash_version = $hv, \
                 root_format_version = $rv, engine_revision = $rev, applied_at = time::now()",
            )
            .bind(("rid", RecordId::new("schema_manifest", "current")))
            .bind(("sv", SCHEMA_VERSION as i64))
            .bind(("hv", HASH_VERSION as i64))
            .bind(("rv", ROOT_FORMAT_VERSION as i64))
            .bind(("rev", env!("CARGO_PKG_VERSION").to_string()))
            .await
            .map_err(|e| map_db_err("write schema manifest", e))?
            .check()
            .map_err(|e| map_db_err("write schema manifest", e))?;
        Ok(())
    }

    pub(crate) fn db(&self) -> &Surreal<Db> {
        &self.db
    }
}

#[derive(SurrealValue)]
struct MigrationRow {
    checksum: String,
    status: String,
}

impl Store {
    /// Raw database handle, for tests that need to corrupt or inspect projections directly.
    /// Never used by product code: every other caller goes through a typed method.
    #[doc(hidden)]
    pub fn db_for_test(&self) -> &Surreal<Db> {
        &self.db
    }
}
