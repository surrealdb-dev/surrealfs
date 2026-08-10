//! The shared mount layer, exercised as a FUSE or NFS adapter would drive it.
//!
//! These are the semantics both adapters inherit, so they are tested once here rather than
//! twice against two wire formats — and they are testable on any platform, unlike the adapters
//! themselves.

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_mount::{errno_for, FileKind, MountKernel, ROOT_INODE};
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepositoryId, SfsError};

async fn mount() -> (Arc<Kernel>, MountKernel) {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    let kernel = Arc::new(
        Kernel::open(store, RepositoryId::parse("mount-test").unwrap())
            .await
            .unwrap(),
    );
    let mount = MountKernel::new(kernel.clone()).await.unwrap();
    (kernel, mount)
}

#[tokio::test]
async fn the_namespace_is_navigable_by_inode() {
    let (_k, m) = mount().await;

    let dir = m.mkdir(ROOT_INODE, "src").await.unwrap();
    assert_eq!(dir.kind, FileKind::Directory);
    assert_ne!(dir.inode, ROOT_INODE);

    let (file, fh) = m.create(dir.inode, "main.rs").await.unwrap();
    m.write(fh, 0, b"fn main() {}").await.unwrap();
    m.release(fh).await.unwrap();

    // lookup by name from a parent inode, the way both protocols resolve paths
    let found = m.lookup(dir.inode, "main.rs").await.unwrap();
    assert_eq!(found.inode, file.inode);
    assert_eq!(found.kind, FileKind::Regular);
    assert_eq!(found.size, 12);

    // getattr by inode agrees
    assert_eq!(m.getattr(file.inode).await.unwrap().size, 12);

    let listing = m.readdir(dir.inode).await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "main.rs");
    assert_eq!(listing[0].inode, file.inode);

    // The root is always inode 1 and always a directory.
    let root = m.getattr(ROOT_INODE).await.unwrap();
    assert_eq!(root.inode, ROOT_INODE);
    assert_eq!(root.kind, FileKind::Directory);
}

#[tokio::test]
async fn positional_io_through_handles() {
    let (_k, m) = mount().await;
    let (_, fh) = m.create(ROOT_INODE, "data.bin").await.unwrap();

    m.write(fh, 0, b"0123456789").await.unwrap();
    assert_eq!(m.read(fh, 2, 4).await.unwrap(), b"2345");
    m.write(fh, 4, b"XY").await.unwrap();
    assert_eq!(m.read(fh, 0, 10).await.unwrap(), b"0123XY6789");
    m.truncate(fh, 4).await.unwrap();
    assert_eq!(m.read(fh, 0, 100).await.unwrap(), b"0123");
    m.release(fh).await.unwrap();

    let attrs = m.lookup(ROOT_INODE, "data.bin").await.unwrap();
    assert_eq!(attrs.size, 4);

    // A handle is gone once released.
    assert!(matches!(m.read(fh, 0, 1).await, Err(SfsError::NotFound(_))));
}

/// The decision that separates this from a filesystem that commits whenever an editor flushes:
/// closing a file makes data visible to the mount and to nobody else.
#[tokio::test]
async fn closing_a_file_does_not_publish_a_commit() {
    let (k, m) = mount().await;
    let commits_before = k.timeline(50).await.unwrap().len();

    let (_, fh) = m.create(ROOT_INODE, "draft.txt").await.unwrap();
    m.write(fh, 0, b"work in progress").await.unwrap();
    m.release(fh).await.unwrap();

    // Visible through the mount...
    assert_eq!(m.lookup(ROOT_INODE, "draft.txt").await.unwrap().size, 16);
    assert!(m.is_dirty().await);
    // ...and to nobody else.
    assert_eq!(
        k.timeline(50).await.unwrap().len(),
        commits_before,
        "close and fsync must never invent a commit"
    );
    assert!(k
        .stat_head(&surrealfs_types::RepoPath::parse("/draft.txt").unwrap())
        .await
        .unwrap()
        .is_none());

    // Only an explicit publication moves the repository.
    m.publish(Some("agent turn complete".into())).await.unwrap();
    assert_eq!(k.timeline(50).await.unwrap().len(), commits_before + 1);
    assert_eq!(
        k.read_head_file(&surrealfs_types::RepoPath::parse("/draft.txt").unwrap())
            .await
            .unwrap(),
        b"work in progress"
    );
    assert!(!m.is_dirty().await);
}

