//! Content-addressed persistent directory tree.
//!
//! The namespace is a tree of immutable directory nodes, each addressed by the digest of its
//! canonical encoding. A node's digest covers its children's digests, so an unchanged subtree
//! keeps its digest and is shared by every commit that references it.
//!
//! Two properties follow, and both are load-bearing:
//!
//! * writing a path rewrites only the nodes on the route from the root to that path, so a
//!   commit persists O(changed paths x depth) nodes rather than the whole namespace;
//! * two trees with equal logical content have equal digests, so a diff can skip any subtree
//!   whose digests match on both sides without reading it.
//!
//! Entry identity is the path, and file content is referenced by chunk digest. There are no
//! allocated inode numbers: an allocated identity would make the root depend on the history
//! that produced it rather than on the content it holds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use surrealfs_types::canonical::{digest, Enc};
use surrealfs_types::state::Extent;
use surrealfs_types::{RepoPath, SfsError, StateNodeId};

/// Leading byte on every entry encoding. It costs one byte and lets entry payloads change
/// later (for example to externalise long extent lists) without restructuring the tree.
pub const ENTRY_ENCODING_VERSION: u8 = 1;

/// POSIX bits carried on every entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Meta {
    pub fn file() -> Self {
        Meta {
            mode: 0o644,
            uid: 0,
            gid: 0,
        }
    }

    pub fn dir() -> Self {
        Meta {
            mode: 0o755,
            uid: 0,
            gid: 0,
        }
    }

    fn encode(&self, e: &mut Enc) {
        e.u32(self.mode).u32(self.uid).u32(self.gid);
    }
}

/// One entry in a directory node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Entry {
    /// A subdirectory, referenced by the digest of its own node.
    Dir {
        meta: Meta,
        node: StateNodeId,
    },
    /// A regular file. Extents are ordered and cover `[0, size)` with no gaps.
    File {
        meta: Meta,
        size: u64,
        extents: Vec<Extent>,
        /// Every path in this file's hard-link group, sorted, including its own — empty for
        /// the ordinary single-link case.
        ///
        /// Membership is stored rather than derived because identity here is the path: two
        /// files that merely happen to hold the same bytes must not become linked, and an
        /// allocated inode number would make the root depend on the history that produced it.
        /// Recording the group as content keeps the root a pure function of logical state.
        ///
        /// The list is duplicated across the group's members, so a group of N paths costs
        /// O(N²) bytes in total. That is deliberate: real hard-link groups have two or three
        /// members, and paying a little there avoids a second addressing scheme everywhere.
        links: Vec<RepoPath>,
    },
    Symlink {
        meta: Meta,
        target: String,
    },
}

impl Entry {
    pub fn meta(&self) -> Meta {
        match self {
            Entry::Dir { meta, .. } | Entry::File { meta, .. } | Entry::Symlink { meta, .. } => {
                *meta
            }
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Entry::Dir { .. })
    }

    /// Number of directory entries referring to this file. Always at least one.
    pub fn link_count(&self) -> usize {
        match self {
            Entry::File { links, .. } => links.len().max(1),
            _ => 1,
        }
    }

    /// The other paths sharing this file, if any.
    pub fn link_group(&self) -> &[RepoPath] {
        match self {
            Entry::File { links, .. } => links,
            _ => &[],
        }
    }

    /// Digest of this entry's content alone, used by diff to detect modification without
    /// comparing extent lists element by element.
    pub fn content_digest(&self) -> surrealfs_types::Digest {
        let mut e = Enc::new();
        self.encode(&mut e);
        digest("tree-entry", &e.finish())
    }

    fn encode(&self, e: &mut Enc) {
        e.u8(ENTRY_ENCODING_VERSION);
        match self {
            Entry::Dir { meta, node } => {
                e.u8(0);
                meta.encode(e);
                e.digest(&node.0);
            }
            Entry::File {
                meta,
                size,
                extents,
                links,
            } => {
                e.u8(1);
                meta.encode(e);
                e.u64(*size).seq(extents.len());
                for ext in extents {
                    e.u64(ext.file_offset).u64(ext.length).digest(&ext.chunk.0);
                }
                e.seq(links.len());
                for link in links {
                    e.str(link.as_str());
                }
            }
            Entry::Symlink { meta, target } => {
                e.u8(2);
                meta.encode(e);
                e.str(target);
            }
        }
    }
}

