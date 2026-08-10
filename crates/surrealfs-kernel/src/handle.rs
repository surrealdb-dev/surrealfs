//! Open file handles: positional reads and writes against a materialised buffer.
//!
//! A handle owns its bytes. `pread`, `pwrite`, `truncate`, and `fstat` are pure in-memory
//! operations that need no access to the workspace, so a caller can hold a handle and keep
//! using the workspace without fighting the borrow checker; only `open` and `close` touch
//! stored state.
//!
//! This is also where open-unlinked semantics live, and where SurrealFS deliberately differs
//! from the pinned AgentFS baseline. There, removing the last link deletes the inode and its
//! data immediately, so a read through a still-open handle silently returns zero bytes. Here
//! the handle keeps its content, reads keep working, and the data is discarded at close
//! because the file it belonged to is gone — which is what POSIX describes and what the
//! `mkstemp`-then-`unlink` idiom depends on.

use surrealfs_content::tree::{Entry, Meta};

/// Ceiling on the size of one open file.
///
/// An open handle materialises the whole file, so this bounds resident memory per handle. It is
/// a second, independent tier above the workspace staging limit: bytes live here from `write`
/// until `close`, and only then become staged chunks. `dofs` enforces the same 256 MiB per-file
/// bound as `EFBIG`; capping only the lower tier would leave this one free to grow.
pub const MAX_OPEN_FILE_BYTES: u64 = 256 << 20;
use surrealfs_types::{Digest, RepoPath, SfsError};

use crate::Workspace;

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Create the file if it does not exist.
    pub create: bool,
    /// Discard existing content on open.
    pub truncate: bool,
}

impl OpenOptions {
    pub fn read() -> Self {
        OpenOptions::default()
    }

    pub fn create() -> Self {
        OpenOptions {
            create: true,
            truncate: false,
        }
    }

    pub fn create_truncate() -> Self {
        OpenOptions {
            create: true,
            truncate: true,
        }
    }
}

/// Metadata about an open handle, from `fstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub meta: Meta,
}

/// An open file. Dropping one without calling [`Workspace::close`] discards its writes, the
/// same way an unflushed buffer would.
#[derive(Debug, Clone)]
pub struct FileHandle {
    path: RepoPath,
    bytes: Vec<u8>,
    meta: Meta,
    dirty: bool,
    /// What the entry's content hashed to when this handle opened, or `None` if the file did
    /// not exist yet. `close` compares against this to decide whether the file it is writing
    /// back to is still the one it opened.
    opened_digest: Option<Digest>,
}

