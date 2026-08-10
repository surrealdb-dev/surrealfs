//! Read paths (roots, chunks, timeline) and execution records (runs, spans, tool calls).
//!
//! Historical reads resolve a commit to its immutable state root and walk the namespace tree
//! from there; they never scan ancestry. Node bodies are re-verified against their digests.

use surrealdb::types::{Bytes, RecordId, SurrealValue};
use surrealfs_content::tree::DirNode;
use surrealfs_types::state::{kv_digest, root_digest, KvMap, Mutation};
use surrealfs_types::{ChunkDigest, CommitId, RepositoryId, SfsError, StateNodeId, StateRootId};

use crate::publish::commit_id_of_rid;
use crate::{
    bodies, map_db_err, rid_chunk, rid_commit, rid_principal, rid_repo, rid_state_node,
    rid_state_root, Store,
};

#[derive(SurrealValue)]
struct RootNodesRow {
    ns: String,
    kv: String,
}

/// A chunk body plus the marker saying whether it is sealed.
#[derive(SurrealValue)]
struct ChunkBodyRow {
    storage_kind: String,
    inline_bytes: Bytes,
}

#[derive(SurrealValue)]
struct CommitRow {
    id: RecordId,
    request_id: String,
    message: Option<String>,
    mutation_count: i64,
    state_root_digest: String,
    domain_sequence: i64,
    committed_at: String,
}

/// One timeline entry.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub commit: CommitId,
    pub request_id: String,
    pub message: Option<String>,
    pub mutation_count: u64,
    pub state_root: StateRootId,
    pub domain_sequence: u64,
    pub committed_at: String,
}

#[derive(SurrealValue)]
struct MutationRow {
    body: MutationBody,
}

#[derive(SurrealValue)]
struct MutationBody {
    json: String,
}

#[derive(SurrealValue)]
struct ToolCallRow {
    tool_name: String,
    status: String,
    input_preview: Option<String>,
    output_preview: Option<String>,
    error_message: Option<String>,
    started_at: String,
}

/// One tool-call record as surfaced by `tools.recent()`.
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub tool_name: String,
    pub status: String,
    pub input_preview: Option<String>,
    pub output_preview: Option<String>,
    pub error_message: Option<String>,
    pub started_at: String,
}

impl Store {
    /// Resolve a state root to its namespace tree root and KV map.
    ///
    /// This reads two rows regardless of how large the tree is; the namespace itself is walked
    /// lazily through [`Store::dir_nodes`].
    pub async fn load_root(
        &self,
        repo: &RepositoryId,
        root: &StateRootId,
    ) -> Result<(StateNodeId, KvMap), SfsError> {
        let row: Option<RootNodesRow> = self
            .db()
            .query("SELECT namespace_node.digest AS ns, kv_node.body.json AS kv FROM ONLY $rid")
            .bind(("rid", rid_state_root(repo, root)))
            .await
            .map_err(|e| map_db_err("read state root", e))?
            .take(0)
            .map_err(|e| map_db_err("decode state root", e))?;
        let row = row.ok_or_else(|| SfsError::NotFound(format!("state root {root}")))?;
        let namespace_root = StateNodeId::parse(&row.ns)?;
        let kv = bodies::parse_kv(&row.kv)?;

        // Re-derive the root from what was loaded. A mismatch means storage disagrees with its
        // own content addressing, which is corruption rather than a stale read.
        let recomputed = root_digest(&namespace_root, &kv_digest(&kv));
        if &recomputed != root {
            return Err(SfsError::Corruption(format!(
                "state root {root} recomputed as {recomputed}"
            )));
        }
        Ok((namespace_root, kv))
    }