#[tokio::test]
async fn aborting_discards_staged_work_and_the_mount_stays_usable() {
    let (_k, m) = mount().await;
    let (_, fh) = m.create(ROOT_INODE, "keep.txt").await.unwrap();
    m.write(fh, 0, b"keep").await.unwrap();
    m.release(fh).await.unwrap();
    m.publish(None).await.unwrap();

    let (_, fh) = m.create(ROOT_INODE, "scratch.txt").await.unwrap();
    m.write(fh, 0, b"discard me").await.unwrap();
    m.release(fh).await.unwrap();
    m.abort("agent run cancelled").await.unwrap();

    assert!(!m.is_dirty().await);
    assert!(matches!(
        m.lookup(ROOT_INODE, "scratch.txt").await,
        Err(SfsError::NotFound(_))
    ));
    // The published file survives, and the mount still works afterwards.
    assert_eq!(m.lookup(ROOT_INODE, "keep.txt").await.unwrap().size, 4);
    let (_, fh) = m.create(ROOT_INODE, "after.txt").await.unwrap();
    m.release(fh).await.unwrap();
    assert!(m.lookup(ROOT_INODE, "after.txt").await.is_ok());
}

#[tokio::test]
async fn namespace_mutations_keep_inode_mappings_coherent() {
    let (_k, m) = mount().await;
    let dir = m.mkdir(ROOT_INODE, "olddir").await.unwrap();
    let (file, fh) = m.create(dir.inode, "f.txt").await.unwrap();
    m.write(fh, 0, b"body").await.unwrap();
    m.release(fh).await.unwrap();

    m.rename(ROOT_INODE, "olddir", ROOT_INODE, "newdir")
        .await
        .unwrap();

    // The subtree kept its numbers, so a client holding them stays coherent.
    assert_eq!(m.getattr(dir.inode).await.unwrap().inode, dir.inode);
    assert_eq!(m.getattr(file.inode).await.unwrap().size, 4);
    let listing = m.readdir(ROOT_INODE).await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "newdir");

    // Removing forgets the mapping rather than leaving it dangling.
    m.unlink(dir.inode, "f.txt").await.unwrap();
    assert!(matches!(
        m.getattr(file.inode).await,
        Err(SfsError::NotFound(_))
    ));
    m.rmdir(ROOT_INODE, "newdir").await.unwrap();
    assert!(m.readdir(ROOT_INODE).await.unwrap().is_empty());
}

#[tokio::test]
async fn symlinks_hard_links_and_metadata() {
    let (_k, m) = mount().await;
    let (target, fh) = m.create(ROOT_INODE, "real.txt").await.unwrap();
    m.write(fh, 0, b"content").await.unwrap();
    m.release(fh).await.unwrap();

    let link = m
        .symlink(ROOT_INODE, "link.txt", "/real.txt")
        .await
        .unwrap();
    assert_eq!(link.kind, FileKind::Symlink);
    assert_eq!(m.readlink(link.inode).await.unwrap(), "/real.txt");

    // A hard link makes both names report nlink 2.
    let second = m.link(target.inode, ROOT_INODE, "alias.txt").await.unwrap();
    assert_eq!(second.nlink, 2);
    assert_eq!(m.getattr(target.inode).await.unwrap().nlink, 2);

    let changed = m
        .setattr(target.inode, Some(0o755), None, None)
        .await
        .unwrap();
    assert_eq!(changed.mode, 0o755);
    assert_eq!(changed.uid, 0, "unspecified fields are left alone");
}

/// A mount reports errno, so the domain errors have to arrive as the right ones.
#[tokio::test]
async fn failures_surface_as_the_errno_a_client_expects() {
    let (_k, m) = mount().await;
    m.mkdir(ROOT_INODE, "dir").await.unwrap();
    let (_, fh) = m.create(ROOT_INODE, "file.txt").await.unwrap();
    m.release(fh).await.unwrap();
    let dir = m.lookup(ROOT_INODE, "dir").await.unwrap();
    let (_, fh) = m.create(dir.inode, "child.txt").await.unwrap();
    m.release(fh).await.unwrap();

    let missing = m.lookup(ROOT_INODE, "nope.txt").await.unwrap_err();
    assert_eq!(errno_for(&missing), libc::ENOENT);

    let not_empty = m.rmdir(ROOT_INODE, "dir").await.unwrap_err();
    assert_eq!(errno_for(&not_empty), libc::ENOTEMPTY);

    let is_a_dir = m.unlink(ROOT_INODE, "dir").await.unwrap_err();
    assert_eq!(errno_for(&is_a_dir), libc::EISDIR);

    let bad_inode = m.getattr(999_999).await.unwrap_err();
    assert_eq!(errno_for(&bad_inode), libc::ENOENT);

    let not_a_link = m.readlink(ROOT_INODE).await.unwrap_err();
    assert_eq!(errno_for(&not_a_link), libc::EINVAL);
}

