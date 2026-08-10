//! FUSE adapter: the Linux wire format over [`MountKernel`].
//!
//! There is deliberately no filesystem logic here. Every semantic question — what a rename does
//! to an inode number, whether closing a file publishes a commit, where an mtime comes from — was
//! answered once in `surrealfs-mount` and is tested there on any platform. This crate translates
//! between that answer and the kernel's protocol, and if it ever starts making decisions of its
//! own, the guarantee that all surfaces agree has already been lost.
//!
//! Two things are worth knowing before reading the code.
//!
//! **The inode discipline.** `fuser` is a *low-level* binding: it speaks the kernel protocol
//! directly, so every reply carries an explicit `FileAttr.ino` and there is no `use_ino` option
//! because no intermediate layer has an opinion to override. libfuse's high-level API is the one
//! that substitutes its own nodeid — that is what ContextFS and `dofs` sit on. Here the
//! requirement is a discipline rather than a flag: fill `ino` from the mount's table on every
//! reply, since a zero or a stale value is now nobody's job to catch.
//!
//! **The attribute TTL is short, on purpose.** AgentFS uses `Duration::MAX`, which tells the
//! kernel to cache attributes and dentries forever and makes any out-of-band change invisible for
//! the life of the mount. A mount here is snapshot-isolated but not inert: publishing rebases it,
//! and a stale cache would show a client the pre-publication tree indefinitely.

#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, OpenFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use surrealfs_mount::{errno_for, Attributes, FileKind, MountKernel};
use surrealfs_types::SfsError;

/// How long the kernel may trust an attribute or dentry before asking again.
///
/// One second rather than `Duration::MAX`: long enough that a build tool statting a tree does not
/// pay a round trip per file, short enough that a publication becomes visible promptly.
const TTL: Duration = Duration::from_secs(1);

/// Generation numbers distinguish reuses of an inode number. We never recycle one, so every
/// generation is zero and stays correct — a client holding an old number gets `ENOENT`, never a
/// different file wearing the same identity.
const GENERATION: Generation = Generation(0);

/// The FUSE filesystem.
pub struct SurrealFuse {
    mount: Arc<MountKernel>,
    /// Used to drive the async kernel from FUSE's synchronous callbacks. The session runs on
    /// plain OS threads, not runtime workers, so blocking on them is sound.
    runtime: tokio::runtime::Handle,
    uid: u32,
    gid: u32,
}

impl SurrealFuse {
    pub fn new(mount: Arc<MountKernel>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            mount,
            runtime,
            // Files carry mode in the state root but not ownership, so the mount presents
            // everything as belonging to whoever is running it. Recording the daemon's ids in a
            // commit would make a tree that fails to reproduce on another machine.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.runtime.block_on(fut)
    }

