//! Repository bootstrap, chunk staging, and the publication transaction.

use std::collections::BTreeMap;
use surrealdb::engine::local::Db;
use surrealdb::types::{Bytes, RecordId, SurrealValue};

use surrealfs_content::tree::{empty_root, DirNode};
use surrealfs_types::state::{commit_digest, kv_digest, KvMap, Mutation};
use surrealfs_types::{
    BranchName, ChunkDigest, CommitId, RepositoryId, RequestId, SfsError, StateRootId,
    HASH_VERSION, ROOT_FORMAT_VERSION,
};

use crate::plan::{
    CommitPlan, CommitReceipt, ReceiptOutcome, MAX_PUBLICATION_METADATA_BYTES,
    MAX_PUBLICATION_MUTATIONS,
};
use crate::{
    bodies, map_db_err, rid_branch, rid_chunk, rid_commit, rid_principal, rid_receipt, rid_repo,
    rid_state_node, rid_state_root, rid_tenant, Store,
};
use surrealfs_types::StateNodeId;

#[derive(SurrealValue)]
struct BranchRow {
    head: RecordId,
}

#[derive(SurrealValue)]
struct HeadCommitRow {
    domain_sequence: i64,
    state_root_digest: String,
}

#[derive(SurrealValue)]
pub(crate) struct ReceiptRow {
    pub command_hash: String,
    pub commit: RecordId,
    pub state_root_digest: String,
    pub previous_head: RecordId,
    pub domain_sequence: i64,
}

/// Current branch position returned by bootstrap/head queries.
#[derive(Debug, Clone)]
pub struct HeadInfo {
    pub head: CommitId,
    pub root: StateRootId,
    pub domain_sequence: u64,
}

pub(crate) fn commit_id_of_rid(rid: &RecordId) -> Result<CommitId, SfsError> {
    let surrealdb::types::RecordIdKey::String(key) = &rid.key else {
        return Err(SfsError::Corruption(format!(
            "unexpected record id key shape in table {}",
            rid.table
        )));
    };
    let digest = key.rsplit_once('/').map(|(_, d)| d).unwrap_or(key.as_str());
    CommitId::parse(digest)
}

impl Store {
    /// Open-or-create a repository: tenant, principal, repository record, empty root, and
    /// a genesis ROOT commit that `main` points at. Idempotent under the publish lock.
    pub async fn ensure_repository(&self, repo: &RepositoryId) -> Result<HeadInfo, SfsError> {
        let _guard = self.publish_lock.lock().await;
        if let Some(info) = self.head_inner(repo, &BranchName::main()).await? {
            self.check_encryption_agrees(repo).await?;
            return Ok(info);
        }

        let namespace_root = empty_root();
        let kv = KvMap::new();
        let root = surrealfs_types::state::root_digest(&namespace_root, &kv_digest(&kv));
        let genesis_request = RequestId::parse("genesis").expect("static id");
        let genesis = commit_digest(
            None,
            &root,
            &genesis_request,
            "embedded",
            Some("repository created"),
            0,
        );

        let tx = self
            .db()
            .clone()
            .begin()
            .await
            .map_err(|e| map_db_err("begin bootstrap", e))?;
        // Stamped once, at creation. Everything written afterwards is sealed or not according to
        // this marker, so the repository can always say which it is without inspecting content.
        let marker = self
            .is_encrypting()
            .then_some(crate::cipher::ENCRYPTION_MARKER);
        let result = bootstrap_tx(&tx, repo, &namespace_root, &kv, &root, &genesis, marker).await;
        match result {
            Ok(()) => {
                tx.commit()
                    .await
                    .map_err(|e| map_db_err("commit bootstrap", e))?;
                Ok(HeadInfo {
                    head: genesis,
                    root,
                    domain_sequence: 0,
                })
            }
            Err(err) => {
                let _ = tx.cancel().await;
                Err(err)
            }
        }
    }

