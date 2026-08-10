//! The protocol-agnostic half of a mount.
//!
//! FUSE and NFS differ in wire format and in almost nothing else that matters here: both
//! address files by integer inode, both want POSIX attributes, both expect errno. This crate
//! holds that shared translation once, so the two adapters stay thin and cannot drift into
//! being second implementations of the filesystem — which fixed decision 5 forbids.
//!
//! Three decisions are worth reading before the code.
//!
//! **Inode numbers are presentation, not identity.** See [`inode`]. They are allocated per
//! mount and never enter a commit.
//!
//! **A mount never publishes on its own.** Writes stage into one long-lived workspace and stay
//! invisible to every other reader until something explicitly publishes. `close` and `fsync`
//! make staged data consistent; they do not invent a commit. That is fixed decision 9, and it
//! is the difference between a filesystem that records what an agent did and one that records
//! what an agent's editor happened to flush.
//!
//! **Timestamps come from provenance.** `Meta` carries mode, uid and gid but no times,
//! deliberately: a clock in the state root would make two identical trees written at different
//! moments hash differently, breaking reproducibility and the reference-model cross-check. But
//! `getattr` needs an mtime, and build tools depend on it. The resolution is that a path's
//! mtime *is* the time of the commit that last wrote it — which the mutation log already
//! records per path. Timestamps are therefore derived, never stored, and they mean something
//! more precise than a filesystem's usual mtime: the moment the change was published.

pub mod errno;
pub mod inode;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use surrealfs_content::tree::Entry;
use surrealfs_kernel::{Kernel, OpenOptions, Workspace};
use surrealfs_types::{RepoPath, SfsError};
use tokio::sync::Mutex;

pub use errno::{errno_for, to_errno};
pub use inode::{InodeTable, ROOT_INODE};

/// What a file is, in the terms a mount protocol understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    Regular,
    Symlink,
}

/// POSIX attributes for one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attributes {
    pub inode: u64,
    pub kind: FileKind,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Names referring to this file. Above one only for hard links.
    pub nlink: u32,
    /// When the commit that last wrote this path was published. Falls back to the mount's
    /// start time for paths with no recorded mutation, such as an implicitly created parent.
    pub mtime: SystemTime,
}

/// One directory entry in a `readdir` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub inode: u64,
    pub name: String,
    pub kind: FileKind,
}

/// An open file, as the protocol sees it.
pub type FileHandleId = u64;

/// The shared mount logic. A FUSE or NFS adapter owns one of these and does nothing but
/// translate its own wire format to and from these calls.
pub struct MountKernel {
    kernel: Arc<Kernel>,
    inodes: InodeTable,
    /// The single workspace every write stages into, for the life of the mount.
    workspace: Mutex<Workspace>,
    handles: Mutex<Handles>,
    /// Derived per-path publication times, so `getattr` does not query provenance every call.
    mtimes: Mutex<HashMap<RepoPath, SystemTime>>,
    mounted_at: SystemTime,
}

#[derive(Default)]
struct Handles {
    open: HashMap<FileHandleId, surrealfs_kernel::FileHandle>,
    next: FileHandleId,
}

impl MountKernel {
    pub async fn new(kernel: Arc<Kernel>) -> Result<Self, SfsError> {
        let workspace = kernel.workspace().await?;
        Ok(MountKernel {
            kernel,
            inodes: InodeTable::new(),
            workspace: Mutex::new(workspace),
            handles: Mutex::new(Handles::default()),
            mtimes: Mutex::new(HashMap::new()),
            mounted_at: SystemTime::now(),
        })
    }

    pub fn inodes(&self) -> &InodeTable {
        &self.inodes
    }

    fn path(&self, inode: u64) -> Result<RepoPath, SfsError> {
        self.inodes
            .path_for(inode)
            .ok_or_else(|| SfsError::NotFound(format!("inode {inode}")))
    }

    /// When the commit that last wrote this path was published.
    async fn mtime_of(&self, path: &RepoPath) -> SystemTime {
        if let Some(known) = self.mtimes.lock().await.get(path) {
            return *known;
        }
        let derived = match self.kernel.explain(path.as_str(), 1).await {
            Ok(history) => history
                .first()
                .and_then(|step| surrealfs_types::time::parse_rfc3339(&step.committed_at))
                .unwrap_or(self.mounted_at),
            // A mount must keep serving even if provenance is unavailable; a wrong timestamp
            // is a far smaller failure than a failed stat.
            Err(_) => self.mounted_at,
        };
        self.mtimes.lock().await.insert(path.clone(), derived);
        derived
    }

    fn attributes_of(&self, path: &RepoPath, entry: &Entry, mtime: SystemTime) -> Attributes {
        let meta = entry.meta();
        let (kind, size) = match entry {
            Entry::Dir { .. } => (FileKind::Directory, 0),
            Entry::File { size, .. } => (FileKind::Regular, *size),
            Entry::Symlink { target, .. } => (FileKind::Symlink, target.len() as u64),
        };
        Attributes {
            inode: self.inodes.inode_for(path),
            kind,
            size,
            mode: meta.mode,
            uid: meta.uid,
            gid: meta.gid,
            nlink: entry.link_count() as u32,
            mtime,
        }
    }