/// Timestamps are derived from the commit that published a change, because the state root
/// deliberately carries no clock.
#[tokio::test]
async fn mtime_reflects_publication_rather_than_a_stored_field() {
    let (_k, m) = mount().await;
    let (_, fh) = m.create(ROOT_INODE, "timed.txt").await.unwrap();
    m.write(fh, 0, b"v1").await.unwrap();
    m.release(fh).await.unwrap();

    let before_publish = m.lookup(ROOT_INODE, "timed.txt").await.unwrap().mtime;
    m.publish(Some("first".into())).await.unwrap();
    let after_publish = m.lookup(ROOT_INODE, "timed.txt").await.unwrap().mtime;

    // Both are real times, and publication supplies one derived from the commit.
    assert!(before_publish <= std::time::SystemTime::now());
    assert!(after_publish <= std::time::SystemTime::now());

    // A later edit advances it.
    let fh = m
        .open(
            m.lookup(ROOT_INODE, "timed.txt").await.unwrap().inode,
            false,
        )
        .await
        .unwrap();
    m.write(fh, 0, b"v2").await.unwrap();
    m.release(fh).await.unwrap();
    let after_edit = m.lookup(ROOT_INODE, "timed.txt").await.unwrap().mtime;
    assert!(
        after_edit >= after_publish,
        "an edit must not move a file's timestamp backwards"
    );
}

/// A mount is the workload that grows the staging buffer without bound, so the ceiling has to
/// reach the client as `EFBIG` rather than the daemon quietly consuming memory until it dies.
#[tokio::test]
async fn a_mount_reports_staging_pressure_and_refuses_to_exceed_it() {
    let (_k, m) = mount().await;
    m.set_staged_limit(64 * 1024).await;

    let (_, fh) = m.create(ROOT_INODE, "big.bin").await.unwrap();
    m.write(fh, 0, &vec![b'a'; 32 * 1024]).await.unwrap();
    m.release(fh).await.unwrap();

    let (staged, limit) = m.staged_pressure().await;
    assert_eq!(limit, 64 * 1024);
    assert!(staged > 0, "a written file must show as staged pressure");

    // Writes buffer in the open handle, so the workspace ceiling is reached when that buffer is
    // staged at close — not at write. The error still arrives as EFBIG, which is what a client
    // can act on, but it arrives from `release`.
    let (_, fh) = m.create(ROOT_INODE, "bigger.bin").await.unwrap();
    m.write(fh, 0, &vec![b'b'; 64 * 1024]).await.unwrap();
    let err = m.release(fh).await.unwrap_err();
    assert_eq!(errno_for(&err), libc::EFBIG);

    // Publishing releases it, and the mount keeps working.
    m.publish(Some("flush".into())).await.unwrap();
    let (after, _) = m.staged_pressure().await;
    assert_eq!(after, 0, "publication must release the staged bytes");
    let (_, fh) = m.create(ROOT_INODE, "after.bin").await.unwrap();
    m.write(fh, 0, b"fine").await.unwrap();
    m.release(fh).await.unwrap();
}

/// `dofs` reports inode 0 for a file between create and release, so every concurrently-pending
/// new file stats as the same inode and anything comparing identity conflates them. We allocate
/// at create, so the window does not exist — worth pinning, because it is cheap to regress.
#[tokio::test]
async fn a_pending_file_has_a_real_inode_before_anything_is_written() {
    let (_k, m) = mount().await;

    let (a, fh_a) = m.create(ROOT_INODE, "a.txt").await.unwrap();
    let (b, fh_b) = m.create(ROOT_INODE, "b.txt").await.unwrap();

    // Both exist, before a single byte is written or either handle is closed.
    assert_ne!(a.inode, 0, "a pending file must not report inode 0");
    assert_ne!(b.inode, 0);
    assert_ne!(
        a.inode, b.inode,
        "two pending files must be distinguishable"
    );

    // And they stat consistently while still open.
    assert_eq!(m.getattr(a.inode).await.unwrap().inode, a.inode);
    assert_eq!(m.lookup(ROOT_INODE, "b.txt").await.unwrap().inode, b.inode);

    m.write(fh_a, 0, b"a").await.unwrap();
    m.release(fh_a).await.unwrap();
    m.release(fh_b).await.unwrap();

    // The number survives the write that materialised the file.
    assert_eq!(m.lookup(ROOT_INODE, "a.txt").await.unwrap().inode, a.inode);
}