    /// Batch-load directory nodes by digest, verifying each body against its id.
    /// Batch-load directory nodes by digest.
    ///
    /// Resident nodes are answered without touching the store; the rest are fetched in one
    /// query rather than one per id. Bodies are verified against the digest they were stored
    /// under before being cached, so a corrupt row can never enter the cache.
    pub async fn dir_nodes(
        &self,
        repo: &RepositoryId,
        ids: &[StateNodeId],
    ) -> Result<Vec<(StateNodeId, DirNode)>, SfsError> {
        let mut out: Vec<(StateNodeId, DirNode)> = Vec::with_capacity(ids.len());
        let mut wanted: Vec<StateNodeId> = Vec::new();
        for id in ids {
            match self.resident.get(id) {
                Some(node) => out.push((id.clone(), node)),
                None => wanted.push(id.clone()),
            }
        }
        if wanted.is_empty() {
            return Ok(out);
        }

        #[derive(SurrealValue)]
        struct Row {
            digest: String,
            json: String,
        }
        let rids: Vec<RecordId> = wanted.iter().map(|id| rid_state_node(repo, id)).collect();
        let rows: Vec<Row> = self
            .db()
            .query("SELECT digest, body.json AS json FROM $ids")
            .bind(("ids", rids))
            .await
            .map_err(|e| map_db_err("read tree nodes", e))?
            .take(0)
            .map_err(|e| map_db_err("decode tree nodes", e))?;

        let mut fetched: std::collections::HashMap<String, String> =
            rows.into_iter().map(|r| (r.digest, r.json)).collect();
        for id in wanted {
            let json = fetched
                .remove(id.as_str())
                .ok_or_else(|| SfsError::NotFound(format!("tree node {id}")))?;
            let node = bodies::decode_dir(&id, &json)?;
            self.resident.insert(id.clone(), node.clone());
            out.push((id, node));
        }
        Ok(out)
    }

    /// Hit and miss counts for the resident node cache.
    pub fn cache_stats(&self) -> crate::CacheStats {
        self.resident.stats()
    }

    /// Drop the resident tier. Safe at any moment — it is a cache, not state.
    pub fn clear_resident_cache(&self) {
        self.resident.clear();
    }

    /// State root of an arbitrary commit (for historical reads).
    pub async fn root_of_commit(
        &self,
        repo: &RepositoryId,
        commit: &CommitId,
    ) -> Result<StateRootId, SfsError> {
        #[derive(SurrealValue)]
        struct Row {
            state_root_digest: String,
        }
        let row: Option<Row> = self
            .db()
            .query("SELECT state_root_digest FROM ONLY $rid")
            .bind(("rid", rid_commit(repo, commit)))
            .await
            .map_err(|e| map_db_err("read commit", e))?
            .take(0)
            .map_err(|e| map_db_err("decode commit", e))?;
        let row = row.ok_or_else(|| SfsError::NotFound(format!("commit {commit}")))?;
        StateRootId::parse(&row.state_root_digest)
    }

    /// Fetch one content chunk, decrypting it if it was stored sealed.
    ///
    /// Callers verify the digest themselves against the plaintext this returns, so encryption is
    /// invisible to them. Whether a body is sealed is read from the row rather than assumed from
    /// the store's configuration: that way a repository holding a mix — which only a partial
    /// migration could produce — reads correctly instead of failing on the older half.
    pub async fn fetch_chunk(
        &self,
        repo: &RepositoryId,
        digest: &ChunkDigest,
    ) -> Result<Vec<u8>, SfsError> {
        let row: Option<ChunkBodyRow> = self
            .db()
            .query("SELECT storage_kind, inline_bytes FROM ONLY $rid")
            .bind(("rid", rid_chunk(repo, digest)))
            .await
            .map_err(|e| map_db_err("read chunk", e))?
            .take(0)
            .map_err(|e| map_db_err("decode chunk", e))?;
        let row = row.ok_or_else(|| SfsError::NotFound(format!("chunk {digest}")))?;
        let bytes = row.inline_bytes.into_inner().to_vec();

        if row.storage_kind != "ENCRYPTED" {
            return Ok(bytes);
        }
        let cipher = self.cipher().ok_or_else(|| {
            SfsError::Encryption(
                "this repository stores encrypted content; supply --key or SURREALFS_KEY".into(),
            )
        })?;
        cipher.open(digest, &bytes)
    }