    /// Refuse to open a repository whose encryption state disagrees with the key we hold.
    ///
    /// Both mismatches are caught here, before any content is read, so the failure names the
    /// actual problem instead of surfacing later as a decryption error on whichever chunk
    /// happened to be touched first.
    ///
    /// Rejecting a key on a *plaintext* repository looks pedantic and is the more important of
    /// the two: it prevents someone believing their data is encrypted when it is not.
    async fn check_encryption_agrees(&self, repo: &RepositoryId) -> Result<(), SfsError> {
        #[derive(SurrealValue)]
        struct Row {
            encryption: Option<String>,
        }
        let row: Option<Row> = self
            .db()
            .query("SELECT encryption FROM ONLY $rid")
            .bind(("rid", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read repository encryption", e))?
            .take(0)
            .map_err(|e| map_db_err("decode repository encryption", e))?;
        let stored = row.and_then(|r| r.encryption);

        match (stored.as_deref(), self.is_encrypting()) {
            (None, false) => Ok(()),
            (Some(marker), true) if marker == crate::cipher::ENCRYPTION_MARKER => Ok(()),
            (Some(marker), true) => Err(SfsError::Encryption(format!(
                "repository was written with {marker}, which this build does not implement"
            ))),
            (Some(_), false) => Err(SfsError::Encryption(
                "repository content is encrypted; supply --key or set SURREALFS_KEY".into(),
            )),
            (None, true) => Err(SfsError::Encryption(
                "repository content is not encrypted; refusing to open it with a key, because \
                 doing so would leave earlier content readable while implying otherwise"
                    .into(),
            )),
        }
    }

    /// Current head of a branch, or an error if the repository/branch is missing.
    pub async fn head(
        &self,
        repo: &RepositoryId,
        branch: &BranchName,
    ) -> Result<HeadInfo, SfsError> {
        self.head_inner(repo, branch)
            .await?
            .ok_or_else(|| SfsError::NotFound(format!("branch {repo}/{branch}")))
    }

    async fn head_inner(
        &self,
        repo: &RepositoryId,
        branch: &BranchName,
    ) -> Result<Option<HeadInfo>, SfsError> {
        let row: Option<BranchRow> = self
            .db()
            .query("SELECT head FROM ONLY $rid")
            .bind(("rid", rid_branch(repo, branch)))
            .await
            .map_err(|e| map_db_err("read branch", e))?
            .take(0)
            .map_err(|e| map_db_err("decode branch", e))?;
        let Some(row) = row else { return Ok(None) };
        let head = commit_id_of_rid(&row.head)?;
        let commit: Option<HeadCommitRow> = self
            .db()
            .query("SELECT domain_sequence, state_root_digest FROM ONLY $rid")
            .bind(("rid", row.head))
            .await
            .map_err(|e| map_db_err("read head commit", e))?
            .take(0)
            .map_err(|e| map_db_err("decode head commit", e))?;
        let commit = commit.ok_or_else(|| {
            SfsError::Corruption(format!("branch {repo}/{branch} head has no commit record"))
        })?;
        Ok(Some(HeadInfo {
            head,
            root: StateRootId::parse(&commit.state_root_digest)?,
            domain_sequence: commit.domain_sequence as u64,
        }))
    }

    /// Stage content chunks before publication. Idempotent, content-addressed, and outside
    /// the publication transaction by design: staged-but-unreferenced chunks are invisible
    /// and reclaimed by later GC.
    pub async fn stage_chunks(
        &self,
        repo: &RepositoryId,
        chunks: &[(ChunkDigest, Vec<u8>)],
    ) -> Result<(), SfsError> {
        for (digest, bytes) in chunks {
            // `length` stays the plaintext length whether or not the body is sealed: GC reports
            // reclaimed bytes from this column, and describing the envelope rather than the data
            // would answer a question nobody asked.
            let (stored, kind) = match self.cipher() {
                Some(cipher) => (cipher.seal(digest, bytes)?, "ENCRYPTED"),
                None => (bytes.clone(), "INLINE"),
            };
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, hash_version: $hv, digest: $digest, \
                     length: $len, storage_kind: $kind, inline_bytes: $bytes, created_at: time::now() }",
                )
                .bind(("rid", rid_chunk(repo, digest)))
                .bind(("repo", rid_repo(repo)))
                .bind(("hv", HASH_VERSION as i64))
                .bind(("digest", digest.as_str().to_string()))
                .bind(("len", bytes.len() as i64))
                .bind(("kind", kind))
                .bind(("bytes", Bytes::from(stored)))
                .await
                .map_err(|e| map_db_err("stage chunk", e))?
                .check()
                .map_err(|e| map_db_err("stage chunk", e))?;
        }
        Ok(())
    }