impl FileHandle {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn fstat(&self) -> FileStat {
        FileStat {
            size: self.bytes.len() as u64,
            meta: self.meta,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Read up to `len` bytes at `offset`. A read past the end returns what exists rather
    /// than an error, matching `pread`.
    pub fn pread(&self, offset: u64, len: usize) -> Vec<u8> {
        let start = (offset as usize).min(self.bytes.len());
        let end = start.saturating_add(len).min(self.bytes.len());
        self.bytes[start..end].to_vec()
    }

    /// Write at `offset`, extending the file and zero-filling any gap.
    pub fn pwrite(&mut self, offset: u64, data: &[u8]) -> Result<usize, SfsError> {
        let start = offset as usize;
        let end = start
            .checked_add(data.len())
            .ok_or_else(|| SfsError::InvalidPath("write offset overflows".into()))?;
        Self::check_size(end)?;
        if end > self.bytes.len() {
            // Writing past the end leaves a hole, which reads back as zeroes.
            self.bytes.resize(end, 0);
        }
        self.bytes[start..end].copy_from_slice(data);
        self.dirty = true;
        Ok(data.len())
    }

    /// Grow or shrink the file; growth zero-fills.
    ///
    /// Growth is checked as well as writes: an open handle materialises its whole file, so
    /// `truncate -s 100G` would otherwise zero-fill a hundred gigabytes of memory before anyone
    /// wrote a byte.
    pub fn truncate(&mut self, size: u64) -> Result<(), SfsError> {
        Self::check_size(size as usize)?;
        self.bytes.resize(size as usize, 0);
        self.dirty = true;
        Ok(())
    }

    fn check_size(size: usize) -> Result<(), SfsError> {
        if size as u64 > MAX_OPEN_FILE_BYTES {
            return Err(SfsError::OverBudget(format!(
                "a single open file is limited to {MAX_OPEN_FILE_BYTES} bytes and this would \
                 reach {size}"
            )));
        }
        Ok(())
    }

    /// The whole file, for callers that want it in one piece.
    pub fn contents(&self) -> &[u8] {
        &self.bytes
    }
}

impl Workspace {
    /// Open a file, materialising its content into the handle.
    pub async fn open(
        &mut self,
        path: &RepoPath,
        opts: OpenOptions,
    ) -> Result<FileHandle, SfsError> {
        self.ensure_open()?;
        match self.stat(path).await? {
            Some(Entry::Dir { .. }) => Err(SfsError::IsADirectory(path.to_string())),
            Some(Entry::Symlink { .. }) => Err(SfsError::InvalidPath(format!(
                "{path} is a symlink; open its target"
            ))),
            Some(entry @ Entry::File { .. }) => {
                let meta = entry.meta();
                let digest = entry.content_digest();
                let bytes = if opts.truncate {
                    Vec::new()
                } else {
                    self.read_file(path).await?
                };
                Ok(FileHandle {
                    path: path.clone(),
                    bytes,
                    meta,
                    dirty: opts.truncate,
                    opened_digest: Some(digest),
                })
            }
            None if opts.create => Ok(FileHandle {
                path: path.clone(),
                bytes: Vec::new(),
                meta: Meta::file(),
                // A newly created file is dirty even if nothing is written, so closing it
                // produces an empty file rather than nothing at all.
                dirty: true,
                opened_digest: None,
            }),
            None => Err(SfsError::NotFound(path.to_string())),
        }
    }

    /// Flush a handle's writes back into the workspace.
    ///
    /// Returns `false` when the writes were discarded because the file the handle opened is
    /// no longer there — either unlinked, or replaced by different content while the handle
    /// was open. In both cases the handle was writing to something that no longer exists, and
    /// silently resurrecting it would be worse than dropping it.
    pub async fn close(&mut self, handle: FileHandle) -> Result<bool, SfsError> {
        self.ensure_open()?;
        if !handle.dirty {
            return Ok(true);
        }
        let current = self.stat(&handle.path).await?;
        let write_back = match (&handle.opened_digest, &current) {
            // Created by this handle and still absent: the ordinary create path.
            (None, None) => true,
            // Created by this handle, but something else claimed the path first. Writing
            // would silently destroy whatever arrived.
            (None, Some(_)) => false,
            // Opened an existing file that has since been unlinked. POSIX keeps the data
            // alive for the descriptor's lifetime and drops it at the last close; that is
            // here, and it is why the handle stayed readable in the meantime.
            (Some(_), None) => false,
            // Opened an existing file: write only if it is still the same file.
            //
            // This deliberately refuses a stale handle that would clobber a file some other
            // writer replaced — an MCP `fs_write` landing while a handle is open, say. A mount
            // must therefore not manufacture a second handle on a path it already has open, or
            // ordinary `fs::write` would be silently discarded here; `SurrealFuse` requests
            // `FUSE_ATOMIC_O_TRUNC` so the kernel truncates through `open` rather than through a
            // separate `setattr` that would need one.
            (Some(opened), Some(entry)) => entry.content_digest() == *opened,
        };
        if !write_back {
            return Ok(false);
        }

        self.write_file(&handle.path, &handle.bytes).await?;
        if handle.meta != Meta::file() {
            self.set_meta(
                &handle.path,
                Some(handle.meta.mode),
                Some(handle.meta.uid),
                Some(handle.meta.gid),
            )
            .await?;
        }
        Ok(true)
    }
}
