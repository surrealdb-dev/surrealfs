//! Pure reference state machine.
//!
//! An independent, deliberately naive implementation of SurrealFS filesystem/KV semantics
//! used to cross-check the kernel and store: generated command sequences are applied to
//! both this model and the real stack, and the resulting state roots must match.
//!
//! The model keeps flat maps and rebuilds the whole tree from scratch on every root
//! computation. That is exactly what the real implementation must *not* do, which is what
//! makes the cross-check meaningful: two independent routes to the same digest.
//!
//! This crate must never depend on the database or the kernel.

use std::collections::BTreeMap;

use surrealfs_content::tree::{self, Entry, MemNodes, Meta, TreeWriter};
use surrealfs_content::{chunk_bytes, CHUNK_SIZE};
use surrealfs_types::state::{kv_digest, root_digest, KvMap, Mutation};
use surrealfs_types::{ChunkDigest, RepoPath, SfsError, StateRootId};

/// In-memory reference filesystem and KV state, holding full content bytes.
#[derive(Debug, Clone, Default)]
pub struct RefModel {
    files: BTreeMap<RepoPath, Vec<u8>>,
    dirs: BTreeMap<RepoPath, ()>,
    symlinks: BTreeMap<RepoPath, String>,
    /// Only paths whose metadata differs from the default; absent means the default applies.
    meta: BTreeMap<RepoPath, Meta>,
    /// Hard-link groups, keyed by every member path. Absent means a single link.
    links: BTreeMap<RepoPath, Vec<RepoPath>>,
    kv: BTreeMap<(String, String), Vec<u8>>,
}

impl RefModel {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_parents(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        for anc in path.ancestors() {
            if anc.is_root() {
                continue;
            }
            if self.files.contains_key(&anc) || self.symlinks.contains_key(&anc) {
                return Err(SfsError::NotADirectory(anc.to_string()));
            }
            self.dirs.entry(anc).or_default();
        }
        Ok(())
    }

    pub fn write_file(&mut self, path: &RepoPath, bytes: &[u8]) -> Result<(), SfsError> {
        if path.is_root() || self.dirs.contains_key(path) {
            return Err(SfsError::IsADirectory(path.to_string()));
        }
        self.ensure_parents(path)?;
        let group = self.links.get(path).cloned().unwrap_or_default();
        if group.is_empty() {
            self.files.insert(path.clone(), bytes.to_vec());
        } else {
            for member in group {
                self.files.insert(member, bytes.to_vec());
            }
        }
        Ok(())
    }

    pub fn mkdir(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        if path.is_root() {
            return Ok(());
        }
        if self.files.contains_key(path) {
            return Err(SfsError::AlreadyExists(path.to_string()));
        }
        self.ensure_parents(path)?;
        self.dirs.insert(path.clone(), ());
        Ok(())
    }

    pub fn unlink(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        if self.dirs.contains_key(path) {
            return Err(SfsError::IsADirectory(path.to_string()));
        }
        if self.files.remove(path).is_none() {
            return Err(SfsError::NotFound(path.to_string()));
        }
        self.meta.remove(path);
        if let Some(group) = self.links.remove(path) {
            let remaining: Vec<RepoPath> = group.into_iter().filter(|p| p != path).collect();
            for member in &remaining {
                if remaining.len() > 1 {
                    self.links.insert(member.clone(), remaining.clone());
                } else {
                    self.links.remove(member);
                }
            }
        }
        Ok(())
    }

    pub fn rmdir(&mut self, path: &RepoPath) -> Result<(), SfsError> {
        if !self.dirs.contains_key(path) {
            return Err(SfsError::NotFound(path.to_string()));
        }
        let has_children = self
            .files
            .keys()
            .chain(self.dirs.keys())
            .any(|p| p != path && p.starts_with(path));
        if has_children {
            return Err(SfsError::DirectoryNotEmpty(path.to_string()));
        }
        self.dirs.remove(path);
        Ok(())
    }