    /// Look up a stored receipt for a request id.
    pub async fn receipt(
        &self,
        repo: &RepositoryId,
        request_id: &RequestId,
    ) -> Result<Option<CommitReceipt>, SfsError> {
        let row: Option<ReceiptRow> = self
            .db()
            .query(
                "SELECT command_hash, commit, state_root_digest, previous_head, domain_sequence \
                 FROM ONLY $rid",
            )
            .bind(("rid", rid_receipt(repo, request_id)))
            .await
            .map_err(|e| map_db_err("read receipt", e))?
            .take(0)
            .map_err(|e| map_db_err("decode receipt", e))?;
        row.map(|row| receipt_of_row(request_id, &row, ReceiptOutcome::Replayed))
            .transpose()
    }

    /// The publication transaction: receipt check, expected-head compare-and-swap, staged
    /// content verification, immutable metadata writes, head advance, receipt store —
    /// committed once. See RUST_SDK_PLAN.md "Workspace publication transaction".
    pub async fn publish(&self, plan: &CommitPlan) -> Result<CommitReceipt, SfsError> {
        if plan.mutations.len() > MAX_PUBLICATION_MUTATIONS {
            return Err(SfsError::OverBudget(format!(
                "{} mutations exceeds the {MAX_PUBLICATION_MUTATIONS} publication budget",
                plan.mutations.len()
            )));
        }
        let mut metadata_bytes = bodies::encode_kv(&plan.kv)?.len();
        for node in plan.new_nodes.values() {
            metadata_bytes += bodies::encode_dir(node)?.len();
        }
        if metadata_bytes > MAX_PUBLICATION_METADATA_BYTES {
            return Err(SfsError::OverBudget(format!(
                "{metadata_bytes} metadata bytes exceeds the {MAX_PUBLICATION_METADATA_BYTES} publication budget"
            )));
        }
        let command_hash = plan.command_hash();

        let _guard = self.publish_lock.lock().await;

        // Idempotency fast path.
        if let Some(existing) = self.receipt(&plan.repository, &plan.request_id).await? {
            return check_replay(plan, &command_hash, existing);
        }

        let tx = self
            .db()
            .clone()
            .begin()
            .await
            .map_err(|e| map_db_err("begin publication", e))?;
        let result = publish_tx(&tx, plan, &command_hash).await;
        match result {
            Ok(receipt) => {
                tx.commit().await.map_err(|e| {
                    // The commit call failed in-process, so nothing was applied; this is
                    // not the ambiguous crash window (that is resolved by receipt lookup
                    // on reopen).
                    map_db_err("commit publication", e)
                })?;
                Ok(receipt)
            }
            Err(err) => {
                let _ = tx.cancel().await;
                Err(err)
            }
        }
    }
}

fn check_replay(
    plan: &CommitPlan,
    command_hash: &surrealfs_types::Digest,
    existing: CommitReceipt,
) -> Result<CommitReceipt, SfsError> {
    if existing.command_hash == *command_hash {
        Ok(existing)
    } else {
        Err(SfsError::RequestMismatch {
            request_id: plan.request_id.to_string(),
        })
    }
}

fn receipt_of_row(
    request_id: &RequestId,
    row: &ReceiptRow,
    outcome: ReceiptOutcome,
) -> Result<CommitReceipt, SfsError> {
    Ok(CommitReceipt {
        request_id: request_id.clone(),
        outcome,
        commit: commit_id_of_rid(&row.commit)?,
        state_root: StateRootId::parse(&row.state_root_digest)?,
        previous_head: commit_id_of_rid(&row.previous_head)?,
        domain_sequence: row.domain_sequence as u64,
        command_hash: surrealfs_types::Digest::parse(&row.command_hash)?,
    })
}