    /// Recent commits, newest first.
    pub async fn timeline(
        &self,
        repo: &RepositoryId,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, SfsError> {
        let rows: Vec<CommitRow> = self
            .db()
            .query(
                "SELECT id, request_id, message, mutation_count, state_root_digest, \
                 domain_sequence, type::string(committed_at) AS committed_at FROM commit \
                 WHERE repository = $repo ORDER BY domain_sequence DESC LIMIT $limit",
            )
            .bind(("repo", rid_repo(repo)))
            .bind(("limit", limit as i64))
            .await
            .map_err(|e| map_db_err("read timeline", e))?
            .take(0)
            .map_err(|e| map_db_err("decode timeline", e))?;
        rows.into_iter()
            .map(|row| {
                Ok(CommitInfo {
                    commit: commit_id_of_rid(&row.id)?,
                    request_id: row.request_id,
                    message: row.message,
                    mutation_count: row.mutation_count as u64,
                    state_root: StateRootId::parse(&row.state_root_digest)?,
                    domain_sequence: row.domain_sequence as u64,
                    committed_at: row.committed_at,
                })
            })
            .collect()
    }

    /// Ordered mutations of one commit.
    pub async fn mutations_of_commit(
        &self,
        repo: &RepositoryId,
        commit: &CommitId,
    ) -> Result<Vec<Mutation>, SfsError> {
        let rows: Vec<MutationRow> = self
            .db()
            .query(
                "SELECT body FROM commit_mutation WHERE repository = $repo AND commit = $commit \
                 ORDER BY ordinal ASC",
            )
            .bind(("repo", rid_repo(repo)))
            .bind(("commit", rid_commit(repo, commit)))
            .await
            .map_err(|e| map_db_err("read mutations", e))?
            .take(0)
            .map_err(|e| map_db_err("decode mutations", e))?;
        rows.into_iter()
            .map(|row| {
                serde_json::from_str(&row.body.json)
                    .map_err(|e| SfsError::Corruption(format!("decode mutation: {e}")))
            })
            .collect()
    }

    /// Create the default agent + a RUNNING run for this session.
    pub async fn ensure_run(&self, repo: &RepositoryId) -> Result<String, SfsError> {
        let agent = RecordId::new("agent", format!("{repo}/default"));
        self.db()
            .query(
                "UPSERT $rid SET repository = $repo, stable_name = 'default', \
                 created_at = time::now()",
            )
            .bind(("rid", agent.clone()))
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("create agent", e))?
            .check()
            .map_err(|e| map_db_err("create agent", e))?;
        let run_key = format!("{repo}/{}", uuid_like());
        self.db()
            .query(
                "CREATE $rid SET repository = $repo, agent = $agent, actor = $actor, \
                 status = 'RUNNING', started_at = time::now(), finished_at = NONE",
            )
            .bind(("rid", RecordId::new("run", run_key.clone())))
            .bind(("repo", rid_repo(repo)))
            .bind(("agent", agent))
            .bind(("actor", rid_principal()))
            .await
            .map_err(|e| map_db_err("create run", e))?
            .check()
            .map_err(|e| map_db_err("create run", e))?;
        Ok(run_key)
    }

    /// Record the start of a tool call: a RUNNING span plus its tool_call record.
    /// Returns the span key used for `caused` attribution at publication.
    pub async fn tool_start(
        &self,
        repo: &RepositoryId,
        run_key: &str,
        tool_name: &str,
        input_preview: Option<String>,
    ) -> Result<String, SfsError> {
        let span_key = format!("{repo}/{}", uuid_like());
        self.db()
            .query(
                "CREATE $rid SET repository = $repo, run = $run, parent_span = NONE, \
                 kind = 'TOOL_CALL', name = $name, status = 'RUNNING', \
                 capture_quality = 'CAPTURED', started_at = time::now(), finished_at = NONE",
            )
            .bind(("rid", RecordId::new("span", span_key.clone())))
            .bind(("repo", rid_repo(repo)))
            .bind(("run", RecordId::new("run", run_key)))
            .bind(("name", tool_name.to_string()))
            .await
            .map_err(|e| map_db_err("create span", e))?
            .check()
            .map_err(|e| map_db_err("create span", e))?;
        self.db()
            .query(
                "CREATE $rid SET repository = $repo, run = $run, span = $span, \
                 tool_name = $name, input_preview = $input, output_preview = NONE, \
                 error_message = NONE, started_at = time::now(), finished_at = NONE",
            )
            .bind(("rid", RecordId::new("tool_call", span_key.clone())))
            .bind(("repo", rid_repo(repo)))
            .bind(("run", RecordId::new("run", run_key)))
            .bind(("span", RecordId::new("span", span_key.clone())))
            .bind(("name", tool_name.to_string()))
            .bind(("input", input_preview))
            .await
            .map_err(|e| map_db_err("create tool_call", e))?
            .check()
            .map_err(|e| map_db_err("create tool_call", e))?;
        Ok(span_key)
    }