    fn attr(&self, a: &Attributes) -> FileAttr {
        FileAttr {
            ino: INodeNo(a.inode),
            size: a.size,
            blocks: a.size.div_ceil(512),
            // atime is reported as mtime: the state root has no clock, and tracking access times
            // would mean writing on every read. Mount `noatime` and mean it.
            atime: a.mtime,
            mtime: a.mtime,
            ctime: a.mtime,
            crtime: a.mtime,
            kind: match a.kind {
                FileKind::Directory => FileType::Directory,
                FileKind::Regular => FileType::RegularFile,
                FileKind::Symlink => FileType::Symlink,
            },
            perm: a.mode as u16,
            nlink: a.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

/// Translate a domain error into the `Errno` a reply wants.
fn err(e: SfsError) -> Errno {
    Errno::from_i32(errno_for(&e))
}

/// A name from the kernel is bytes; ours are UTF-8 paths. A non-UTF-8 name cannot name anything
/// in this filesystem, so it is `ENOENT` rather than a lossy conversion that would silently
/// address a different file.
fn name_str(name: &OsStr) -> Result<&str, Errno> {
    name.to_str().ok_or(Errno::ENOENT)
}

/// Run a fallible mount call and reply with the error if it fails.
macro_rules! reply_err {
    ($reply:expr, $result:expr) => {
        match $result {
            Ok(value) => value,
            Err(e) => {
                $reply.error(err(e));
                return;
            }
        }
    };
}

impl fuser::Filesystem for SurrealFuse {
    /// Ask the kernel to truncate through `open` rather than through a separate `setattr`.
    ///
    /// This is load-bearing, not a tuning knob. Without `FUSE_ATOMIC_O_TRUNC`, `open(O_TRUNC)` —
    /// which is what every `fs::write` to an existing file performs — arrives as `open` followed
    /// by a `setattr(size=0)` the adapter can only service by opening a *second* handle on the
    /// same path. Closing that second handle changes the file, and the kernel's stale-handle
    /// protection then refuses the write on the first, so the caller's data disappears with no
    /// error. A real mount reproduced exactly that before this was requested.
    ///
    /// If the kernel refuses the capability, mounting fails rather than proceeding into that
    /// behaviour silently.
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        config
            .add_capabilities(fuser::InitFlags::FUSE_ATOMIC_O_TRUNC)
            .map_err(|missing| {
                std::io::Error::other(format!(
                    "this kernel does not support {missing:?}; without atomic O_TRUNC an \
                     overwrite would be silently discarded"
                ))
            })
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = match name_str(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let attrs = reply_err!(reply, self.block_on(self.mount.lookup(parent.0, name)));
        reply.entry(&TTL, &self.attr(&attrs), GENERATION);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let attrs = reply_err!(reply, self.block_on(self.mount.getattr(ino.0)));
        reply.attr(&TTL, &self.attr(&attrs));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let target = reply_err!(reply, self.block_on(self.mount.readlink(ino.0)));
        reply.data(target.as_bytes());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = reply_err!(reply, self.block_on(self.mount.readdir(ino.0)));

        // `.` and `..` are the kernel's to expect and ours to supply; a directory without them
        // breaks `find`, `du`, and anything walking upward. `..` resolves to the directory itself
        // at the root, matching every other filesystem.
        let mut all: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (ino.0, FileType::Directory, "..".into()),
        ];
        all.extend(entries.into_iter().map(|e| {
            let kind = match e.kind {
                FileKind::Directory => FileType::Directory,
                FileKind::Regular => FileType::RegularFile,
                FileKind::Symlink => FileType::Symlink,
            };
            (e.inode, kind, e.name)
        }));

        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            // The offset handed back is where to resume, so it is the index *after* this entry.
            if reply.add(INodeNo(child), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // `OpenFlags` is a raw `i32` here rather than a bitflags type, so the mask is applied
        // directly. `O_TRUNC` comes from `libc` because its value is platform-defined.
        let truncate = flags.0 & libc::O_TRUNC != 0;
        let fh = reply_err!(reply, self.block_on(self.mount.open(ino.0, truncate)));
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name = match name_str(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let (attrs, fh) = reply_err!(reply, self.block_on(self.mount.create(parent.0, name)));
        // A create carries a mode; apply it rather than silently ignoring the caller's intent.
        let attrs = if mode & 0o777 != 0o644 {
            reply_err!(
                reply,
                self.block_on(
                    self.mount
                        .setattr(attrs.inode, Some(mode & 0o777), None, None)
                )
            )
        } else {
            attrs
        };
        reply.created(
            &TTL,
            &self.attr(&attrs),
            GENERATION,
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let bytes = reply_err!(
            reply,
            self.block_on(self.mount.read(fh.0, offset, size as usize))
        );
        reply.data(&bytes);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let written = reply_err!(reply, self.block_on(self.mount.write(fh.0, offset, data)));
        reply.written(written as u32);
    }

    /// `flush` and `fsync` make staged data consistent. Neither publishes: that is fixed decision
    /// 9, and it is the difference between recording what an agent did and recording whenever its
    /// editor happened to flush a buffer.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply_err!(reply, self.block_on(self.mount.release(fh.0)));
        reply.ok();
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = match name_str(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let attrs = reply_err!(reply, self.block_on(self.mount.mkdir(parent.0, name)));
        reply.entry(&TTL, &self.attr(&attrs), GENERATION);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = match name_str(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        reply_err!(reply, self.block_on(self.mount.unlink(parent.0, name)));
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = match name_str(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        reply_err!(reply, self.block_on(self.mount.rmdir(parent.0, name)));
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (name, newname) = match (name_str(name), name_str(newname)) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return reply.error(Errno::ENOENT),
        };
        reply_err!(
            reply,
            self.block_on(self.mount.rename(parent.0, name, newparent.0, newname))
        );
        reply.ok();
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        link: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let name = match name_str(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let target = match link.to_str() {
            Some(t) => t,
            None => return reply.error(Errno::EINVAL),
        };
        let attrs = reply_err!(
            reply,
            self.block_on(self.mount.symlink(parent.0, name, target))
        );
        reply.entry(&TTL, &self.attr(&attrs), GENERATION);
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let newname = match name_str(newname) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let attrs = reply_err!(
            reply,
            self.block_on(self.mount.link(ino.0, newparent.0, newname))
        );
        reply.entry(&TTL, &self.attr(&attrs), GENERATION);
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // A truncate arrives here as a size change, and needs an open handle to act on.
        if let Some(size) = size {
            match fh {
                Some(fh) => reply_err!(reply, self.block_on(self.mount.truncate(fh.0, size))),
                None => {
                    let fh = reply_err!(reply, self.block_on(self.mount.open(ino.0, false)));
                    reply_err!(reply, self.block_on(self.mount.truncate(fh, size)));
                    reply_err!(reply, self.block_on(self.mount.release(fh)));
                }
            }
        }
        // Timestamps are deliberately not settable: the state root carries no clock, so a stored
        // time would break reproducibility. `utimens` is accepted and forgotten rather than
        // refused, because build tools call it incidentally and failing would break them.
        let attrs = reply_err!(
            reply,
            self.block_on(self.mount.setattr(ino.0, mode, uid, gid))
        );
        reply.attr(&TTL, &self.attr(&attrs));
    }
}