async fn bootstrap_tx(
    tx: &surrealdb::method::Transaction<Db>,
    repo: &RepositoryId,
    namespace_root: &StateNodeId,
    kv: &KvMap,
    root: &StateRootId,
    genesis: &CommitId,
    encryption: Option<&str>,
) -> Result<(), SfsError> {
    tx.query("UPSERT $rid SET slug = 'default', created_at = time::now()")
        .bind(("rid", rid_tenant()))
        .await
        .map_err(|e| map_db_err("create tenant", e))?
        .check()
        .map_err(|e| map_db_err("create tenant", e))?;
    tx.query(
        "UPSERT $rid SET tenant = $tenant, kind = 'SERVICE', external_subject = 'embedded', \
         created_at = time::now()",
    )
    .bind(("rid", rid_principal()))
    .bind(("tenant", rid_tenant()))
    .await
    .map_err(|e| map_db_err("create principal", e))?
    .check()
    .map_err(|e| map_db_err("create principal", e))?;
    tx.query(
        "CREATE $rid SET tenant = $tenant, slug = $slug, path_mode = 'BYTE_CASE_SENSITIVE', \
         hash_version = $hv, root_format_version = $rv, encryption = $encryption, \
         created_at = time::now()",
    )
    .bind(("rid", rid_repo(repo)))
    .bind(("tenant", rid_tenant()))
    .bind(("slug", repo.as_str().to_string()))
    .bind(("hv", HASH_VERSION as i64))
    .bind(("rv", ROOT_FORMAT_VERSION as i64))
    .bind(("encryption", encryption.map(str::to_string)))
    .await
    .map_err(|e| map_db_err("create repository", e))?
    .check()
    .map_err(|e| map_db_err("create repository", e))?;

    write_state(tx, repo, &BTreeMap::new(), kv, namespace_root, root).await?;

    tx.query(
        "CREATE $rid SET repository = $repo, kind = 'ROOT', first_parent = NONE, \
         author_principal = $principal, author_span = NONE, request_id = 'genesis', \
         message = 'repository created', mutation_count = 0, state_root = $root_rid, \
         state_root_digest = $root_digest, hash_version = $hv, domain_sequence = 0, \
         committed_at = time::now()",
    )
    .bind(("rid", rid_commit(repo, genesis)))
    .bind(("repo", rid_repo(repo)))
    .bind(("principal", rid_principal()))
    .bind(("root_rid", rid_state_root(repo, root)))
    .bind(("root_digest", root.as_str().to_string()))
    .bind(("hv", HASH_VERSION as i64))
    .await
    .map_err(|e| map_db_err("create genesis commit", e))?
    .check()
    .map_err(|e| map_db_err("create genesis commit", e))?;

    tx.query(
        "CREATE $rid SET repository = $repo, name = $name, head = $head, \
         created_at = time::now(), updated_at = time::now()",
    )
    .bind(("rid", rid_branch(repo, &BranchName::main())))
    .bind(("repo", rid_repo(repo)))
    .bind(("name", BranchName::main().as_str().to_string()))
    .bind(("head", rid_commit(repo, genesis)))
    .await
    .map_err(|e| map_db_err("create branch", e))?
    .check()
    .map_err(|e| map_db_err("create branch", e))?;
    Ok(())
}