/// An immutable directory node: names to entries, ordered by name.
///
/// Ordering is part of the encoding, so `readdir` is a sorted read of one node and the digest
/// does not depend on insertion order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirNode {
    pub entries: BTreeMap<String, Entry>,
}

impl DirNode {
    pub fn digest(&self) -> StateNodeId {
        let mut e = Enc::new();
        e.seq(self.entries.len());
        for (name, entry) in &self.entries {
            e.str(name);
            entry.encode(&mut e);
        }
        StateNodeId(digest("dir-node", &e.finish()))
    }
}

/// Digest of the empty tree — the root of a freshly created repository.
pub fn empty_root() -> StateNodeId {
    DirNode::default().digest()
}

/// Read access to previously persisted nodes.
pub trait NodeSource {
    fn dir_node(&self, id: &StateNodeId) -> Result<DirNode, SfsError>;
}

/// Resolve a node id, short-circuiting the empty node.
///
/// The empty directory is a well-known constant, so it is never written to storage and a fresh
/// repository holds zero tree nodes. Every read path goes through here rather than calling
/// `NodeSource::dir_node` directly, which keeps that invariant in one place.
pub fn resolve<S: NodeSource + ?Sized>(source: &S, id: &StateNodeId) -> Result<DirNode, SfsError> {
    if *id == empty_root() {
        return Ok(DirNode::default());
    }
    source.dir_node(id)
}

/// A `NodeSource` backed by an in-memory map. Used by tests and by the reference model.
#[derive(Debug, Clone, Default)]
pub struct MemNodes {
    pub nodes: BTreeMap<StateNodeId, DirNode>,
}

impl MemNodes {
    pub fn insert_all(&mut self, nodes: BTreeMap<StateNodeId, DirNode>) {
        self.nodes.extend(nodes);
    }
}

impl NodeSource for MemNodes {
    fn dir_node(&self, id: &StateNodeId) -> Result<DirNode, SfsError> {
        self.nodes
            .get(id)
            .cloned()
            .ok_or_else(|| SfsError::NotFound(format!("tree node {id}")))
    }
}

/// Accumulates the nodes created while applying a batch of changes.
///
/// Reads fall through to nodes written earlier in the same batch before reaching the backing
/// source, so successive edits compose. `into_new_nodes` returns exactly the set that must be
/// persisted, which is what keeps a commit proportional to its change set.
pub struct TreeWriter<'a, S: NodeSource> {
    source: &'a S,
    new_nodes: BTreeMap<StateNodeId, DirNode>,
}

impl<'a, S: NodeSource> TreeWriter<'a, S> {
    pub fn new(source: &'a S) -> Self {
        TreeWriter {
            source,
            new_nodes: BTreeMap::new(),
        }
    }

    fn load(&self, id: &StateNodeId) -> Result<DirNode, SfsError> {
        if let Some(node) = self.new_nodes.get(id) {
            return Ok(node.clone());
        }
        resolve(self.source, id)
    }

    fn store(&mut self, node: DirNode) -> StateNodeId {
        let id = node.digest();
        if !node.entries.is_empty() {
            self.new_nodes.insert(id.clone(), node);
        }
        id
    }

    pub fn into_new_nodes(self) -> BTreeMap<StateNodeId, DirNode> {
        self.new_nodes
    }

    pub fn new_node_count(&self) -> usize {
        self.new_nodes.len()
    }

    /// Insert or replace `path`, creating missing parent directories.
    pub fn insert(
        &mut self,
        root: &StateNodeId,
        path: &RepoPath,
        entry: Entry,
    ) -> Result<StateNodeId, SfsError> {
        let comps = components(path)?;
        self.insert_at(root, &comps, entry)
    }

