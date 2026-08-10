//! Inode numbers for mount protocols.
//!
//! FUSE and NFS both address files by integer inode, and SurrealFS deliberately has none: a
//! path *is* the identity, precisely so a state root stays a pure function of content. These
//! two facts have to be reconciled somewhere, and the honest place is here — at the
//! presentation boundary, not in the data model.
//!
//! So inode numbers are allocated by the mount, live for as long as the mount does, and never
//! reach a commit. They are the same class of thing as the resident node cache: a disposable
//! projection over durable truth. Two mounts of the same repository will hand out different
//! numbers for the same file, which is fine — nothing persists them, and nothing compares
//! them across mounts.
//!
//! Renames preserve the number. `rename` moves the mapping and carries the subtree with it, so
//! a client that stats a file, renames it, and stats it again sees the same inode, as it would
//! on a real filesystem.
//!
//! Two limits are worth stating plainly rather than discovering later, and both are shared with
//! ContextFS, which reaches the same position from the same content-addressed premise:
//!
//! - **Numbers do not survive a remount.** They come from a counter that restarts at zero. An
//!   NFS adapter must therefore derive its file handle from the path digest rather than from
//!   the inode number, or an unmount turns a cached handle into silent misdirection.
//! - **A mount is snapshot-isolated.** It never re-reads head, so a rename performed by another
//!   surface is invisible to it until it publishes and rebases.
//!
//! **Which binding the adapter uses decides whether these numbers survive to userspace.**
//! libfuse's *high-level*, path-based API overwrites `st_ino` with its own nodeid unless the
//! mount passes `use_ino` — so a table like this one is computed and then discarded. ContextFS
//! and `dofs` both sit on that API, and only `dofs` sets the option.
//!
//! We use `fuser`, which is a *low-level* binding: it speaks the kernel protocol directly, every
//! reply carries an explicit `FileAttr.ino`, and there is no `use_ino` option because there is no
//! intermediate layer with an opinion. The requirement on the adapter is therefore not a mount
//! flag but a discipline — fill `ino` from this table on every single reply, since a zero or a
//! stale value is now nobody's job to catch.
//!
//! One trap avoided by construction: `dofs` reports `ino: 0` for a file between create and
//! release, so every concurrently-pending new file stats as inode 0 at once and anything doing
//! identity comparison — hardlink detection, `find -samefile`, tar dedup — conflates them. Here
//! the number is allocated at `create`, before any write, so the window does not exist.

use std::collections::HashMap;
use std::sync::Mutex;

use surrealfs_types::RepoPath;

/// FUSE fixes the root at inode 1, and NFS clients assume the same for the mount point.
pub const ROOT_INODE: u64 = 1;

/// A bidirectional, mount-lifetime map between paths and inode numbers.
pub struct InodeTable {
    inner: Mutex<Inner>,
}

struct Inner {
    to_path: HashMap<u64, RepoPath>,
    to_inode: HashMap<RepoPath, u64>,
    next: u64,
}

impl InodeTable {
    pub fn new() -> Self {
        let root = RepoPath::root();
        let mut to_path = HashMap::new();
        let mut to_inode = HashMap::new();
        to_path.insert(ROOT_INODE, root.clone());
        to_inode.insert(root, ROOT_INODE);
        InodeTable {
            inner: Mutex::new(Inner {
                to_path,
                to_inode,
                // 1 is the root; everything else is allocated from 2 upwards.
                next: ROOT_INODE + 1,
            }),
        }
    }

    /// The inode for a path, allocating one on first sight.
    pub fn inode_for(&self, path: &RepoPath) -> u64 {
        let mut inner = self.inner.lock().expect("inode table mutex poisoned");
        if let Some(inode) = inner.to_inode.get(path) {
            return *inode;
        }
        let inode = inner.next;
        inner.next += 1;
        inner.to_path.insert(inode, path.clone());
        inner.to_inode.insert(path.clone(), inode);
        inode
    }

    /// The path an inode refers to, if this mount has issued that number.
    pub fn path_for(&self, inode: u64) -> Option<RepoPath> {
        self.inner
            .lock()
            .expect("inode table mutex poisoned")
            .to_path
            .get(&inode)
            .cloned()
    }

