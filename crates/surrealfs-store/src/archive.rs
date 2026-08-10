//! Portable session archives.
//!
//! AgentFS gets "a session in a single file you can share" for free by being one SQLite file.
//! A SurrealKV repository is a directory, so the equivalent has to be explicit — and making it
//! explicit buys something the file-copy approach cannot offer: the archive is engine-
//! independent, and every root is re-derived on import rather than trusted.
//!
//! The framing is deliberately dull: a magic line, then uniform records of
//! `[kind][json length][json][blob length][blob]`. Content and tree nodes carry their digests,
//! and import rejects any record whose bytes disagree with the digest it arrived under, so a
//! truncated or tampered archive fails loudly instead of producing a plausible repository.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use surrealdb::types::{Bytes, RecordId, SurrealValue};
use surrealfs_content::tree::DirNode;
use surrealfs_types::state::{kv_digest, root_digest, KvMap};
use surrealfs_types::{
    ChunkDigest, RepositoryId, SfsError, StateNodeId, HASH_VERSION, ROOT_FORMAT_VERSION,
};

use crate::{bodies, map_db_err, rid_repo, rid_state_node, Store};

const MAGIC: &[u8] = b"SURREALFS-ARCHIVE-1\n";

/// Record kinds. Adding one is backward compatible: an older reader stops at the first kind it
/// does not know rather than guessing, which is why the kind byte leads every record.
mod kind {
    pub const HEADER: u8 = 1;
    pub const CHUNK: u8 = 2;
    pub const TREE_NODE: u8 = 3;
    pub const KV_NODE: u8 = 4;
    pub const STATE_ROOT: u8 = 5;
    pub const COMMIT: u8 = 6;
    pub const BRANCH: u8 = 7;
    pub const SNAPSHOT: u8 = 8;
    pub const RUN: u8 = 9;
    pub const SPAN: u8 = 10;
    pub const TOOL_CALL: u8 = 11;
    pub const MUTATION: u8 = 12;
    pub const END: u8 = 0;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveHeader {
    pub repository: String,
    pub hash_version: u32,
    pub root_format_version: u32,
    pub exported_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct ArchiveStats {
    pub chunks: usize,
    pub tree_nodes: usize,
    pub commits: usize,
    pub branches: usize,
    pub snapshots: usize,
    pub tool_calls: usize,
    pub bytes: u64,
}

/// Writes archive records into any sink.
pub struct ArchiveWriter<W: std::io::Write> {
    out: W,
}

impl<W: std::io::Write> ArchiveWriter<W> {
    pub fn new(mut out: W) -> std::io::Result<Self> {
        out.write_all(MAGIC)?;
        Ok(ArchiveWriter { out })
    }

    fn record(&mut self, kind: u8, json: &str, blob: &[u8]) -> std::io::Result<()> {
        self.out.write_all(&[kind])?;
        self.out.write_all(&(json.len() as u64).to_le_bytes())?;
        self.out.write_all(json.as_bytes())?;
        self.out.write_all(&(blob.len() as u64).to_le_bytes())?;
        self.out.write_all(blob)?;
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<W> {
        self.record(kind::END, "{}", &[])?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Reads archive records back.
pub struct ArchiveReader<R: std::io::Read> {
    input: R,
}

impl<R: std::io::Read> ArchiveReader<R> {
    pub fn new(mut input: R) -> Result<Self, SfsError> {
        let mut magic = vec![0u8; MAGIC.len()];
        input.read_exact(&mut magic)?;
        if magic != MAGIC {
            return Err(SfsError::Corruption(
                "not a SurrealFS archive, or a version this build cannot read".into(),
            ));
        }
        Ok(ArchiveReader { input })
    }

    /// Next record as `(kind, json, blob)`, or `None` at the end marker.
    fn next(&mut self) -> Result<Option<(u8, String, Vec<u8>)>, SfsError> {
        let mut kind = [0u8; 1];
        if self.input.read_exact(&mut kind).is_err() {
            // A stream that stops without an end marker is truncated, not finished.
            return Err(SfsError::Corruption(
                "archive ended without a terminator".into(),
            ));
        }
        if kind[0] == kind::END {
            return Ok(None);
        }
        let json = self.read_framed()?;
        let blob = self.read_framed()?;
        let json = String::from_utf8(json)
            .map_err(|e| SfsError::Corruption(format!("archive record is not UTF-8: {e}")))?;
        Ok(Some((kind[0], json, blob)))
    }

    fn read_framed(&mut self) -> Result<Vec<u8>, SfsError> {
        let mut len = [0u8; 8];
        self.input.read_exact(&mut len)?;
        let len = u64::from_le_bytes(len) as usize;
        let mut buf = vec![0u8; len];
        self.input.read_exact(&mut buf)?;
        Ok(buf)
    }
}

// Row shapes read out of the store for export, and written back on import. They mirror the
// schema rather than the domain types because an archive must reproduce stored state exactly.

#[derive(SurrealValue, Serialize, Deserialize)]
struct ChunkRow {
    digest: String,
    length: i64,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct NodeRow {
    digest: String,
    kind: String,
    json: String,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct RootRow {
    digest: String,
    ns: String,
    kv: String,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct CommitRow {
    digest: String,
    kind: String,
    first_parent: Option<String>,
    request_id: String,
    message: Option<String>,
    mutation_count: i64,
    state_root_digest: String,
    domain_sequence: i64,
    author_span: Option<String>,
    /// When the commit was published, so time references survive a round trip.
    ///
    /// Optional because archives written before this field existed do not carry it; those
    /// import with the current time, exactly as they did before.
    #[serde(default)]
    committed_at: Option<String>,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct BranchRow {
    name: String,
    head: String,
    message: Option<String>,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct SnapshotRow {
    name: String,
    commit: String,
    message: Option<String>,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct RunRow {
    key: String,
    status: String,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct SpanRow {
    key: String,
    run: String,
    kind: String,
    name: String,
    status: String,
    capture_quality: String,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct ToolCallRow {
    key: String,
    run: String,
    tool_name: String,
    input_preview: Option<String>,
    output_preview: Option<String>,
    error_message: Option<String>,
    duration_ms: Option<i64>,
}

#[derive(SurrealValue, Serialize, Deserialize)]
struct MutationRow {
    commit: String,
    ordinal: i64,
    kind: String,
    path: String,
    domain_sequence: i64,
    json: String,
}

/// Strip the `repo/` prefix a record id carries, leaving the bare key.
fn bare_key(id: &RecordId) -> Result<String, SfsError> {
    let surrealdb::types::RecordIdKey::String(key) = &id.key else {
        return Err(SfsError::Corruption(format!(
            "unexpected record id shape in {}",
            id.table
        )));
    };
    Ok(key
        .rsplit_once('/')
        .map(|(_, k)| k)
        .unwrap_or(key)
        .to_string())
}

impl Store {
    /// Write a repository to an archive.
    pub async fn export_archive<W: std::io::Write>(
        &self,
        repo: &RepositoryId,
        out: W,
    ) -> Result<ArchiveStats, SfsError> {
        let mut writer = ArchiveWriter::new(out).map_err(SfsError::Io)?;
        let mut stats = ArchiveStats::default();

        let header = ArchiveHeader {
            repository: repo.as_str().to_string(),
            hash_version: HASH_VERSION,
            root_format_version: ROOT_FORMAT_VERSION,
            exported_at: String::new(),
        };
        writer
            .record(kind::HEADER, &json(&header)?, &[])
            .map_err(SfsError::Io)?;

        // Content first: an importer can then verify every reference it meets afterwards.
        #[derive(SurrealValue)]
        struct ChunkExport {
            digest: String,
            length: i64,
            storage_kind: String,
            inline_bytes: Bytes,
        }
        let chunks: Vec<ChunkExport> = self
            .db()
            .query(
                "SELECT digest, length, storage_kind, inline_bytes FROM chunk \
                 WHERE repository = $repo",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read chunks", e))?
            .take(0)
            .map_err(|e| map_db_err("decode chunks", e))?;
        for chunk in chunks {
            let stored = chunk.inline_bytes.into_inner().to_vec();

            // Archives hold plaintext. Import verifies every body against its BLAKE3 digest, and
            // that digest is over the plaintext, so writing ciphertext here would produce an
            // archive that can never be imported anywhere — including back into this repository.
            //
            // The consequence is worth stating plainly rather than discovering: an archive is as
            // sensitive as the data in it, and exporting an encrypted repository needs the key.
            let bytes = if chunk.storage_kind == "ENCRYPTED" {
                let digest = ChunkDigest::parse(&chunk.digest)?;
                let cipher = self.cipher().ok_or_else(|| {
                    SfsError::Encryption(
                        "this repository is encrypted; exporting it needs the key".into(),
                    )
                })?;
                cipher.open(&digest, &stored)?
            } else {
                stored
            };
            let row = ChunkRow {
                digest: chunk.digest,
                length: chunk.length,
            };
            stats.bytes += bytes.len() as u64;
            stats.chunks += 1;
            writer
                .record(kind::CHUNK, &json(&row)?, &bytes)
                .map_err(SfsError::Io)?;
        }

        #[derive(SurrealValue)]
        struct NodeExport {
            digest: String,
            kind: String,
            body: NodeBody,
        }
        #[derive(SurrealValue)]
        struct NodeBody {
            json: String,
        }
        let nodes: Vec<NodeExport> = self
            .db()
            .query("SELECT digest, kind, body FROM state_node WHERE repository = $repo")
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read state nodes", e))?
            .take(0)
            .map_err(|e| map_db_err("decode state nodes", e))?;
        for node in nodes {
            let record_kind = if node.kind == "KV" {
                kind::KV_NODE
            } else {
                stats.tree_nodes += 1;
                kind::TREE_NODE
            };
            let row = NodeRow {
                digest: node.digest,
                kind: node.kind,
                json: node.body.json,
            };
            writer
                .record(record_kind, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        #[derive(SurrealValue)]
        struct RootExport {
            digest: String,
            ns: String,
            kv: String,
        }
        let roots: Vec<RootExport> = self
            .db()
            .query(
                "SELECT digest, namespace_node.digest AS ns, kv_node.digest AS kv \
                 FROM state_root WHERE repository = $repo",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read state roots", e))?
            .take(0)
            .map_err(|e| map_db_err("decode state roots", e))?;
        for root in roots {
            let row = RootRow {
                digest: root.digest,
                ns: root.ns,
                kv: root.kv,
            };
            writer
                .record(kind::STATE_ROOT, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        #[derive(SurrealValue)]
        struct CommitExport {
            id: RecordId,
            kind: String,
            first_parent: Option<RecordId>,
            request_id: String,
            message: Option<String>,
            mutation_count: i64,
            state_root_digest: String,
            domain_sequence: i64,
            author_span: Option<RecordId>,
            committed_at: String,
        }
        let commits: Vec<CommitExport> = self
            .db()
            .query(
                "SELECT id, kind, first_parent, request_id, message, mutation_count, \
                 state_root_digest, domain_sequence, author_span, \
                 type::string(committed_at) AS committed_at FROM commit \
                 WHERE repository = $repo ORDER BY domain_sequence ASC",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read commits", e))?
            .take(0)
            .map_err(|e| map_db_err("decode commits", e))?;
        for commit in commits {
            let row = CommitRow {
                digest: bare_key(&commit.id)?,
                kind: commit.kind,
                first_parent: commit.first_parent.as_ref().map(bare_key).transpose()?,
                request_id: commit.request_id,
                message: commit.message,
                mutation_count: commit.mutation_count,
                state_root_digest: commit.state_root_digest,
                domain_sequence: commit.domain_sequence,
                author_span: commit.author_span.as_ref().map(bare_key).transpose()?,
                committed_at: Some(commit.committed_at),
            };
            stats.commits += 1;
            writer
                .record(kind::COMMIT, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        for branch in self.branches(repo).await? {
            let row = BranchRow {
                name: branch.name,
                head: branch.head.to_string(),
                message: branch.message,
            };
            stats.branches += 1;
            writer
                .record(kind::BRANCH, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }
        for snapshot in self.snapshots(repo).await? {
            let row = SnapshotRow {
                name: snapshot.name,
                commit: snapshot.commit.to_string(),
                message: snapshot.message,
            };
            stats.snapshots += 1;
            writer
                .record(kind::SNAPSHOT, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        // Provenance. Without it the archive would carry state but lose the record of what
        // produced it, which is most of the point of this system.
        #[derive(SurrealValue)]
        struct RunExport {
            id: RecordId,
            status: String,
        }
        let runs: Vec<RunExport> = self
            .db()
            .query("SELECT id, status FROM run WHERE repository = $repo")
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read runs", e))?
            .take(0)
            .map_err(|e| map_db_err("decode runs", e))?;
        for run in runs {
            let row = RunRow {
                key: bare_key(&run.id)?,
                status: run.status,
            };
            writer
                .record(kind::RUN, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        #[derive(SurrealValue)]
        struct SpanExport {
            id: RecordId,
            run: RecordId,
            kind: String,
            name: String,
            status: String,
            capture_quality: String,
        }
        let spans: Vec<SpanExport> = self
            .db()
            .query(
                "SELECT id, run, kind, name, status, capture_quality FROM span \
                 WHERE repository = $repo",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read spans", e))?
            .take(0)
            .map_err(|e| map_db_err("decode spans", e))?;
        for span in spans {
            let row = SpanRow {
                key: bare_key(&span.id)?,
                run: bare_key(&span.run)?,
                kind: span.kind,
                name: span.name,
                status: span.status,
                capture_quality: span.capture_quality,
            };
            writer
                .record(kind::SPAN, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        #[derive(SurrealValue)]
        struct ToolExport {
            id: RecordId,
            run: RecordId,
            tool_name: String,
            input_preview: Option<String>,
            output_preview: Option<String>,
            error_message: Option<String>,
            duration_ms: Option<i64>,
        }
        let calls: Vec<ToolExport> = self
            .db()
            .query(
                "SELECT id, run, tool_name, input_preview, output_preview, error_message, \
                 duration_ms FROM tool_call WHERE repository = $repo",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read tool calls", e))?
            .take(0)
            .map_err(|e| map_db_err("decode tool calls", e))?;
        for call in calls {
            let row = ToolCallRow {
                key: bare_key(&call.id)?,
                run: bare_key(&call.run)?,
                tool_name: call.tool_name,
                input_preview: call.input_preview,
                output_preview: call.output_preview,
                error_message: call.error_message,
                duration_ms: call.duration_ms,
            };
            stats.tool_calls += 1;
            writer
                .record(kind::TOOL_CALL, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        #[derive(SurrealValue)]
        struct MutationExport {
            commit: RecordId,
            ordinal: i64,
            kind: String,
            path: String,
            domain_sequence: i64,
            body: MutationBody,
        }
        #[derive(SurrealValue)]
        struct MutationBody {
            json: String,
        }
        let mutations: Vec<MutationExport> = self
            .db()
            .query(
                "SELECT commit, ordinal, kind, path, domain_sequence, body FROM commit_mutation \
                 WHERE repository = $repo ORDER BY domain_sequence ASC, ordinal ASC",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read mutations", e))?
            .take(0)
            .map_err(|e| map_db_err("decode mutations", e))?;
        for mutation in mutations {
            let row = MutationRow {
                commit: bare_key(&mutation.commit)?,
                ordinal: mutation.ordinal,
                kind: mutation.kind,
                path: mutation.path,
                domain_sequence: mutation.domain_sequence,
                json: mutation.body.json,
            };
            writer
                .record(kind::MUTATION, &json(&row)?, &[])
                .map_err(SfsError::Io)?;
        }

        writer.finish().map_err(SfsError::Io)?;
        Ok(stats)
    }
}

fn json<T: Serialize>(value: &T) -> Result<String, SfsError> {
    serde_json::to_string(value).map_err(|e| SfsError::Storage(format!("encode record: {e}")))
}

fn parse<T: for<'a> Deserialize<'a>>(text: &str) -> Result<T, SfsError> {
    serde_json::from_str(text)
        .map_err(|e| SfsError::Corruption(format!("decode archive record: {e}")))
}

/// Everything an archive carried, verified and ready to insert.
#[derive(Default)]
pub struct ArchiveContents {
    pub header: Option<ArchiveHeader>,
    pub chunks: Vec<(ChunkDigest, Vec<u8>)>,
    pub nodes: Vec<(StateNodeId, String, String)>,
    pub roots: Vec<(String, String, String)>,
    pub commits: Vec<CommitRecord>,
    pub branches: Vec<(String, String, Option<String>)>,
    pub snapshots: Vec<(String, String, Option<String>)>,
    pub runs: Vec<(String, String)>,
    pub spans: Vec<SpanRecord>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub mutations: Vec<MutationRecord>,
}

pub struct CommitRecord {
    pub digest: String,
    pub kind: String,
    pub first_parent: Option<String>,
    pub request_id: String,
    pub message: Option<String>,
    pub mutation_count: i64,
    pub state_root_digest: String,
    pub domain_sequence: i64,
    pub author_span: Option<String>,
    /// Original publication time. `None` for archives written before this was carried.
    pub committed_at: Option<String>,
}

pub struct SpanRecord {
    pub key: String,
    pub run: String,
    pub kind: String,
    pub name: String,
    pub status: String,
    pub capture_quality: String,
}

pub struct ToolCallRecord {
    pub key: String,
    pub run: String,
    pub tool_name: String,
    pub input_preview: Option<String>,
    pub output_preview: Option<String>,
    pub error_message: Option<String>,
    pub duration_ms: Option<i64>,
}

pub struct MutationRecord {
    pub commit: String,
    pub ordinal: i64,
    pub kind: String,
    pub path: String,
    pub domain_sequence: i64,
    pub json: String,
}

/// Read and verify an archive without touching any store.
///
/// Verification happens here rather than during insertion so a bad archive is rejected before
/// it can put anything in a repository: chunk bytes must hash to the digest they arrived
/// under, tree nodes must hash to theirs, and every state root must re-derive from the two
/// node digests it names.
pub fn read_archive<R: std::io::Read>(input: R) -> Result<ArchiveContents, SfsError> {
    let mut reader = ArchiveReader::new(input)?;
    let mut out = ArchiveContents::default();
    let mut node_digests: BTreeMap<String, String> = BTreeMap::new();

    while let Some((record_kind, text, blob)) = reader.next()? {
        match record_kind {
            kind::HEADER => {
                let header: ArchiveHeader = parse(&text)?;
                if header.hash_version != HASH_VERSION
                    || header.root_format_version != ROOT_FORMAT_VERSION
                {
                    return Err(SfsError::Migration(format!(
                        "archive uses hash version {} and root format {}; this build reads {HASH_VERSION} and {ROOT_FORMAT_VERSION}",
                        header.hash_version, header.root_format_version
                    )));
                }
                out.header = Some(header);
            }
            kind::CHUNK => {
                let row: ChunkRow = parse(&text)?;
                let digest = ChunkDigest::parse(&row.digest)?;
                // The archive's own claim about this content is checked against the content.
                surrealfs_content::verify_chunk(&digest, &blob)?;
                out.chunks.push((digest, blob));
            }
            kind::TREE_NODE => {
                let row: NodeRow = parse(&text)?;
                let id = StateNodeId::parse(&row.digest)?;
                // decode_dir re-derives the digest and rejects a mismatch.
                let _: DirNode = bodies::decode_dir(&id, &row.json)?;
                node_digests.insert(row.digest.clone(), "DIR".into());
                out.nodes.push((id, row.kind, row.json));
            }
            kind::KV_NODE => {
                let row: NodeRow = parse(&text)?;
                let id = StateNodeId::parse(&row.digest)?;
                let _: KvMap = bodies::decode_kv(&id, &row.json)?;
                node_digests.insert(row.digest.clone(), "KV".into());
                out.nodes.push((id, row.kind, row.json));
            }
            kind::STATE_ROOT => {
                let row: RootRow = parse(&text)?;
                let ns = StateNodeId::parse(&row.ns)?;
                let kv = StateNodeId::parse(&row.kv)?;
                let recomputed = root_digest(&ns, &kv);
                if recomputed.as_str() != row.digest {
                    return Err(SfsError::Corruption(format!(
                        "archived root {} does not re-derive from its nodes (got {recomputed})",
                        row.digest
                    )));
                }
                out.roots.push((row.digest, row.ns, row.kv));
            }
            kind::COMMIT => {
                let row: CommitRow = parse(&text)?;
                out.commits.push(CommitRecord {
                    digest: row.digest,
                    kind: row.kind,
                    first_parent: row.first_parent,
                    request_id: row.request_id,
                    message: row.message,
                    mutation_count: row.mutation_count,
                    state_root_digest: row.state_root_digest,
                    domain_sequence: row.domain_sequence,
                    author_span: row.author_span,
                    committed_at: row.committed_at,
                });
            }
            kind::BRANCH => {
                let row: BranchRow = parse(&text)?;
                out.branches.push((row.name, row.head, row.message));
            }
            kind::SNAPSHOT => {
                let row: SnapshotRow = parse(&text)?;
                out.snapshots.push((row.name, row.commit, row.message));
            }
            kind::RUN => {
                let row: RunRow = parse(&text)?;
                out.runs.push((row.key, row.status));
            }
            kind::SPAN => {
                let row: SpanRow = parse(&text)?;
                out.spans.push(SpanRecord {
                    key: row.key,
                    run: row.run,
                    kind: row.kind,
                    name: row.name,
                    status: row.status,
                    capture_quality: row.capture_quality,
                });
            }
            kind::TOOL_CALL => {
                let row: ToolCallRow = parse(&text)?;
                out.tool_calls.push(ToolCallRecord {
                    key: row.key,
                    run: row.run,
                    tool_name: row.tool_name,
                    input_preview: row.input_preview,
                    output_preview: row.output_preview,
                    error_message: row.error_message,
                    duration_ms: row.duration_ms,
                });
            }
            kind::MUTATION => {
                let row: MutationRow = parse(&text)?;
                out.mutations.push(MutationRecord {
                    commit: row.commit,
                    ordinal: row.ordinal,
                    kind: row.kind,
                    path: row.path,
                    domain_sequence: row.domain_sequence,
                    json: row.json,
                });
            }
            other => {
                return Err(SfsError::Migration(format!(
                    "archive contains record kind {other}, which this build does not understand"
                )))
            }
        }
    }

    if out.header.is_none() {
        return Err(SfsError::Corruption("archive has no header".into()));
    }
    // Every root's node digests must have arrived; a root pointing at absent content would
    // import into a repository that cannot be read.
    for (digest, ns, kv) in &out.roots {
        for needed in [ns, kv] {
            if !node_digests.contains_key(needed) {
                return Err(SfsError::Corruption(format!(
                    "root {digest} references node {needed}, which the archive does not contain"
                )));
            }
        }
    }
    Ok(out)
}

impl Store {
    /// Insert a verified archive into this store under `repo`.
    pub async fn import_archive(
        &self,
        repo: &RepositoryId,
        contents: ArchiveContents,
    ) -> Result<ArchiveStats, SfsError> {
        let mut stats = ArchiveStats::default();
        self.ensure_repository(repo).await?;

        for (digest, bytes) in &contents.chunks {
            stats.bytes += bytes.len() as u64;
            stats.chunks += 1;
            self.stage_chunks(repo, std::slice::from_ref(&(digest.clone(), bytes.clone())))
                .await?;
        }

        for (id, node_kind, body) in &contents.nodes {
            if node_kind == "DIR" {
                stats.tree_nodes += 1;
            }
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, root_format_version: $rv, \
                     kind: $kind, digest: $digest, body: { json: $json }, created_at: time::now() }",
                )
                .bind(("rid", rid_state_node(repo, id)))
                .bind(("repo", rid_repo(repo)))
                .bind(("rv", ROOT_FORMAT_VERSION as i64))
                .bind(("kind", node_kind.clone()))
                .bind(("digest", id.as_str().to_string()))
                .bind(("json", body.clone()))
                .await
                .map_err(|e| map_db_err("import node", e))?
                .check()
                .map_err(|e| map_db_err("import node", e))?;
        }

        for (digest, ns, kv) in &contents.roots {
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, root_format_version: $rv, \
                     namespace_node: $ns, kv_node: $kv, digest: $digest, created_at: time::now() }",
                )
                .bind((
                    "rid",
                    RecordId::new("state_root", format!("{repo}/{digest}")),
                ))
                .bind(("repo", rid_repo(repo)))
                .bind(("rv", ROOT_FORMAT_VERSION as i64))
                .bind(("ns", RecordId::new("state_node", format!("{repo}/{ns}"))))
                .bind(("kv", RecordId::new("state_node", format!("{repo}/{kv}"))))
                .bind(("digest", digest.clone()))
                .await
                .map_err(|e| map_db_err("import root", e))?
                .check()
                .map_err(|e| map_db_err("import root", e))?;
        }

        // Provenance before commits, so a commit's author span already exists when it lands.
        for (key, status) in &contents.runs {
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, agent: $agent, actor: $actor, \
                     status: $status, started_at: time::now(), finished_at: NONE }",
                )
                .bind(("rid", RecordId::new("run", format!("{repo}/{key}"))))
                .bind(("repo", rid_repo(repo)))
                .bind(("agent", RecordId::new("agent", format!("{repo}/default"))))
                .bind(("actor", crate::rid_principal()))
                .bind(("status", status.clone()))
                .await
                .map_err(|e| map_db_err("import run", e))?
                .check()
                .map_err(|e| map_db_err("import run", e))?;
        }
        for span in &contents.spans {
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, run: $run, parent_span: NONE, \
                     kind: $kind, name: $name, status: $status, \
                     capture_quality: 'IMPORTED', started_at: time::now(), finished_at: NONE }",
                )
                .bind(("rid", RecordId::new("span", format!("{repo}/{}", span.key))))
                .bind(("repo", rid_repo(repo)))
                .bind(("run", RecordId::new("run", format!("{repo}/{}", span.run))))
                .bind(("kind", span.kind.clone()))
                .bind(("name", span.name.clone()))
                .bind(("status", span.status.clone()))
                .await
                .map_err(|e| map_db_err("import span", e))?
                .check()
                .map_err(|e| map_db_err("import span", e))?;
        }
        for call in &contents.tool_calls {
            stats.tool_calls += 1;
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, run: $run, span: $span, \
                     tool_name: $name, input_preview: $input, output_preview: $output, \
                     error_message: $error, duration_ms: $duration, started_at: time::now(), \
                     finished_at: NONE }",
                )
                .bind((
                    "rid",
                    RecordId::new("tool_call", format!("{repo}/{}", call.key)),
                ))
                .bind(("repo", rid_repo(repo)))
                .bind(("run", RecordId::new("run", format!("{repo}/{}", call.run))))
                .bind((
                    "span",
                    RecordId::new("span", format!("{repo}/{}", call.key)),
                ))
                .bind(("name", call.tool_name.clone()))
                .bind(("input", call.input_preview.clone()))
                .bind(("output", call.output_preview.clone()))
                .bind(("error", call.error_message.clone()))
                .bind(("duration", call.duration_ms))
                .await
                .map_err(|e| map_db_err("import tool call", e))?
                .check()
                .map_err(|e| map_db_err("import tool call", e))?;
        }

        for commit in &contents.commits {
            stats.commits += 1;
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, kind: $kind, \
                     first_parent: $parent, author_principal: $principal, author_span: $span, \
                     request_id: $request, message: $message, mutation_count: $mcount, \
                     state_root: $root, state_root_digest: $root_digest, hash_version: $hv, \
                     domain_sequence: $seq, \
                     committed_at: type::datetime($committed_at) }",
                )
                .bind((
                    "committed_at",
                    // Preserve the original publication time. Stamping `time::now()` here, as
                    // this did, relocated every commit to the moment of import and quietly
                    // invalidated every time reference built on the history.
                    commit.committed_at.clone().unwrap_or_else(|| {
                        surrealfs_types::time::format_rfc3339(std::time::SystemTime::now())
                    }),
                ))
                .bind((
                    "rid",
                    RecordId::new("commit", format!("{repo}/{}", commit.digest)),
                ))
                .bind(("repo", rid_repo(repo)))
                .bind(("kind", commit.kind.clone()))
                .bind((
                    "parent",
                    commit
                        .first_parent
                        .as_ref()
                        .map(|p| RecordId::new("commit", format!("{repo}/{p}"))),
                ))
                .bind(("principal", crate::rid_principal()))
                .bind((
                    "span",
                    commit
                        .author_span
                        .as_ref()
                        .map(|s| RecordId::new("span", format!("{repo}/{s}"))),
                ))
                .bind(("request", commit.request_id.clone()))
                .bind(("message", commit.message.clone()))
                .bind(("mcount", commit.mutation_count))
                .bind((
                    "root",
                    RecordId::new("state_root", format!("{repo}/{}", commit.state_root_digest)),
                ))
                .bind(("root_digest", commit.state_root_digest.clone()))
                .bind(("hv", HASH_VERSION as i64))
                .bind(("seq", commit.domain_sequence))
                .await
                .map_err(|e| map_db_err("import commit", e))?
                .check()
                .map_err(|e| map_db_err("import commit", e))?;
        }

        for mutation in &contents.mutations {
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, commit: $commit, ordinal: $ordinal, \
                     kind: $kind, path: $path, domain_sequence: $seq, body: { json: $json } }",
                )
                .bind((
                    "rid",
                    RecordId::new(
                        "commit_mutation",
                        format!("{repo}/{}/{}", mutation.commit, mutation.ordinal),
                    ),
                ))
                .bind(("repo", rid_repo(repo)))
                .bind((
                    "commit",
                    RecordId::new("commit", format!("{repo}/{}", mutation.commit)),
                ))
                .bind(("ordinal", mutation.ordinal))
                .bind(("kind", mutation.kind.clone()))
                .bind(("path", mutation.path.clone()))
                .bind(("seq", mutation.domain_sequence))
                .bind(("json", mutation.json.clone()))
                .await
                .map_err(|e| map_db_err("import mutation", e))?
                .check()
                .map_err(|e| map_db_err("import mutation", e))?;
        }

        for (name, head, message) in &contents.branches {
            stats.branches += 1;
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, name: $name, head: $head, \
                     message: $message, created_at: time::now(), updated_at: time::now() }",
                )
                .bind(("rid", RecordId::new("branch", format!("{repo}/{name}"))))
                .bind(("repo", rid_repo(repo)))
                .bind(("name", name.clone()))
                .bind(("head", RecordId::new("commit", format!("{repo}/{head}"))))
                .bind(("message", message.clone()))
                .await
                .map_err(|e| map_db_err("import branch", e))?
                .check()
                .map_err(|e| map_db_err("import branch", e))?;
        }
        for (name, commit, message) in &contents.snapshots {
            stats.snapshots += 1;
            self.db()
                .query(
                    "UPSERT $rid CONTENT { repository: $repo, name: $name, commit: $commit, \
                     created_by: $principal, message: $message, created_at: time::now() }",
                )
                .bind(("rid", RecordId::new("snapshot", format!("{repo}/{name}"))))
                .bind(("repo", rid_repo(repo)))
                .bind(("name", name.clone()))
                .bind((
                    "commit",
                    RecordId::new("commit", format!("{repo}/{commit}")),
                ))
                .bind(("principal", crate::rid_principal()))
                .bind(("message", message.clone()))
                .await
                .map_err(|e| map_db_err("import snapshot", e))?
                .check()
                .map_err(|e| map_db_err("import snapshot", e))?;
        }

        // A final independent check: every imported root must still re-derive from what is
        // now in the store, not merely from what the archive claimed.
        for (digest, _, _) in &contents.roots {
            let root = surrealfs_types::StateRootId::parse(digest)?;
            let (ns, kv) = self.load_root(repo, &root).await?;
            if root_digest(&ns, &kv_digest(&kv)) != root {
                return Err(SfsError::Corruption(format!(
                    "imported root {digest} does not verify against stored state"
                )));
            }
        }
        Ok(stats)
    }
}
