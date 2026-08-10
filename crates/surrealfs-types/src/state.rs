//! Mutations and root digests.
//!
//! A state root is the digest over the namespace tree root and the KV node. The namespace is a
//! persistent content-addressed tree (`surrealfs_content::tree`) so unchanged subtrees are
//! shared between commits; the KV half is a single whole-map node, which is sized for agent
//! workloads and documented on [`KvMap`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::canonical::{digest, Enc};
use crate::id::{ChunkDigest, CommitId, Digest, RequestId, StateNodeId, StateRootId};
use crate::path::RepoPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InodeKind {
    Regular,
    Directory,
    Symlink,
}

impl InodeKind {
    pub fn tag(self) -> u8 {
        match self {
            InodeKind::Regular => 0,
            InodeKind::Directory => 1,
            InodeKind::Symlink => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            InodeKind::Regular => "REGULAR",
            InodeKind::Directory => "DIRECTORY",
            InodeKind::Symlink => "SYMLINK",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InodeMeta {
    pub kind: InodeKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub symlink_target: Option<String>,
}

impl InodeMeta {
    pub fn file(size: u64) -> Self {
        InodeMeta {
            kind: InodeKind::Regular,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size,
            symlink_target: None,
        }
    }

    pub fn dir() -> Self {
        InodeMeta {
            kind: InodeKind::Directory,
            mode: 0o755,
            uid: 0,
            gid: 0,
            size: 0,
            symlink_target: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub file_offset: u64,
    pub length: u64,
    pub chunk: ChunkDigest,
}

/// KV entries are namespaced string keys whose value bytes are stored as content chunks.
pub type KvKey = (String, String);

/// The KV half of a state root.
///
/// Unlike the namespace, this is still a single whole-map node. Agent KV sets are small
/// (hundreds of keys) where file trees reach tens of thousands, so the structural sharing that
/// the namespace needs does not yet pay for itself here. Promoting this to an ordered
/// content-addressed map is a contained change: only [`kv_digest`] and the store's node
/// encoding would move.
pub type KvMap = BTreeMap<KvKey, ChunkDigest>;

/// Digest of the KV map.
pub fn kv_digest(kv: &KvMap) -> StateNodeId {
    let mut e = Enc::new();
    e.seq(kv.len());
    for ((ns, key), value) in kv {
        e.str(ns).str(key).digest(&value.0);
    }
    StateNodeId(digest("kv-node", &e.finish()))
}

/// A state root: the namespace tree root plus the KV node.
///
/// Both halves are content-addressed, so equal logical state yields an equal root regardless of
/// the history that produced it. That is what makes root verification a proof of byte-for-byte
/// restoration rather than a claim about provenance.
pub fn root_digest(namespace_tree: &StateNodeId, kv: &StateNodeId) -> StateRootId {
    let mut e = Enc::new();
    e.digest(&namespace_tree.0).digest(&kv.0);
    StateRootId(digest("state-root", &e.finish()))
}

/// One logical mutation inside a commit, recorded in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Mutation {
    MkDir {
        path: RepoPath,
    },
    WriteFile {
        path: RepoPath,
        size: u64,
        content: Vec<Extent>,
    },
    Unlink {
        path: RepoPath,
    },
    RmDir {
        path: RepoPath,
    },
    /// A rename, recorded as intent rather than inferred.
    ///
    /// The state root sees only that one path vanished and another appeared — identity is the
    /// path, so there is nothing in the tree to carry the relationship. Recording it here is
    /// what lets `explain` say "renamed from /old" instead of showing an unrelated delete and
    /// add, and it costs one mutation row.
    Rename {
        from: RepoPath,
        to: RepoPath,
    },
    /// Metadata-only change; content is untouched.
    SetMeta {
        path: RepoPath,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    Symlink {
        path: RepoPath,
        target: String,
    },
    /// A second name for an existing file.
    Link {
        from: RepoPath,
        to: RepoPath,
    },
    KvSet {
        namespace: String,
        key: String,
        value: ChunkDigest,
        value_len: u64,
    },
    KvDelete {
        namespace: String,
        key: String,
    },
}

impl Mutation {
    /// The path this mutation touched. KV entries use a `kv:` prefix so filesystem paths
    /// and KV keys share one queryable column without ever colliding.
    pub fn target_path(&self) -> String {
        match self {
            Mutation::MkDir { path }
            | Mutation::WriteFile { path, .. }
            | Mutation::Unlink { path }
            | Mutation::RmDir { path }
            | Mutation::SetMeta { path, .. }
            | Mutation::Symlink { path, .. } => path.to_string(),
            // Indexed under the destination: that is where the file is now, and it is what a
            // reader asks about. The source appears in the body for `explain` to report.
            Mutation::Rename { to, .. } | Mutation::Link { to, .. } => to.to_string(),
            Mutation::KvSet { namespace, key, .. } | Mutation::KvDelete { namespace, key } => {
                format!("kv:{namespace}/{key}")
            }
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            Mutation::MkDir { .. } => "MKDIR",
            Mutation::WriteFile { .. } => "WRITE_FILE",
            Mutation::Unlink { .. } => "UNLINK",
            Mutation::RmDir { .. } => "RMDIR",
            Mutation::Rename { .. } => "RENAME",
            Mutation::SetMeta { .. } => "SET_META",
            Mutation::Symlink { .. } => "SYMLINK",
            Mutation::Link { .. } => "LINK",
            Mutation::KvSet { .. } => "KV_SET",
            Mutation::KvDelete { .. } => "KV_DELETE",
        }
    }

    fn encode(&self, e: &mut Enc) {
        match self {
            Mutation::MkDir { path } => {
                e.u8(0).str(path.as_str());
            }
            Mutation::WriteFile {
                path,
                size,
                content,
            } => {
                e.u8(1).str(path.as_str()).u64(*size).seq(content.len());
                for ext in content {
                    e.u64(ext.file_offset).u64(ext.length).digest(&ext.chunk.0);
                }
            }
            Mutation::Unlink { path } => {
                e.u8(2).str(path.as_str());
            }
            Mutation::RmDir { path } => {
                e.u8(3).str(path.as_str());
            }
            Mutation::KvSet {
                namespace,
                key,
                value,
                value_len,
            } => {
                e.u8(4)
                    .str(namespace)
                    .str(key)
                    .digest(&value.0)
                    .u64(*value_len);
            }
            Mutation::KvDelete { namespace, key } => {
                e.u8(5).str(namespace).str(key);
            }
            Mutation::Rename { from, to } => {
                e.u8(6).str(from.as_str()).str(to.as_str());
            }
            Mutation::SetMeta {
                path,
                mode,
                uid,
                gid,
            } => {
                e.u8(7).str(path.as_str()).u32(*mode).u32(*uid).u32(*gid);
            }
            Mutation::Symlink { path, target } => {
                e.u8(8).str(path.as_str()).str(target);
            }
            Mutation::Link { from, to } => {
                e.u8(9).str(from.as_str()).str(to.as_str());
            }
        }
    }
}

/// Deterministic hash of a publication command: same request id + same command hash is a
/// replay; same request id + different command hash is a caller bug.
pub fn command_hash(
    repository: &str,
    branch: &str,
    request_id: &RequestId,
    base_root: &StateRootId,
    mutations: &[Mutation],
) -> Digest {
    let mut e = Enc::new();
    e.str(repository)
        .str(branch)
        .str(request_id.as_str())
        .digest(&base_root.0)
        .seq(mutations.len());
    for m in mutations {
        m.encode(&mut e);
    }
    digest("publish-command", &e.finish())
}

/// Deterministic commit identity.
///
/// Note what is *not* here: the repository. A commit is identified by its content — its
/// parent, the state it produced, and what it claims to be — so the same history carries the
/// same identity wherever it lives. That is what makes a session archive portable: importing
/// it elsewhere reproduces the commits rather than renaming them. Storage still separates
/// repositories, because the record id carries the repository prefix.
pub fn commit_digest(
    first_parent: Option<&CommitId>,
    state_root: &StateRootId,
    request_id: &RequestId,
    author: &str,
    message: Option<&str>,
    mutation_count: u64,
) -> CommitId {
    let mut e = Enc::new();
    match first_parent {
        None => {
            e.u8(0);
        }
        Some(p) => {
            e.u8(1).digest(&p.0);
        }
    }
    e.digest(&state_root.0)
        .str(request_id.as_str())
        .str(author)
        .opt_str(message)
        .u64(mutation_count);
    CommitId(digest("commit", &e.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::chunk_digest;

    #[test]
    fn kv_digest_is_order_independent_and_content_sensitive() {
        let mut a = KvMap::new();
        a.insert(("app".into(), "k".into()), ChunkDigest(chunk_digest(b"v")));
        a.insert(("app".into(), "z".into()), ChunkDigest(chunk_digest(b"w")));
        let mut b = KvMap::new();
        b.insert(("app".into(), "z".into()), ChunkDigest(chunk_digest(b"w")));
        b.insert(("app".into(), "k".into()), ChunkDigest(chunk_digest(b"v")));
        assert_eq!(kv_digest(&a), kv_digest(&b));

        b.insert(
            ("app".into(), "k".into()),
            ChunkDigest(chunk_digest(b"changed")),
        );
        assert_ne!(kv_digest(&a), kv_digest(&b));
    }

    /// Frozen for ROOT_FORMAT_VERSION 1 / HASH_VERSION 1. A change here is a format break,
    /// not a test to update.
    #[test]
    fn empty_kv_node_golden() {
        assert_eq!(
            kv_digest(&KvMap::new()).as_str(),
            "48f43d19930bdf5799d284997d9713679855e516554d5741ea864cc9599a4819"
        );
    }

    #[test]
    fn root_binds_both_halves() {
        let ns_a = StateNodeId(digest("dir-node", b"a"));
        let ns_b = StateNodeId(digest("dir-node", b"b"));
        let kv = kv_digest(&KvMap::new());
        assert_eq!(root_digest(&ns_a, &kv), root_digest(&ns_a, &kv));
        assert_ne!(root_digest(&ns_a, &kv), root_digest(&ns_b, &kv));

        let mut kv_map = KvMap::new();
        kv_map.insert(("n".into(), "k".into()), ChunkDigest(chunk_digest(b"v")));
        assert_ne!(
            root_digest(&ns_a, &kv),
            root_digest(&ns_a, &kv_digest(&kv_map))
        );
    }
}