/// Overwriting an existing file: the sequence `fs::write` produces on a path that already exists,
/// which is open-with-truncate rather than create. Found by a real FUSE mount returning an empty
/// file, so it is pinned here where it can be tested on any platform.
#[tokio::test]
async fn overwriting_an_existing_file_through_open_truncate() {
    let (_k, m) = mount().await;

    let (attrs, fh) = m.create(ROOT_INODE, "f.txt").await.unwrap();
    m.write(fh, 0, b"first").await.unwrap();
    m.release(fh).await.unwrap();
    assert_eq!(m.lookup(ROOT_INODE, "f.txt").await.unwrap().size, 5);

    // Reopen with truncate, write different content, close.
    let fh = m.open(attrs.inode, true).await.unwrap();
    m.write(fh, 0, b"second body").await.unwrap();
    m.release(fh).await.unwrap();

    let fh = m.open(attrs.inode, false).await.unwrap();
    let got = m.read(fh, 0, 100).await.unwrap();
    m.release(fh).await.unwrap();
    assert_eq!(
        got, b"second body",
        "an overwrite through open-truncate lost the new content"
    );
    assert_eq!(m.lookup(ROOT_INODE, "f.txt").await.unwrap().size, 11);
}

/// Two handles on one path: the second one's close changes the file, and the first one's write is
/// then refused rather than clobbering it. That refusal is deliberate — it is what stops a stale
/// handle overwriting a file another surface replaced.
///
/// It is recorded here because it constrains the FUSE adapter: without `FUSE_ATOMIC_O_TRUNC` the
/// kernel turns `fs::write` into open + `setattr(size=0)` + write, and an adapter servicing that
/// setattr with its own handle lands in exactly this case and loses the caller's data. A real
/// mount did precisely that before the capability was requested.
#[tokio::test]
async fn a_second_handle_closing_underneath_an_open_one_is_refused() {
    let (_k, m) = mount().await;
    let (attrs, fh) = m.create(ROOT_INODE, "f.txt").await.unwrap();
    m.write(fh, 0, b"original").await.unwrap();
    m.release(fh).await.unwrap();

    let a = m.open(attrs.inode, false).await.unwrap();
    let b = m.open(attrs.inode, false).await.unwrap();
    m.truncate(b, 0).await.unwrap();
    m.release(b).await.unwrap();

    m.write(a, 0, b"replacement").await.unwrap();
    m.release(a).await.unwrap();

    // The stale handle did not win; the file is what the second handle left behind.
    let fh = m.open(attrs.inode, false).await.unwrap();
    assert_eq!(
        m.read(fh, 0, 100).await.unwrap(),
        b"",
        "a stale handle clobbered a file that changed underneath it"
    );
}

/// Overwriting with *shorter* content.
///
/// This is the case that catches a dropped `O_TRUNC`: when the replacement is longer, it covers
/// the old bytes and a missing truncate is invisible. Every earlier test here wrote longer
/// content, so the bug survived them.
#[tokio::test]
async fn overwriting_with_shorter_content_discards_the_old_tail() {
    let (_k, m) = mount().await;

    let (attrs, fh) = m.create(ROOT_INODE, "f.txt").await.unwrap();
    m.write(fh, 0, b"the original, rather long, content")
        .await
        .unwrap();
    m.release(fh).await.unwrap();

    // open(O_TRUNC), then write something shorter.
    let fh = m.open(attrs.inode, true).await.unwrap();
    m.write(fh, 0, b"short").await.unwrap();
    m.release(fh).await.unwrap();

    let fh = m.open(attrs.inode, false).await.unwrap();
    let got = m.read(fh, 0, 1000).await.unwrap();
    m.release(fh).await.unwrap();
    assert_eq!(
        got, b"short",
        "the old tail survived, so O_TRUNC was dropped somewhere"
    );
    assert_eq!(m.lookup(ROOT_INODE, "f.txt").await.unwrap().size, 5);
}
