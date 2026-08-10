//! Reclaiming unreferenced content and tree nodes.
//!
//! Two kinds of garbage accumulate.
//!
//! Chunk payloads are staged *before* the publication transaction, deliberately, so that a
//! large write never rides inside the transaction that moves the branch head. If that
//! transaction then fails — an expected-head conflict, most often — the bytes are already in
//! the store and no commit will ever name them. (A workspace that is simply aborted leaves
//! nothing behind: its writes never left process memory, because staging happens at publish.)
//!
//! Separately, an edit that rewrites the tree several times in one workspace — an unlink that
//! also rewrites a hard-link group, say — produces intermediate roots that are stored but
//! never referenced by a commit.
//!
//! Reachability, not reference counting, decides what stays: walk every state root, collect
//! what it reaches, and everything else is garbage. That is the only approach that stays
//! correct when content is shared by digest, because an object's reference count is a fact
//! about the whole store rather than about the operation that created it.
//!
//! A grace period keeps recently written objects regardless. There is a real window between
//! staging a chunk and committing the reference to it, and a sweep landing inside that window
//! would otherwise reap bytes a publication is about to claim.

use std::collections::BTreeSet;

use surrealdb::types::{RecordId, SurrealValue};
use surrealfs_types::{RepositoryId, SfsError};

use crate::{bodies, map_db_err, rid_repo, Store};

/// Objects that are never collected before this much time has passed, whatever the reachability
/// analysis says. One hour is far longer than any publication takes and short enough that an
/// aborted session does not hold bytes for a working day.
pub const DEFAULT_GRACE_SECONDS: i64 = 3600;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub chunks_removed: usize,
    pub nodes_removed: usize,
    pub bytes_reclaimed: u64,
    /// Objects that were unreferenced but young enough to be inside the grace period.
    pub kept_within_grace: usize,
}

#[derive(SurrealValue)]
struct RootNodes {
    ns: String,
    kv: String,
}

#[derive(SurrealValue)]
struct NodeRow {
    digest: String,
    kind: String,
    json: String,
    age_seconds: i64,
}

#[derive(SurrealValue)]
struct ChunkRow {
    digest: String,
    length: i64,
    age_seconds: i64,
}

impl Store {
    /// Remove content and tree nodes that no state root reaches.
    ///
    /// Commits are never removed, so anything a commit's root reaches is retained; this only
    /// collects what nothing points at.
    pub async fn gc(&self, repo: &RepositoryId, grace_seconds: i64) -> Result<GcReport, SfsError> {
        // Serialise against publication. A sweep that ran concurrently with a publish could
        // observe a root before the nodes beneath it, and conclude they were unreachable.
        let _guard = self.publish_lock.lock().await;

        let roots: Vec<RootNodes> = self
            .db()
            .query(
                "SELECT namespace_node.digest AS ns, kv_node.digest AS kv \
                 FROM state_root WHERE repository = $repo",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read state roots", e))?
            .take(0)
            .map_err(|e| map_db_err("decode state roots", e))?;

        // Every node in the store, so the walk resolves without a query per step.
        let nodes: Vec<NodeRow> = self
            .db()
            .query(
                "SELECT digest, kind, body.json AS json, \
                        duration::secs(time::now() - created_at) AS age_seconds \
                 FROM state_node WHERE repository = $repo",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read state nodes", e))?
            .take(0)
            .map_err(|e| map_db_err("decode state nodes", e))?;

        let mut reachable_nodes: BTreeSet<String> = BTreeSet::new();
        let mut referenced_chunks: BTreeSet<String> = BTreeSet::new();

        for root in &roots {
            walk_tree(
                &nodes,
                &root.ns,
                &mut reachable_nodes,
                &mut referenced_chunks,
            )?;
            // The KV node is reachable, and its values name chunks.
            if reachable_nodes.insert(root.kv.clone()) {
                if let Some(node) = nodes.iter().find(|n| n.digest == root.kv) {
                    let id = surrealfs_types::StateNodeId::parse(&node.digest)?;
                    for digest in bodies::decode_kv(&id, &node.json)?.values() {
                        referenced_chunks.insert(digest.to_string());
                    }
                }
            }
        }

        let mut report = GcReport::default();

        for node in &nodes {
            if reachable_nodes.contains(&node.digest) {
                continue;
            }
            if node.age_seconds < grace_seconds {
                report.kept_within_grace += 1;
                continue;
            }
            self.db()
                .query("DELETE $rid")
                .bind((
                    "rid",
                    RecordId::new("state_node", format!("{repo}/{}", node.digest)),
                ))
                .await
                .map_err(|e| map_db_err("remove node", e))?
                .check()
                .map_err(|e| map_db_err("remove node", e))?;
            report.nodes_removed += 1;
        }

        let chunks: Vec<ChunkRow> = self
            .db()
            .query(
                "SELECT digest, length, \
                        duration::secs(time::now() - created_at) AS age_seconds \
                 FROM chunk WHERE repository = $repo",
            )
            .bind(("repo", rid_repo(repo)))
            .await
            .map_err(|e| map_db_err("read chunks", e))?
            .take(0)
            .map_err(|e| map_db_err("decode chunks", e))?;

        for chunk in &chunks {
            if referenced_chunks.contains(&chunk.digest) {
                continue;
            }
            if chunk.age_seconds < grace_seconds {
                report.kept_within_grace += 1;
                continue;
            }
            self.db()
                .query("DELETE $rid")
                .bind((
                    "rid",
                    RecordId::new("chunk", format!("{repo}/{}", chunk.digest)),
                ))
                .await
                .map_err(|e| map_db_err("remove chunk", e))?
                .check()
                .map_err(|e| map_db_err("remove chunk", e))?;
            report.chunks_removed += 1;
            report.bytes_reclaimed += chunk.length as u64;
        }

        Ok(report)
    }
}

/// Mark every node and chunk reachable from one tree root.
fn walk_tree(
    nodes: &[NodeRow],
    root: &str,
    reachable: &mut BTreeSet<String>,
    chunks: &mut BTreeSet<String>,
) -> Result<(), SfsError> {
    // Iterative rather than recursive: a deep namespace should not depend on stack depth.
    let mut pending = vec![root.to_string()];
    while let Some(digest) = pending.pop() {
        if !reachable.insert(digest.clone()) {
            continue;
        }
        let Some(node) = nodes.iter().find(|n| n.digest == digest) else {
            // The empty tree node is a constant and may legitimately be absent.
            continue;
        };
        if node.kind != "DIR" {
            continue;
        }
        let id = surrealfs_types::StateNodeId::parse(&node.digest)?;
        for entry in bodies::decode_dir(&id, &node.json)?.entries.values() {
            match entry {
                surrealfs_content::tree::Entry::Dir { node, .. } => pending.push(node.to_string()),
                surrealfs_content::tree::Entry::File { extents, .. } => {
                    for extent in extents {
                        chunks.insert(extent.chunk.to_string());
                    }
                }
                surrealfs_content::tree::Entry::Symlink { .. } => {}
            }
        }
    }
    Ok(())
}