    fn insert_at(
        &mut self,
        node_id: &StateNodeId,
        comps: &[&str],
        entry: Entry,
    ) -> Result<StateNodeId, SfsError> {
        let mut node = self.load(node_id)?;
        let (head, rest) = comps
            .split_first()
            .expect("path has at least one component");

        if rest.is_empty() {
            node.entries.insert((*head).to_string(), entry);
            return Ok(self.store(node));
        }

        let child_id = match node.entries.get(*head) {
            Some(Entry::Dir { node, .. }) => node.clone(),
            Some(_) => {
                return Err(SfsError::NotADirectory(format!(
                    "{head} is not a directory"
                )))
            }
            None => self.store(DirNode::default()),
        };
        let child_meta = match node.entries.get(*head) {
            Some(Entry::Dir { meta, .. }) => *meta,
            _ => Meta::dir(),
        };
        let new_child = self.insert_at(&child_id, rest, entry)?;
        node.entries.insert(
            (*head).to_string(),
            Entry::Dir {
                meta: child_meta,
                node: new_child,
            },
        );
        Ok(self.store(node))
    }

    /// Remove `path`. Returns `NotFound` if it does not exist.
    pub fn remove(&mut self, root: &StateNodeId, path: &RepoPath) -> Result<StateNodeId, SfsError> {
        let comps = components(path)?;
        self.remove_at(root, &comps, path)
    }

    fn remove_at(
        &mut self,
        node_id: &StateNodeId,
        comps: &[&str],
        full: &RepoPath,
    ) -> Result<StateNodeId, SfsError> {
        let mut node = self.load(node_id)?;
        let (head, rest) = comps
            .split_first()
            .expect("path has at least one component");

        if rest.is_empty() {
            if node.entries.remove(*head).is_none() {
                return Err(SfsError::NotFound(full.to_string()));
            }
            return Ok(self.store(node));
        }

        let (child_id, child_meta) = match node.entries.get(*head) {
            Some(Entry::Dir { meta, node }) => (node.clone(), *meta),
            Some(_) => return Err(SfsError::NotADirectory((*head).to_string())),
            None => return Err(SfsError::NotFound(full.to_string())),
        };
        let new_child = self.remove_at(&child_id, rest, full)?;
        node.entries.insert(
            (*head).to_string(),
            Entry::Dir {
                meta: child_meta,
                node: new_child,
            },
        );
        Ok(self.store(node))
    }
}