    // ---- reads ----

    /// Resolve a name inside a directory, as FUSE `lookup` and NFS `LOOKUP` do.
    pub async fn lookup(&self, parent: u64, name: &str) -> Result<Attributes, SfsError> {
        let path = self.path(parent)?.join(name)?;
        self.getattr_path(&path).await
    }

    pub async fn getattr(&self, inode: u64) -> Result<Attributes, SfsError> {
        let path = self.path(inode)?;
        self.getattr_path(&path).await
    }

    async fn getattr_path(&self, path: &RepoPath) -> Result<Attributes, SfsError> {
        // The root has no entry of its own; it is the tree.
        if path.is_root() {
            return Ok(Attributes {
                inode: ROOT_INODE,
                kind: FileKind::Directory,
                size: 0,
                mode: 0o755,
                uid: 0,
                gid: 0,
                nlink: 2,
                mtime: self.mounted_at,
            });
        }
        let entry = self
            .workspace
            .lock()
            .await
            .stat(path)
            .await?
            .ok_or_else(|| SfsError::NotFound(path.to_string()))?;
        let mtime = self.mtime_of(path).await;
        Ok(self.attributes_of(path, &entry, mtime))
    }

    pub async fn readdir(&self, inode: u64) -> Result<Vec<DirEntry>, SfsError> {
        let path = self.path(inode)?;
        let listed = self.workspace.lock().await.list_dir(&path).await?;
        let mut out = Vec::with_capacity(listed.len());
        for entry in listed {
            let child = path.join(&entry.name)?;
            out.push(DirEntry {
                inode: self.inodes.inode_for(&child),
                kind: if entry.is_dir {
                    FileKind::Directory
                } else {
                    FileKind::Regular
                },
                name: entry.name,
            });
        }
        Ok(out)
    }

    pub async fn readlink(&self, inode: u64) -> Result<String, SfsError> {
        let path = self.path(inode)?;
        self.workspace.lock().await.readlink(&path).await
    }

    // ---- open files ----

    /// Open an existing file, optionally discarding its content first.
    ///
    /// `truncate` is what `O_TRUNC` means, and the name matters: this parameter was once called
    /// `create` and mapped to `OpenOptions::create()`, which sets `truncate: false`. The adapter
    /// computed the flag correctly and the mount then dropped it, so `open(O_TRUNC)` never
    /// truncated. Overwriting with *longer* content hid it completely — the new bytes covered the
    /// old ones — and every test wrote longer content until one wrote shorter.
    pub async fn open(&self, inode: u64, truncate: bool) -> Result<FileHandleId, SfsError> {
        let path = self.path(inode)?;
        let opts = if truncate {
            OpenOptions::create_truncate()
        } else {
            OpenOptions::read()
        };
        let handle = self.workspace.lock().await.open(&path, opts).await?;
        let mut handles = self.handles.lock().await;
        handles.next += 1;
        let id = handles.next;
        handles.open.insert(id, handle);
        Ok(id)
    }

    /// Create a file and return it already open, as FUSE `create` expects.
    pub async fn create(
        &self,
        parent: u64,
        name: &str,
    ) -> Result<(Attributes, FileHandleId), SfsError> {
        let path = self.path(parent)?.join(name)?;
        {
            let mut ws = self.workspace.lock().await;
            let handle = ws.open(&path, OpenOptions::create_truncate()).await?;
            ws.close(handle).await?;
        }
        self.touch(&path).await;
        let attrs = self.getattr_path(&path).await?;
        let fh = self.open(attrs.inode, true).await?;
        Ok((attrs, fh))
    }