    /// Record tool completion. `error` marks FAILED; otherwise SUCCEEDED.
    pub async fn tool_finish(
        &self,
        _repo: &RepositoryId,
        span_key: &str,
        output_preview: Option<String>,
        error: Option<String>,
    ) -> Result<(), SfsError> {
        let status = if error.is_some() {
            "FAILED"
        } else {
            "SUCCEEDED"
        };
        self.db()
            .query("UPDATE $rid SET status = $status, finished_at = time::now()")
            .bind(("rid", RecordId::new("span", span_key)))
            .bind(("status", status.to_string()))
            .await
            .map_err(|e| map_db_err("finish span", e))?
            .check()
            .map_err(|e| map_db_err("finish span", e))?;
        self.db()
            .query(
                "UPDATE $rid SET output_preview = $output, error_message = $error, \
                 finished_at = time::now(), \
                 duration_ms = duration::millis(time::now() - started_at)",
            )
            .bind(("rid", RecordId::new("tool_call", span_key)))
            .bind(("output", output_preview))
            .bind(("error", error))
            .await
            .map_err(|e| map_db_err("finish tool_call", e))?
            .check()
            .map_err(|e| map_db_err("finish tool_call", e))?;
        Ok(())
    }

    /// Recent tool calls, newest first, with their span status.
    pub async fn tool_recent(
        &self,
        repo: &RepositoryId,
        limit: usize,
    ) -> Result<Vec<ToolCallInfo>, SfsError> {
        let rows: Vec<ToolCallRow> = self
            .db()
            .query(
                "SELECT tool_name, span.status AS status, input_preview, output_preview, \
                 error_message, type::string(started_at) AS started_at FROM tool_call \
                 WHERE repository = $repo ORDER BY started_at DESC LIMIT $limit",
            )
            .bind(("repo", rid_repo(repo)))
            .bind(("limit", limit as i64))
            .await
            .map_err(|e| map_db_err("read tool calls", e))?
            .take(0)
            .map_err(|e| map_db_err("decode tool calls", e))?;
        Ok(rows
            .into_iter()
            .map(|row| ToolCallInfo {
                tool_name: row.tool_name,
                status: row.status,
                input_preview: row.input_preview,
                output_preview: row.output_preview,
                error_message: row.error_message,
                started_at: row.started_at,
            })
            .collect())
    }

    /// Open a workspace record (metadata only; deltas stay in the kernel).
    pub async fn workspace_open(
        &self,
        repo: &RepositoryId,
        branch: &surrealfs_types::BranchName,
        base_commit: &CommitId,
    ) -> Result<String, SfsError> {
        let ws_key = format!("{repo}/{}", uuid_like());
        self.db()
            .query(
                "CREATE $rid SET repository = $repo, branch = $branch, base_commit = $base, \
                 author_principal = $principal, status = 'OPEN', final_commit = NONE, \
                 abort_reason = NONE, created_at = time::now(), closed_at = NONE",
            )
            .bind(("rid", RecordId::new("workspace", ws_key.clone())))
            .bind(("repo", rid_repo(repo)))
            .bind(("branch", crate::rid_branch(repo, branch)))
            .bind(("base", rid_commit(repo, base_commit)))
            .bind(("principal", rid_principal()))
            .await
            .map_err(|e| map_db_err("open workspace", e))?
            .check()
            .map_err(|e| map_db_err("open workspace", e))?;
        self.db()
            .query("RELATE $ws->based_on->$base SET repository = $repo")
            .bind(("ws", RecordId::new("workspace", ws_key.clone())))
            .bind(("base", rid_commit(repo, base_commit)))
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("relate based_on", e))?
            .check()
            .map_err(|e| map_db_err("relate based_on", e))?;
        Ok(ws_key)
    }

    /// Mark a workspace aborted. Its staged state was purely in-memory and is gone.
    pub async fn workspace_abort(&self, ws_key: &str, reason: &str) -> Result<(), SfsError> {
        // (workspace keys embed the repository prefix, so no repo argument is needed)
        self.db()
            .query(
                "UPDATE $rid SET status = 'ABORTED', abort_reason = $reason, \
                 closed_at = time::now()",
            )
            .bind(("rid", RecordId::new("workspace", ws_key)))
            .bind(("reason", reason.to_string()))
            .await
            .map_err(|e| map_db_err("abort workspace", e))?
            .check()
            .map_err(|e| map_db_err("abort workspace", e))?;
        Ok(())
    }
}