/// Look up one path. `None` when absent.
pub fn get<S: NodeSource>(
    source: &S,
    root: &StateNodeId,
    path: &RepoPath,
) -> Result<Option<Entry>, SfsError> {
    if path.is_root() {
        return Ok(Some(Entry::Dir {
            meta: Meta::dir(),
            node: root.clone(),
        }));
    }
    let comps = components(path)?;
    let mut node = resolve(source, root)?;
    for (i, comp) in comps.iter().enumerate() {
        let Some(entry) = node.entries.get(*comp) else {
            return Ok(None);
        };
        if i + 1 == comps.len() {
            return Ok(Some(entry.clone()));
        }
        match entry {
            Entry::Dir { node: child, .. } => node = resolve(source, child)?,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// List a directory's immediate children, ordered by name.
pub fn readdir<S: NodeSource>(
    source: &S,
    root: &StateNodeId,
    path: &RepoPath,
) -> Result<Vec<(String, Entry)>, SfsError> {
    let node_id = if path.is_root() {
        root.clone()
    } else {
        match get(source, root, path)? {
            Some(Entry::Dir { node, .. }) => node,
            Some(_) => return Err(SfsError::NotADirectory(path.to_string())),
            None => return Err(SfsError::NotFound(path.to_string())),
        }
    };
    let node = resolve(source, &node_id)?;
    Ok(node.entries.into_iter().collect())
}

/// Every path in the tree, ordered, with its entry. Directories are included.
pub fn walk<S: NodeSource>(
    source: &S,
    root: &StateNodeId,
) -> Result<Vec<(RepoPath, Entry)>, SfsError> {
    let mut out = Vec::new();
    walk_into(source, root, &RepoPath::root(), &mut out)?;
    Ok(out)
}

fn walk_into<S: NodeSource>(
    source: &S,
    node_id: &StateNodeId,
    prefix: &RepoPath,
    out: &mut Vec<(RepoPath, Entry)>,
) -> Result<(), SfsError> {
    let node = resolve(source, node_id)?;
    for (name, entry) in node.entries {
        let path = prefix.join(&name)?;
        out.push((path.clone(), entry.clone()));
        if let Entry::Dir { node, .. } = entry {
            walk_into(source, &node, &path, out)?;
        }
    }
    Ok(())
}

/// A single difference between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added(RepoPath, Entry),
    Removed(RepoPath, Entry),
    Modified {
        path: RepoPath,
        before: Entry,
        after: Entry,
    },
}

impl Change {
    pub fn path(&self) -> &RepoPath {
        match self {
            Change::Added(p, _) | Change::Removed(p, _) => p,
            Change::Modified { path, .. } => path,
        }
    }
}

/// Diff two roots. Subtrees whose digests match are skipped without being read, so the cost is
/// proportional to the difference rather than to the size of either tree.
pub fn diff<S: NodeSource>(
    source: &S,
    before: &StateNodeId,
    after: &StateNodeId,
) -> Result<Vec<Change>, SfsError> {
    let mut out = Vec::new();
    diff_into(source, before, after, &RepoPath::root(), &mut out)?;
    Ok(out)
}

fn diff_into<S: NodeSource>(
    source: &S,
    before: &StateNodeId,
    after: &StateNodeId,
    prefix: &RepoPath,
    out: &mut Vec<Change>,
) -> Result<(), SfsError> {
    if before == after {
        return Ok(());
    }
    let old = resolve(source, before)?;
    let new = resolve(source, after)?;

    for (name, old_entry) in &old.entries {
        let path = prefix.join(name)?;
        match new.entries.get(name) {
            None => {
                out.push(Change::Removed(path.clone(), old_entry.clone()));
                if let Entry::Dir { node, .. } = old_entry {
                    let mut sub = Vec::new();
                    walk_into(source, node, &path, &mut sub)?;
                    out.extend(sub.into_iter().map(|(p, e)| Change::Removed(p, e)));
                }
            }
            Some(new_entry) if new_entry == old_entry => {}
            Some(new_entry) => match (old_entry, new_entry) {
                (Entry::Dir { node: a, .. }, Entry::Dir { node: b, .. }) => {
                    diff_into(source, a, b, &path, out)?;
                }
                _ => out.push(Change::Modified {
                    path,
                    before: old_entry.clone(),
                    after: new_entry.clone(),
                }),
            },
        }
    }

    for (name, new_entry) in &new.entries {
        if old.entries.contains_key(name) {
            continue;
        }
        let path = prefix.join(name)?;
        out.push(Change::Added(path.clone(), new_entry.clone()));
        if let Entry::Dir { node, .. } = new_entry {
            let mut sub = Vec::new();
            walk_into(source, node, &path, &mut sub)?;
            out.extend(sub.into_iter().map(|(p, e)| Change::Added(p, e)));
        }
    }
    Ok(())
}

fn components(path: &RepoPath) -> Result<Vec<&str>, SfsError> {
    if path.is_root() {
        return Err(SfsError::InvalidPath(
            "cannot address the root entry".into(),
        ));
    }
    Ok(path.as_str().trim_start_matches('/').split('/').collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealfs_types::canonical::chunk_digest;
    use surrealfs_types::ChunkDigest;

    fn file(bytes: &[u8]) -> Entry {
        Entry::File {
            meta: Meta::file(),
            size: bytes.len() as u64,
            links: Vec::new(),
            extents: vec![Extent {
                file_offset: 0,
                length: bytes.len() as u64,
                chunk: ChunkDigest(chunk_digest(bytes)),
            }],
        }
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::parse(s).unwrap()
    }

    /// Build a tree, committing the writer's nodes into `mem`.
    fn build(mem: &mut MemNodes, root: &StateNodeId, edits: &[(&str, Entry)]) -> StateNodeId {
        let mut root = root.clone();
        let mut writer = TreeWriter::new(&*mem);
        for (path, entry) in edits {
            root = writer.insert(&root, &p(path), entry.clone()).unwrap();
        }
        mem.insert_all(writer.into_new_nodes());
        root
    }

    #[test]
    fn insert_lookup_readdir() {
        let mut mem = MemNodes::default();
        let root = build(
            &mut mem,
            &empty_root(),
            &[("/a/b/c.txt", file(b"hi")), ("/a/d.txt", file(b"yo"))],
        );

        assert_eq!(
            get(&mem, &root, &p("/a/b/c.txt")).unwrap(),
            Some(file(b"hi"))
        );
        assert_eq!(get(&mem, &root, &p("/nope")).unwrap(), None);

        let names: Vec<_> = readdir(&mem, &root, &p("/a"))
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["b".to_string(), "d.txt".to_string()]);
    }

    #[test]
    fn equal_content_gives_equal_root_regardless_of_order() {
        let mut a = MemNodes::default();
        let ra = build(
            &mut a,
            &empty_root(),
            &[("/x/1", file(b"one")), ("/y/2", file(b"two"))],
        );
        let mut b = MemNodes::default();
        let rb = build(
            &mut b,
            &empty_root(),
            &[("/y/2", file(b"two")), ("/x/1", file(b"one"))],
        );
        assert_eq!(ra, rb, "roots must be a pure function of content");
    }

    /// The property M1a exists for: a commit persists nodes proportional to its change set,
    /// not to the size of the tree.
    #[test]
    fn write_cost_is_proportional_to_change_not_tree_size() {
        let mut mem = MemNodes::default();
        let mut root = empty_root();
        let edits: Vec<(String, Entry)> = (0..1000)
            .map(|i| {
                (
                    format!("/src/mod{}/file.rs", i),
                    file(format!("v{i}").as_bytes()),
                )
            })
            .collect();
        {
            let mut writer = TreeWriter::new(&mem);
            for (path, entry) in &edits {
                root = writer.insert(&root, &p(path), entry.clone()).unwrap();
            }
            mem.insert_all(writer.into_new_nodes());
        }
        assert_eq!(walk(&mem, &root).unwrap().len(), 1000 + 1001);

        // Touch exactly one file in a 1000-file tree.
        let mut writer = TreeWriter::new(&mem);
        let new_root = writer
            .insert(&root, &p("/src/mod500/file.rs"), file(b"changed"))
            .unwrap();
        let written = writer.new_node_count();
        assert!(
            written <= 3,
            "one file in a 1000-file tree rewrote {written} nodes; expected the root, /src \
             and /src/mod500 only"
        );
        assert_ne!(new_root, root);
    }

    #[test]
    fn diff_reports_add_modify_remove_and_skips_equal_subtrees() {
        let mut mem = MemNodes::default();
        let base = build(
            &mut mem,
            &empty_root(),
            &[
                ("/keep/untouched.txt", file(b"same")),
                ("/edit/target.txt", file(b"before")),
                ("/gone/old.txt", file(b"bye")),
            ],
        );
        let next = build(
            &mut mem,
            &base,
            &[
                ("/edit/target.txt", file(b"after")),
                ("/new/fresh.txt", file(b"hello")),
            ],
        );
        let next = {
            let mut writer = TreeWriter::new(&mem);
            let r = writer.remove(&next, &p("/gone/old.txt")).unwrap();
            mem.insert_all(writer.into_new_nodes());
            r
        };

        let changes = diff(&mem, &base, &next).unwrap();
        let mut described: Vec<String> = changes
            .iter()
            .map(|c| match c {
                Change::Added(p, _) => format!("A {p}"),
                Change::Removed(p, _) => format!("D {p}"),
                Change::Modified { path, .. } => format!("M {path}"),
            })
            .collect();
        described.sort();
        assert_eq!(
            described,
            vec![
                "A /new".to_string(),
                "A /new/fresh.txt".to_string(),
                "D /gone/old.txt".to_string(),
                "M /edit/target.txt".to_string(),
            ]
        );
        assert!(
            !described.iter().any(|d| d.contains("/keep")),
            "an unchanged subtree must not appear in the diff"
        );
    }

    #[test]
    fn removing_the_last_entry_returns_to_the_empty_root() {
        let mut mem = MemNodes::default();
        let root = build(&mut mem, &empty_root(), &[("/solo.txt", file(b"x"))]);
        let mut writer = TreeWriter::new(&mem);
        let back = writer.remove(&root, &p("/solo.txt")).unwrap();
        assert_eq!(back, empty_root());
        assert!(matches!(
            writer.remove(&root, &p("/missing.txt")),
            Err(SfsError::NotFound(_))
        ));
    }
}