    pub async fn read(
        &self,
        fh: FileHandleId,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, SfsError> {
        let handles = self.handles.lock().await;
        let handle = handles
            .open
            .get(&fh)
            .ok_or_else(|| SfsError::NotFound(format!("file handle {fh}")))?;
        Ok(handle.pread(offset, len))
    }

    pub async fn write(
        &self,
        fh: FileHandleId,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, SfsError> {
        let mut handles = self.handles.lock().await;
        let handle = handles
            .open
            .get_mut(&fh)
            .ok_or_else(|| SfsError::NotFound(format!("file handle {fh}")))?;
        handle.pwrite(offset, data)
    }

    pub async fn truncate(&self, fh: FileHandleId, size: u64) -> Result<(), SfsError> {
        let mut handles = self.handles.lock().await;
        let handle = handles
            .open
            .get_mut(&fh)
            .ok_or_else(|| SfsError::NotFound(format!("file handle {fh}")))?;
        handle.truncate(size)?;
        Ok(())
    }

    /// Flush a handle's writes into the workspace.
    ///
    /// This is where `close` and `fsync` land, and it is deliberately *not* a commit: the data
    /// becomes visible to this mount and stays invisible to everyone else until an explicit
    /// publication. See fixed decision 9.
    pub async fn release(&self, fh: FileHandleId) -> Result<bool, SfsError> {
        let handle = self.handles.lock().await.open.remove(&fh);
        let Some(handle) = handle else {
            return Err(SfsError::NotFound(format!("file handle {fh}")));
        };
        let path = handle.path().clone();
        let written = self.workspace.lock().await.close(handle).await?;
        if written {
            self.touch(&path).await;
        }
        Ok(written)
    }

    // ---- namespace mutations ----

    pub async fn mkdir(&self, parent: u64, name: &str) -> Result<Attributes, SfsError> {
        let path = self.path(parent)?.join(name)?;
        self.workspace.lock().await.mkdir(&path).await?;
        self.touch(&path).await;
        self.getattr_path(&path).await
    }

    pub async fn unlink(&self, parent: u64, name: &str) -> Result<(), SfsError> {
        let path = self.path(parent)?.join(name)?;
        self.workspace.lock().await.unlink(&path).await?;
        self.inodes.forget(&path);
        self.mtimes.lock().await.remove(&path);
        Ok(())
    }

    pub async fn rmdir(&self, parent: u64, name: &str) -> Result<(), SfsError> {
        let path = self.path(parent)?.join(name)?;
        self.workspace.lock().await.rmdir(&path).await?;
        self.inodes.forget(&path);
        self.mtimes.lock().await.remove(&path);
        Ok(())
    }

    pub async fn rename(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<(), SfsError> {
        let from = self.path(parent)?.join(name)?;
        let to = self.path(new_parent)?.join(new_name)?;
        self.workspace.lock().await.rename(&from, &to).await?;
        self.inodes.rename(&from, &to);
        self.mtimes.lock().await.remove(&from);
        self.touch(&to).await;
        Ok(())
    }

    pub async fn symlink(
        &self,
        parent: u64,
        name: &str,
        target: &str,
    ) -> Result<Attributes, SfsError> {
        let path = self.path(parent)?.join(name)?;
        self.workspace.lock().await.symlink(&path, target).await?;
        self.touch(&path).await;
        self.getattr_path(&path).await
    }

    pub async fn link(
        &self,
        inode: u64,
        new_parent: u64,
        new_name: &str,
    ) -> Result<Attributes, SfsError> {
        let existing = self.path(inode)?;
        let path = self.path(new_parent)?.join(new_name)?;
        self.workspace.lock().await.link(&existing, &path).await?;
        self.touch(&path).await;
        self.getattr_path(&path).await
    }

    pub async fn setattr(
        &self,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<Attributes, SfsError> {
        let path = self.path(inode)?;
        self.workspace
            .lock()
            .await
            .set_meta(&path, mode, uid, gid)
            .await?;
        self.touch(&path).await;
        self.getattr_path(&path).await
    }

    // ---- publication ----

    /// True when the mount has staged anything not yet published.
    pub async fn is_dirty(&self) -> bool {
        self.workspace.lock().await.is_dirty()
    }

    /// Unpublished bytes held in memory, and the ceiling they are measured against.
    ///
    /// A mount never publishes on its own, so this is the number that grows for as long as an
    /// agent session runs. It is exposed rather than kept private because the useful response to
    /// pressure — publish, or abort — is the caller's to make, and by the time a write fails with
    /// `EFBIG` the choice has already been forced.
    pub async fn staged_pressure(&self) -> (u64, u64) {
        let ws = self.workspace.lock().await;
        (ws.staged_bytes(), ws.staged_limit())
    }

    /// Set the ceiling on unpublished bytes for this mount.
    pub async fn set_staged_limit(&self, limit: u64) {
        self.workspace.lock().await.set_staged_limit(limit);
    }

    /// Publish everything staged since the mount started (or since the last publication), and
    /// open a fresh workspace to continue against.
    ///
    /// Nothing calls this implicitly. A mount that is never told to publish stages
    /// indefinitely and the repository never moves, which is the intended behaviour: the
    /// decision to commit belongs to whatever is supervising the agent, not to the agent's
    /// choice of when to flush a buffer.
    pub async fn publish(
        &self,
        message: Option<String>,
    ) -> Result<surrealfs_kernel::CommitReceipt, SfsError> {
        let mut ws = self.workspace.lock().await;
        let receipt = ws.publish(None, message).await?;
        *ws = self.kernel.workspace().await?;
        // Publication times changed; let them be re-derived.
        self.mtimes.lock().await.clear();
        Ok(receipt)
    }

    /// Discard everything staged and start again from the current head.
    pub async fn abort(&self, reason: &str) -> Result<(), SfsError> {
        let mut ws = self.workspace.lock().await;
        ws.abort(reason).await?;
        *ws = self.kernel.workspace().await?;
        self.mtimes.lock().await.clear();
        Ok(())
    }

    /// Record that a path changed now, so its attributes reflect the edit before any
    /// publication has given it a commit time.
    async fn touch(&self, path: &RepoPath) {
        self.mtimes
            .lock()
            .await
            .insert(path.clone(), SystemTime::now());
    }
}
