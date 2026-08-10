//! Store-backed access to the namespace tree.
//!
//! The tree operations in `surrealfs_content::tree` are synchronous and read through a
//! `NodeSource`, while the store is async. These helpers bridge the two: fetch the nodes an
//! operation needs into a [`MemNodes`] cache, then run the synchronous tree code against it.
//!
//! Resolving a path costs one round trip per component rather than a load of the whole
//! namespace, which is the point of the tree.

use surrealfs_content::tree::{self, DirNode, Entry, MemNodes};
use surrealfs_store::Store;
use surrealfs_types::{RepoPath, RepositoryId, SfsError, StateNodeId};

/// One directory listing row, as surfaced by the SDK and CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

impl DirEntry {
    pub fn from_entry(name: String, entry: &Entry) -> Self {
        match entry {
            Entry::Dir { .. } => DirEntry {
                name,
                is_dir: true,
                size: 0,
            },
            Entry::File { size, .. } => DirEntry {
                name,
                is_dir: false,
                size: *size,
            },
            Entry::Symlink { target, .. } => DirEntry {
                name,
                is_dir: false,
                size: target.len() as u64,
            },
        }
    }
}

/// Fetch one node into the cache if it is not already there.
pub async fn load_node(
    store: &Store,
    repo: &RepositoryId,
    id: &StateNodeId,
    cache: &mut MemNodes,
) -> Result<DirNode, SfsError> {
    if let Some(node) = cache.nodes.get(id) {
        return Ok(node.clone());
    }
    if *id == tree::empty_root() {
        return Ok(DirNode::default());
    }
    let mut fetched = store.dir_nodes(repo, std::slice::from_ref(id)).await?;
    let (_, node) = fetched
        .pop()
        .ok_or_else(|| SfsError::NotFound(format!("tree node {id}")))?;
    cache.nodes.insert(id.clone(), node.clone());
    Ok(node)
}

/// Load every directory node along `path`, so the synchronous tree code can then read or
/// rewrite that route without touching storage again.
pub async fn prefetch(
    store: &Store,
    repo: &RepositoryId,
    root: &StateNodeId,
    path: &RepoPath,
    cache: &mut MemNodes,
) -> Result<(), SfsError> {
    let mut current = load_node(store, repo, root, cache).await?;
    if path.is_root() {
        return Ok(());
    }
    for comp in path.as_str().trim_start_matches('/').split('/') {
        match current.entries.get(comp) {
            Some(Entry::Dir { node, .. }) => {
                let node = node.clone();
                current = load_node(store, repo, &node, cache).await?;
            }
            // A non-directory or missing component ends the route; the caller's tree
            // operation decides whether that is an error.
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// Resolve a single path against a namespace root.
pub async fn stat(
    store: &Store,
    repo: &RepositoryId,
    root: &StateNodeId,
    path: &RepoPath,
) -> Result<Option<Entry>, SfsError> {
    let mut cache = MemNodes::default();
    prefetch(store, repo, root, path, &mut cache).await?;
    tree::get(&cache, root, path)
}

/// List a directory against a namespace root.
pub async fn list_dir(
    store: &Store,
    repo: &RepositoryId,
    root: &StateNodeId,
    path: &RepoPath,
) -> Result<Vec<DirEntry>, SfsError> {
    let mut cache = MemNodes::default();
    prefetch(store, repo, root, path, &mut cache).await?;
    // readdir descends one level past the prefetched route to reach the directory itself.
    if let Some(Entry::Dir { node, .. }) = tree::get(&cache, root, path)? {
        load_node(store, repo, &node, &mut cache).await?;
    }
    Ok(tree::readdir(&cache, root, path)?
        .into_iter()
        .map(|(name, entry)| DirEntry::from_entry(name, &entry))
        .collect())
}

/// Every path in a tree, ordered. Used by ingest, diff, and export; costs a full walk.
pub async fn walk_all(
    store: &Store,
    repo: &RepositoryId,
    root: &StateNodeId,
) -> Result<Vec<(RepoPath, Entry)>, SfsError> {
    let mut cache = MemNodes::default();
    load_all_into(store, repo, root, &mut cache).await?;
    tree::walk(&cache, root)
}

/// Load an entire tree into the cache. Only for whole-tree operations.
pub async fn load_all_into(
    store: &Store,
    repo: &RepositoryId,
    root: &StateNodeId,
    cache: &mut MemNodes,
) -> Result<(), SfsError> {
    let node = load_node(store, repo, root, cache).await?;
    for entry in node.entries.values() {
        if let Entry::Dir { node, .. } = entry {
            Box::pin(load_all_into(store, repo, node, cache)).await?;
        }
    }
    Ok(())
}
