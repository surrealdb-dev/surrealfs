//! Commit plans and receipts: the store adapter's input and output types.

use std::collections::BTreeMap;

use surrealfs_content::tree::DirNode;
use surrealfs_types::state::{kv_digest, root_digest, KvMap, Mutation};
use surrealfs_types::{
    BranchName, CommitId, Digest, RepositoryId, RequestId, StateNodeId, StateRootId,
};

/// A validated publication request produced by the kernel. Carries only bounded
/// metadata and content references; chunk payloads are staged separately beforehand.
#[derive(Debug, Clone)]
pub struct CommitPlan {
    pub repository: RepositoryId,
    pub branch: BranchName,
    pub request_id: RequestId,
    /// Branch head the caller based its changes on (compare-and-swap check).
    pub expected_head: CommitId,
    /// State root the caller based its changes on.
    pub base_root: StateRootId,
    /// Root of the new namespace tree.
    pub namespace_root: StateNodeId,
    /// Tree nodes this commit introduces. Nodes carried over from the base tree are absent,
    /// which is what keeps a publication proportional to its change set.
    pub new_nodes: BTreeMap<StateNodeId, DirNode>,
    /// Complete new KV map (see `KvMap` for why this half is not yet a tree).
    pub kv: KvMap,
    pub mutations: Vec<Mutation>,
    /// Span record key (`span:<key>`) that authored this commit, if declared.
    pub author_span: Option<String>,
    /// Workspace record key to mark COMMITTED and link via `published_as`.
    pub workspace: Option<String>,
    pub message: Option<String>,
}

impl CommitPlan {
    /// The state root this plan publishes.
    pub fn new_root(&self) -> StateRootId {
        root_digest(&self.namespace_root, &kv_digest(&self.kv))
    }

    pub fn command_hash(&self) -> Digest {
        surrealfs_types::state::command_hash(
            self.repository.as_str(),
            self.branch.as_str(),
            &self.request_id,
            &self.base_root,
            &self.mutations,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptOutcome {
    /// This request performed the publication.
    Applied,
    /// The same request (same command hash) was already applied earlier.
    Replayed,
}

/// Durable evidence of one applied publication.
#[derive(Debug, Clone)]
pub struct CommitReceipt {
    pub request_id: RequestId,
    pub outcome: ReceiptOutcome,
    pub commit: CommitId,
    pub state_root: StateRootId,
    pub previous_head: CommitId,
    pub domain_sequence: u64,
    /// Hash of the command that produced this receipt; a replayed request must match it.
    pub command_hash: Digest,
}

/// Publication budgets enforced before the engine transaction begins (the product
/// budget from RUST_SDK_PLAN.md; deliberately far below any engine memtable limit).
pub const MAX_PUBLICATION_METADATA_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PUBLICATION_MUTATIONS: usize = 10_000;