/// Persist the nodes a commit introduces plus its root.
///
/// Only `new_nodes` are written: tree nodes reachable from the base root are already stored
/// and are shared by digest. Writes are UPSERTs on content-addressed ids, so a node that two
/// commits both introduce is written idempotently rather than conflicting.
async fn write_state(
    tx: &surrealdb::method::Transaction<Db>,
    repo: &RepositoryId,
    new_nodes: &BTreeMap<StateNodeId, DirNode>,
    kv: &KvMap,
    namespace_root: &StateNodeId,
    root: &StateRootId,
) -> Result<(), SfsError> {
    for (id, node) in new_nodes {
        write_node(tx, repo, "DIR", id, &bodies::encode_dir(node)?).await?;
    }
    // The empty directory is a constant that `TreeWriter` never emits, so a root that points
    // at it (a fresh repository, or one emptied by this commit) would otherwise reference a
    // row that does not exist. Materialise it so `state_root.namespace_node` always resolves.
    if *namespace_root == empty_root() {
        write_node(
            tx,
            repo,
            "DIR",
            namespace_root,
            &bodies::encode_dir(&DirNode::default())?,
        )
        .await?;
    }
    let kv_node = kv_digest(kv);
    write_node(tx, repo, "KV", &kv_node, &bodies::encode_kv(kv)?).await?;

    tx.query(
        "UPSERT $rid CONTENT { repository: $repo, root_format_version: $rv, \
         namespace_node: $ns, kv_node: $kv, digest: $digest, created_at: time::now() }",
    )
    .bind(("rid", rid_state_root(repo, root)))
    .bind(("repo", rid_repo(repo)))
    .bind(("rv", ROOT_FORMAT_VERSION as i64))
    .bind(("ns", rid_state_node(repo, namespace_root)))
    .bind(("kv", rid_state_node(repo, &kv_node)))
    .bind(("digest", root.as_str().to_string()))
    .await
    .map_err(|e| map_db_err("write state root", e))?
    .check()
    .map_err(|e| map_db_err("write state root", e))?;
    Ok(())
}

async fn write_node(
    tx: &surrealdb::method::Transaction<Db>,
    repo: &RepositoryId,
    kind: &str,
    id: &StateNodeId,
    json: &str,
) -> Result<(), SfsError> {
    tx.query(
        "UPSERT $rid CONTENT { repository: $repo, root_format_version: $rv, kind: $kind, \
         digest: $digest, body: { json: $json }, created_at: time::now() }",
    )
    .bind(("rid", rid_state_node(repo, id)))
    .bind(("repo", rid_repo(repo)))
    .bind(("rv", ROOT_FORMAT_VERSION as i64))
    .bind(("kind", kind.to_string()))
    .bind(("digest", id.as_str().to_string()))
    .bind(("json", json.to_string()))
    .await
    .map_err(|e| map_db_err("write state node", e))?
    .check()
    .map_err(|e| map_db_err("write state node", e))?;
    Ok(())
}