    /// Forget a path's number, as `unlink` and `rmdir` should.
    ///
    /// The number is not recycled. Reuse is how a stale client handle silently starts
    /// referring to a different file, and mount protocols cache aggressively enough that it
    /// would happen.
    pub fn forget(&self, path: &RepoPath) {
        let mut inner = self.inner.lock().expect("inode table mutex poisoned");
        if let Some(inode) = inner.to_inode.remove(path) {
            inner.to_path.remove(&inode);
        }
    }

    /// Move a path's mapping, and every mapping beneath it, to a new prefix.
    ///
    /// Called on rename. The moved entries keep their numbers here, so a rename that a client
    /// follows by path stays coherent within one mount.
    pub fn rename(&self, from: &RepoPath, to: &RepoPath) {
        let mut inner = self.inner.lock().expect("inode table mutex poisoned");
        let moved: Vec<RepoPath> = inner
            .to_inode
            .keys()
            .filter(|p| *p == from || p.starts_with(from))
            .cloned()
            .collect();
        for old in moved {
            let Some(inode) = inner.to_inode.remove(&old) else {
                continue;
            };
            let suffix = old.as_str()[from.as_str().len()..].to_string();
            let Ok(new) = RepoPath::parse(&format!("{}{suffix}", to.as_str())) else {
                // An unrepresentable destination just drops the mapping; the next lookup
                // allocates a fresh number rather than leaving a wrong one in place.
                inner.to_path.remove(&inode);
                continue;
            };
            inner.to_path.insert(inode, new.clone());
            inner.to_inode.insert(new, inode);
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("inode table mutex poisoned")
            .to_inode
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for InodeTable {
    fn default() -> Self {
        InodeTable::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::parse(s).unwrap()
    }

    #[test]
    fn the_root_is_always_inode_one() {
        let table = InodeTable::new();
        assert_eq!(table.inode_for(&RepoPath::root()), ROOT_INODE);
        assert_eq!(table.path_for(ROOT_INODE), Some(RepoPath::root()));
    }

    #[test]
    fn numbers_are_stable_and_bidirectional() {
        let table = InodeTable::new();
        let a = table.inode_for(&p("/src/main.rs"));
        let b = table.inode_for(&p("/src/lib.rs"));

        assert_ne!(a, b);
        assert_eq!(
            table.inode_for(&p("/src/main.rs")),
            a,
            "stable on re-lookup"
        );
        assert_eq!(table.path_for(a), Some(p("/src/main.rs")));
        assert_eq!(table.path_for(b), Some(p("/src/lib.rs")));
        assert_eq!(table.path_for(99_999), None);
    }

    /// Recycling a number would let a cached client handle silently address a different file.
    #[test]
    fn forgotten_numbers_are_never_reused() {
        let table = InodeTable::new();
        let first = table.inode_for(&p("/gone.txt"));
        table.forget(&p("/gone.txt"));
        assert_eq!(table.path_for(first), None);

        let second = table.inode_for(&p("/new.txt"));
        assert_ne!(
            second, first,
            "a fresh path must not inherit a retired number"
        );
    }

    #[test]
    fn rename_moves_the_whole_subtree() {
        let table = InodeTable::new();
        let dir = table.inode_for(&p("/old"));
        let file = table.inode_for(&p("/old/nested/f.txt"));

        table.rename(&p("/old"), &p("/new"));

        assert_eq!(table.path_for(dir), Some(p("/new")));
        assert_eq!(table.path_for(file), Some(p("/new/nested/f.txt")));
        // Looking the moved path up again returns the number it already had.
        assert_eq!(table.inode_for(&p("/new/nested/f.txt")), file);
        // The old name is gone rather than aliasing the moved entry: asking for it again
        // allocates a fresh number instead of handing back the one that moved.
        let revived = table.inode_for(&p("/old"));
        assert_ne!(revived, dir);
        assert_ne!(revived, file);
    }

    /// A rename does not disturb unrelated paths that merely share a name prefix.
    #[test]
    fn rename_does_not_catch_sibling_prefixes() {
        let table = InodeTable::new();
        let sibling = table.inode_for(&p("/olderfile.txt"));
        table.inode_for(&p("/old/f.txt"));

        table.rename(&p("/old"), &p("/new"));

        assert_eq!(
            table.path_for(sibling),
            Some(p("/olderfile.txt")),
            "/olderfile.txt is not inside /old"
        );
    }
}