/// Random 128-bit hex key for run/span/workspace record ids (not part of any digest).
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let mut e = surrealfs_types::canonical::Enc::new();
    e.u64(nanos as u64)
        .u64((nanos >> 64) as u64)
        .u64(std::process::id() as u64)
        .u64(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    surrealfs_types::canonical::digest("ephemeral-id", &e.finish()).as_str()[..32].to_string()
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Store {
    /// Number of stored state nodes for a repository. Used by tests to assert that a commit
    /// costs storage proportional to its change set rather than to the size of the tree.
    pub async fn state_node_count(&self, repo: &RepositoryId) -> Result<usize, SfsError> {
        let counts: Vec<i64> = self
            .db()
            .query("SELECT VALUE count() FROM state_node WHERE repository = $repo GROUP ALL")
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("count state nodes", e))?
            .take(0)
            .map_err(|e| map_db_err("decode state node count", e))?;
        Ok(counts.first().copied().unwrap_or(0) as usize)
    }
}

/// One step in a path's history: the commit that changed it and the tool call responsible.
#[derive(Debug, Clone)]
pub struct Provenance {
    pub commit: CommitId,
    pub kind: String,
    pub committed_at: String,
    pub message: Option<String>,
    /// `None` when the change was published without a declared tool-call span.
    pub tool_name: Option<String>,
    pub tool_status: Option<String>,
}

#[derive(SurrealValue)]
struct ProvenanceRow {
    commit: RecordId,
    kind: String,
    committed_at: String,
    message: Option<String>,
    tool_name: Option<String>,
    tool_status: Option<String>,
}

impl Store {
    /// Who changed this path, and why — newest first.
    ///
    /// This is the query AgentFS structurally cannot answer: its tool-call table has no edge
    /// to the filesystem. Here the mutation carries the path, the commit carries the authoring
    /// span, and the span carries the tool call, so one traversal covers the whole chain.
    pub async fn explain_path(
        &self,
        repo: &RepositoryId,
        path: &str,
        limit: usize,
    ) -> Result<Vec<Provenance>, SfsError> {
        let rows: Vec<ProvenanceRow> = self
            .db()
            .query(
                "SELECT commit, kind, \
                        type::string(commit.committed_at) AS committed_at, \
                        commit.message AS message, \
                        commit.author_span.name AS tool_name, \
                        commit.author_span.status AS tool_status \
                 FROM commit_mutation \
                 WHERE repository = $repo AND path = $path \
                 ORDER BY domain_sequence DESC LIMIT $limit",
            )
            .bind(("repo", rid_repo(repo)))
            .bind(("path", path.to_string()))
            .bind(("limit", limit as i64))
            .await
            .map_err(|e| map_db_err("explain path", e))?
            .take(0)
            .map_err(|e| map_db_err("decode provenance", e))?;
        rows.into_iter()
            .map(|row| {
                Ok(Provenance {
                    commit: commit_id_of_rid(&row.commit)?,
                    kind: row.kind,
                    committed_at: row.committed_at,
                    message: row.message,
                    tool_name: row.tool_name,
                    tool_status: row.tool_status,
                })
            })
            .collect()
    }
}