async fn publish_tx(
    tx: &surrealdb::method::Transaction<Db>,
    plan: &CommitPlan,
    command_hash: &surrealfs_types::Digest,
) -> Result<CommitReceipt, SfsError> {
    let repo = &plan.repository;

    // Receipt re-check inside the transaction (defense in depth under the publish lock).
    let existing: Option<ReceiptRow> = tx
        .query("SELECT command_hash, commit, state_root_digest, previous_head, domain_sequence FROM ONLY $rid")
        .bind(("rid", rid_receipt(repo, &plan.request_id)))
        .await
        .map_err(|e| map_db_err("recheck receipt", e))?
        .take(0)
        .map_err(|e| map_db_err("decode receipt", e))?;
    if existing.is_some() {
        return Err(SfsError::Ambiguous {
            request_id: plan.request_id.to_string(),
            detail: "receipt appeared during publication; resolve by receipt lookup".into(),
        });
    }

    // Expected-head compare-and-swap.
    let branch: Option<BranchRow> = tx
        .query("SELECT head FROM ONLY $rid")
        .bind(("rid", rid_branch(repo, &plan.branch)))
        .await
        .map_err(|e| map_db_err("read branch", e))?
        .take(0)
        .map_err(|e| map_db_err("decode branch", e))?;
    let branch =
        branch.ok_or_else(|| SfsError::NotFound(format!("branch {repo}/{}", plan.branch)))?;
    let actual_head = commit_id_of_rid(&branch.head)?;
    if actual_head != plan.expected_head {
        return Err(SfsError::HeadConflict {
            branch: plan.branch.to_string(),
            expected: plan.expected_head.to_string(),
            actual: actual_head.to_string(),
        });
    }
    let head_commit: Option<HeadCommitRow> = tx
        .query("SELECT domain_sequence, state_root_digest FROM ONLY $rid")
        .bind(("rid", rid_commit(repo, &actual_head)))
        .await
        .map_err(|e| map_db_err("read head commit", e))?
        .take(0)
        .map_err(|e| map_db_err("decode head commit", e))?;
    // The value is not needed — the sequence is allocated repository-wide below — but a head
    // pointing at a commit that does not exist is corruption worth catching here.
    head_commit.ok_or_else(|| SfsError::Corruption("branch head has no commit record".into()))?;

    // Verify the staged content this publication introduces.
    let mut required: Vec<RecordId> = Vec::new();
    for m in &plan.mutations {
        match m {
            Mutation::WriteFile { content, .. } => {
                required.extend(content.iter().map(|e| rid_chunk(repo, &e.chunk)));
            }
            Mutation::KvSet { value, .. } => required.push(rid_chunk(repo, value)),
            _ => {}
        }
    }
    required.sort();
    required.dedup();
    if !required.is_empty() {
        let found: Vec<RecordId> = tx
            .query("SELECT VALUE id FROM $chunks")
            .bind(("chunks", required.clone()))
            .await
            .map_err(|e| map_db_err("verify staged chunks", e))?
            .take(0)
            .map_err(|e| map_db_err("decode staged chunks", e))?;
        if found.len() != required.len() {
            return Err(SfsError::Corruption(format!(
                "publication references {} chunks but only {} are staged",
                required.len(),
                found.len()
            )));
        }
    }

    let new_root = plan.new_root();
    let highest: Vec<i64> = tx
        .query(
            "SELECT VALUE domain_sequence FROM commit WHERE repository = $repo \
             ORDER BY domain_sequence DESC LIMIT 1",
        )
        .bind(("repo", rid_repo(repo)))
        .await
        .map_err(|e| map_db_err("read commit sequence", e))?
        .take(0)
        .map_err(|e| map_db_err("decode commit sequence", e))?;
    let domain_sequence = highest.first().copied().unwrap_or(0) as u64 + 1;
    let commit_id = commit_digest(
        Some(&actual_head),
        &new_root,
        &plan.request_id,
        "embedded",
        plan.message.as_deref(),
        plan.mutations.len() as u64,
    );

    write_state(
        tx,
        repo,
        &plan.new_nodes,
        &plan.kv,
        &plan.namespace_root,
        &new_root,
    )
    .await?;

    tx.query(
        "CREATE $rid SET repository = $repo, kind = 'NORMAL', first_parent = $parent, \
         author_principal = $principal, author_span = $span, request_id = $request_id, \
         message = $message, mutation_count = $mcount, state_root = $root_rid, \
         state_root_digest = $root_digest, hash_version = $hv, domain_sequence = $seq, \
         committed_at = time::now()",
    )
    .bind(("rid", rid_commit(repo, &commit_id)))
    .bind(("repo", rid_repo(repo)))
    .bind(("parent", rid_commit(repo, &actual_head)))
    .bind(("principal", rid_principal()))
    .bind((
        "span",
        plan.author_span
            .as_ref()
            .map(|s| RecordId::new("span", s.as_str())),
    ))
    .bind(("request_id", plan.request_id.as_str().to_string()))
    .bind(("message", plan.message.clone()))
    .bind(("mcount", plan.mutations.len() as i64))
    .bind(("root_rid", rid_state_root(repo, &new_root)))
    .bind(("root_digest", new_root.as_str().to_string()))
    .bind(("hv", HASH_VERSION as i64))
    .bind(("seq", domain_sequence as i64))
    .await
    .map_err(|e| map_db_err("create commit", e))?
    .check()
    .map_err(|e| map_db_err("create commit", e))?;

    tx.query("RELATE $parent->parent_of->$child SET repository = $repo")
        .bind(("parent", rid_commit(repo, &actual_head)))
        .bind(("child", rid_commit(repo, &commit_id)))
        .bind(("repo", rid_repo(repo)))
        .await
        .map_err(|e| map_db_err("relate parent", e))?
        .check()
        .map_err(|e| map_db_err("relate parent", e))?;

    for (ordinal, mutation) in plan.mutations.iter().enumerate() {
        let body = serde_json::to_string(mutation)
            .map_err(|e| SfsError::Storage(format!("encode mutation: {e}")))?;
        tx.query(
            "CREATE $rid SET repository = $repo, commit = $commit, ordinal = $ordinal, \
             kind = $kind, path = $path, domain_sequence = $seq, body = { json: $json }",
        )
        .bind((
            "rid",
            RecordId::new("commit_mutation", format!("{repo}/{commit_id}/{ordinal}")),
        ))
        .bind(("repo", rid_repo(repo)))
        .bind(("commit", rid_commit(repo, &commit_id)))
        .bind(("ordinal", ordinal as i64))
        .bind(("kind", mutation.kind_str().to_string()))
        .bind(("path", mutation.target_path()))
        .bind(("seq", domain_sequence as i64))
        .bind(("json", body))
        .await
        .map_err(|e| map_db_err("create mutation", e))?
        .check()
        .map_err(|e| map_db_err("create mutation", e))?;
    }

    if let Some(span_key) = &plan.author_span {
        tx.query("RELATE $span->caused->$commit SET repository = $repo")
            .bind(("span", RecordId::new("span", span_key.as_str())))
            .bind(("commit", rid_commit(repo, &commit_id)))
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("relate caused", e))?
            .check()
            .map_err(|e| map_db_err("relate caused", e))?;
    }

    if let Some(ws_key) = &plan.workspace {
        tx.query(
            "UPDATE $ws SET status = 'COMMITTED', final_commit = $commit, closed_at = time::now()",
        )
        .bind(("ws", RecordId::new("workspace", ws_key.as_str())))
        .bind(("commit", rid_commit(repo, &commit_id)))
        .await
        .map_err(|e| map_db_err("close workspace", e))?
        .check()
        .map_err(|e| map_db_err("close workspace", e))?;
        tx.query("RELATE $ws->published_as->$commit SET repository = $repo")
            .bind(("ws", RecordId::new("workspace", ws_key.as_str())))
            .bind(("commit", rid_commit(repo, &commit_id)))
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("relate published_as", e))?
            .check()
            .map_err(|e| map_db_err("relate published_as", e))?;
    }

    tx.query("UPDATE $rid SET head = $head, updated_at = time::now()")
        .bind(("rid", rid_branch(repo, &plan.branch)))
        .bind(("head", rid_commit(repo, &commit_id)))
        .await
        .map_err(|e| map_db_err("advance head", e))?
        .check()
        .map_err(|e| map_db_err("advance head", e))?;

    tx.query(
        "CREATE $rid SET repository = $repo, request_id = $request_id, command_hash = $hash, \
         outcome = 'APPLIED', commit = $commit, state_root_digest = $root_digest, \
         previous_head = $previous, domain_sequence = $seq, created_at = time::now()",
    )
    .bind(("rid", rid_receipt(repo, &plan.request_id)))
    .bind(("repo", rid_repo(repo)))
    .bind(("request_id", plan.request_id.as_str().to_string()))
    .bind(("hash", command_hash.as_str().to_string()))
    .bind(("commit", rid_commit(repo, &commit_id)))
    .bind(("root_digest", new_root.as_str().to_string()))
    .bind(("previous", rid_commit(repo, &actual_head)))
    .bind(("seq", domain_sequence as i64))
    .await
    .map_err(|e| map_db_err("store receipt", e))?
    .check()
    .map_err(|e| map_db_err("store receipt", e))?;

    Ok(CommitReceipt {
        request_id: plan.request_id.clone(),
        outcome: ReceiptOutcome::Applied,
        commit: commit_id,
        state_root: new_root,
        previous_head: actual_head,
        domain_sequence,
        command_hash: command_hash.clone(),
    })
}