    pub fn symlink(&mut self, path: &RepoPath, target: &str) -> Result<(), SfsError> {
        if self.files.contains_key(path) || self.dirs.contains_key(path) {
            return Err(SfsError::AlreadyExists(path.to_string()));
        }
        self.ensure_parents(path)?;
        self.symlinks.insert(path.clone(), target.to_string());
        Ok(())
    }

    /// A second name for an existing file; both then refer to one file.
    pub fn link(&mut self, existing: &RepoPath, new: &RepoPath) -> Result<(), SfsError> {
        if !self.files.contains_key(existing) {
            return Err(SfsError::NotFound(existing.to_string()));
        }
        if self.files.contains_key(new) || self.dirs.contains_key(new) {
            return Err(SfsError::AlreadyExists(new.to_string()));
        }
        self.ensure_parents(new)?;
        let bytes = self.files[existing].clone();
        self.files.insert(new.clone(), bytes);

        let mut group = self.links.get(existing).cloned().unwrap_or_default();
        if group.is_empty() {
            group.push(existing.clone());
        }
        group.push(new.clone());
        group.sort();
        group.dedup();
        for member in &group {
            self.links.insert(member.clone(), group.clone());
        }
        Ok(())
    }

    pub fn set_meta(&mut self, path: &RepoPath, mode: u32, uid: u32, gid: u32) {
        self.meta.insert(path.clone(), Meta { mode, uid, gid });
    }

    /// Move a path and everything under it, carrying metadata along.
    pub fn rename(&mut self, from: &RepoPath, to: &RepoPath) -> Result<(), SfsError> {
        if from == to {
            return Ok(());
        }
        self.ensure_parents(to)?;
        let moved: Vec<RepoPath> = self
            .files
            .keys()
            .chain(self.dirs.keys())
            .chain(self.symlinks.keys())
            .filter(|p| *p == from || p.starts_with(from))
            .cloned()
            .collect();
        if moved.is_empty() {
            return Err(SfsError::NotFound(from.to_string()));
        }
        for old in moved {
            let suffix = old.as_str()[from.as_str().len()..].to_string();
            let new = RepoPath::parse(&format!("{}{suffix}", to.as_str()))?;
            if let Some(bytes) = self.files.remove(&old) {
                self.files.insert(new.clone(), bytes);
            }
            if self.dirs.remove(&old).is_some() {
                self.dirs.insert(new.clone(), ());
            }
            if let Some(target) = self.symlinks.remove(&old) {
                self.symlinks.insert(new.clone(), target);
            }
            if let Some(meta) = self.meta.remove(&old) {
                self.meta.insert(new, meta);
            }
        }
        Ok(())
    }

    pub fn kv_set(&mut self, ns: &str, key: &str, value: &[u8]) {
        self.kv
            .insert((ns.to_string(), key.to_string()), value.to_vec());
    }

    pub fn kv_delete(&mut self, ns: &str, key: &str) -> Result<(), SfsError> {
        self.kv
            .remove(&(ns.to_string(), key.to_string()))
            .map(|_| ())
            .ok_or_else(|| SfsError::NotFound(format!("kv {ns}/{key}")))
    }

    pub fn read_file(&self, path: &RepoPath) -> Option<&[u8]> {
        self.files.get(path).map(|v| v.as_slice())
    }

    pub fn kv_get(&self, ns: &str, key: &str) -> Option<&[u8]> {
        self.kv
            .get(&(ns.to_string(), key.to_string()))
            .map(|v| v.as_slice())
    }