impl Store {
    /// The repository's first commit. Used as the default diff base; a dedicated ordered
    /// lookup rather than fetching the whole timeline and taking its tail.
    pub async fn first_commit(&self, repo: &RepositoryId) -> Result<CommitId, SfsError> {
        #[derive(SurrealValue)]
        struct Row {
            id: RecordId,
        }
        let rows: Vec<Row> = self
            .db()
            .query(
                "SELECT id FROM commit WHERE repository = $repo \
                 ORDER BY domain_sequence ASC LIMIT 1",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read first commit", e))?
            .take(0)
            .map_err(|e| map_db_err("decode first commit", e))?;
        let row = rows
            .first()
            .ok_or_else(|| SfsError::NotFound(format!("no commits in {repo}")))?;
        commit_id_of_rid(&row.id)
    }

    /// The commit that was current at a moment: the newest one published at or before `at`.
    ///
    /// Resolution is repository-wide rather than per-branch, because commits are numbered per
    /// repository — asking "what existed at 3pm" has one answer, and which branch it happened to
    /// be on is a separate question.
    ///
    /// The `domain_sequence` tie-break is load-bearing. `committed_at` is engine wall-clock with
    /// no monotonicity guarantee: a clock adjustment can give two commits the same timestamp, or
    /// invert them. `domain_sequence` is allocated under the publish lock and is authoritative
    /// for publication order, so ordering by it second means this never returns an earlier
    /// commit when a later one shares the timestamp.
    ///
    /// Uses the `commit_repo_time` index, defined in migration 0001 and unused until now.
    pub async fn commit_at_or_before(
        &self,
        repo: &RepositoryId,
        at: std::time::SystemTime,
    ) -> Result<Option<CommitId>, SfsError> {
        #[derive(SurrealValue)]
        struct Row {
            id: RecordId,
        }
        let stamp = surrealfs_types::time::format_rfc3339(at);
        let rows: Vec<Row> = self
            .db()
            .query(
                "SELECT id FROM commit \
                 WHERE repository = $repo AND committed_at <= <datetime>$at \
                 ORDER BY committed_at DESC, domain_sequence DESC LIMIT 1",
            )
            .bind(("repo", rid_repo(repo)))
            .bind(("at", stamp))
            .await
            .map_err(|e| map_db_err("read commit at time", e))?
            .take(0)
            .map_err(|e| map_db_err("decode commit at time", e))?;
        rows.first()
            .map(|row| commit_id_of_rid(&row.id))
            .transpose()
    }

    /// When a commit was published, as an RFC 3339 string.
    pub async fn committed_at(
        &self,
        repo: &RepositoryId,
        commit: &CommitId,
    ) -> Result<Option<String>, SfsError> {
        #[derive(SurrealValue)]
        struct Row {
            committed_at: String,
        }
        let row: Option<Row> = self
            .db()
            .query("SELECT type::string(committed_at) AS committed_at FROM ONLY $rid")
            .bind(("rid", crate::rid_commit(repo, commit)))
            .await
            .map_err(|e| map_db_err("read commit time", e))?
            .take(0)
            .map_err(|e| map_db_err("decode commit time", e))?;
        Ok(row.map(|r| r.committed_at))
    }
}

/// Aggregate outcomes for one tool name.
#[derive(Debug, Clone)]
pub struct ToolStats {
    pub tool_name: String,
    pub calls: u64,
    pub succeeded: u64,
    pub failed: u64,
    /// Calls that started and never reported an outcome. They are counted, but deliberately
    /// excluded from the duration aggregates: an interrupted call has no duration, and
    /// treating it as instant would flatter every average.
    pub running: u64,
    pub avg_duration_ms: Option<f64>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
}

#[derive(SurrealValue)]
struct ToolStatsRow {
    tool_name: String,
    calls: i64,
    succeeded: i64,
    failed: i64,
    running: i64,
    avg_duration_ms: Option<f64>,
    min_duration_ms: Option<i64>,
    max_duration_ms: Option<i64>,
}

impl Store {
    /// Per-tool call counts, outcomes, and duration aggregates, busiest first.
    pub async fn tool_stats(&self, repo: &RepositoryId) -> Result<Vec<ToolStats>, SfsError> {
        let rows: Vec<ToolStatsRow> = self
            .db()
            .query(
                "SELECT tool_name, \
                        count() AS calls, \
                        count(span.status = 'SUCCEEDED') AS succeeded, \
                        count(span.status = 'FAILED') AS failed, \
                        count(span.status = 'RUNNING') AS running, \
                        math::mean(duration_ms) AS avg_duration_ms, \
                        math::min(duration_ms) AS min_duration_ms, \
                        math::max(duration_ms) AS max_duration_ms \
                 FROM tool_call WHERE repository = $repo \
                 GROUP BY tool_name",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read tool stats", e))?
            .take(0)
            .map_err(|e| map_db_err("decode tool stats", e))?;
        let mut stats: Vec<ToolStats> = rows
            .into_iter()
            .map(|row| ToolStats {
                tool_name: row.tool_name,
                calls: row.calls as u64,
                succeeded: row.succeeded as u64,
                failed: row.failed as u64,
                running: row.running as u64,
                avg_duration_ms: row.avg_duration_ms,
                min_duration_ms: row.min_duration_ms,
                max_duration_ms: row.max_duration_ms,
            })
            .collect();
        stats.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.tool_name.cmp(&b.tool_name)));
        Ok(stats)
    }
}