    /// Apply a recorded mutation. Content comes from the caller because mutations carry
    /// chunk digests rather than bytes.
    pub fn apply(&mut self, m: &Mutation, content: Option<&[u8]>) -> Result<(), SfsError> {
        match m {
            Mutation::MkDir { path } => self.mkdir(path),
            Mutation::WriteFile { path, .. } => {
                self.write_file(path, content.expect("WriteFile needs content"))
            }
            Mutation::Unlink { path } => self.unlink(path),
            Mutation::RmDir { path } => self.rmdir(path),
            Mutation::KvSet { namespace, key, .. } => {
                self.kv_set(namespace, key, content.expect("KvSet needs content"));
                Ok(())
            }
            Mutation::KvDelete { namespace, key } => self.kv_delete(namespace, key),
            Mutation::Rename { from, to } => self.rename(from, to),
            Mutation::Link { from, to } => self.link(from, to),
            Mutation::Symlink { path, target } => self.symlink(path, target),
            Mutation::SetMeta {
                path,
                mode,
                uid,
                gid,
            } => {
                self.set_meta(path, *mode, *uid, *gid);
                Ok(())
            }
        }
    }

    /// The KV half of the root.
    pub fn kv_map(&self) -> KvMap {
        self.kv
            .iter()
            .map(|((ns, key), value)| {
                (
                    (ns.clone(), key.clone()),
                    ChunkDigest(surrealfs_types::canonical::chunk_digest(value)),
                )
            })
            .collect()
    }

    /// Rebuild the namespace tree from scratch and return its root.
    pub fn namespace_root(&self) -> Result<surrealfs_types::StateNodeId, SfsError> {
        let mem = MemNodes::default();
        let mut writer = TreeWriter::new(&mem);
        let mut root = tree::empty_root();

        for path in self.dirs.keys() {
            root = writer.insert(
                &root,
                path,
                Entry::Dir {
                    meta: self.meta.get(path).copied().unwrap_or_else(Meta::dir),
                    node: tree::empty_root(),
                },
            )?;
        }
        for (path, target) in &self.symlinks {
            root = writer.insert(
                &root,
                path,
                Entry::Symlink {
                    meta: self.meta.get(path).copied().unwrap_or_else(Meta::file),
                    target: target.clone(),
                },
            )?;
        }
        for (path, bytes) in &self.files {
            debug_assert_eq!(
                CHUNK_SIZE,
                256 * 1024,
                "model mirrors the content chunk rule"
            );
            let (extents, _) = chunk_bytes(bytes);
            root = writer.insert(
                &root,
                path,
                Entry::File {
                    meta: self.meta.get(path).copied().unwrap_or_else(Meta::file),
                    size: bytes.len() as u64,
                    extents,
                    links: self.links.get(path).cloned().unwrap_or_default(),
                },
            )?;
        }
        Ok(root)
    }

    pub fn root_digest(&self) -> Result<StateRootId, SfsError> {
        Ok(root_digest(
            &self.namespace_root()?,
            &kv_digest(&self.kv_map()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::parse(s).unwrap()
    }

    #[test]
    fn filesystem_rules() {
        let mut m = RefModel::new();
        let f = p("/a/b/file.txt");
        m.write_file(&f, b"hi").unwrap();
        assert!(m.dirs.contains_key(&p("/a/b")));
        assert!(m.rmdir(&p("/a")).is_err());
        m.unlink(&f).unwrap();
        m.rmdir(&p("/a/b")).unwrap();
        m.rmdir(&p("/a")).unwrap();
        assert_eq!(
            m.root_digest().unwrap(),
            RefModel::new().root_digest().unwrap()
        );
    }

    #[test]
    fn root_reflects_kv() {
        let mut m = RefModel::new();
        let before = m.root_digest().unwrap();
        m.kv_set("app", "k", b"v");
        assert_ne!(m.root_digest().unwrap(), before);
        m.kv_delete("app", "k").unwrap();
        assert_eq!(m.root_digest().unwrap(), before);
    }

    /// Insertion order must not affect the root: it is a function of content only.
    #[test]
    fn root_is_order_independent() {
        let mut a = RefModel::new();
        a.write_file(&p("/x/1"), b"one").unwrap();
        a.write_file(&p("/y/2"), b"two").unwrap();
        let mut b = RefModel::new();
        b.write_file(&p("/y/2"), b"two").unwrap();
        b.write_file(&p("/x/1"), b"one").unwrap();
        assert_eq!(a.root_digest().unwrap(), b.root_digest().unwrap());
    }
}
