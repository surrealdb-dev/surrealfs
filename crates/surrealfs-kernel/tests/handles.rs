//! Open file handles and positional I/O.
//!
//! The case worth the most attention is open-unlinked. AgentFS deletes the inode and its data
//! the moment the last link goes, so a read through a still-open handle returns zero bytes —
//! the `mkstemp`-then-`unlink` idiom silently loses data there. SurrealFS keeps the handle's
//! content readable and drops it at close, which is what POSIX describes. That is a deliberate
//! difference, scored `STRONGER_WITH_DOCUMENTED_DIFFERENCE` rather than a parity failure.

use std::sync::Arc;

use surrealfs_kernel::{Kernel, OpenOptions};
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("handle-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

#[tokio::test]
async fn positional_read_and_write() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/data.bin"), b"0123456789").await.unwrap();

    let mut f = ws.open(&p("/data.bin"), OpenOptions::read()).await.unwrap();
    assert_eq!(f.fstat().size, 10);
    assert_eq!(f.pread(0, 4), b"0123");
    assert_eq!(f.pread(6, 4), b"6789");
    // Reading past the end returns what exists rather than erroring.
    assert_eq!(f.pread(8, 100), b"89");
    assert_eq!(f.pread(50, 10), b"");

    f.pwrite(2, b"XY").unwrap();
    assert_eq!(f.contents(), b"01XY456789");
    assert!(ws.close(f).await.unwrap());
    ws.publish(None, Some("edit".into())).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/data.bin")).await.unwrap(),
        b"01XY456789"
    );
}

#[tokio::test]
async fn writing_past_the_end_zero_fills() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    let mut f = ws
        .open(&p("/sparse.bin"), OpenOptions::create())
        .await
        .unwrap();
    f.pwrite(4, b"tail").unwrap();
    assert_eq!(f.contents(), b"\0\0\0\0tail");
    assert_eq!(f.fstat().size, 8);
    assert!(ws.close(f).await.unwrap());
    ws.publish(None, None).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/sparse.bin")).await.unwrap(),
        b"\0\0\0\0tail"
    );
}

#[tokio::test]
async fn truncate_grows_and_shrinks() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/f.txt"), b"abcdefghij").await.unwrap();

    let mut f = ws.open(&p("/f.txt"), OpenOptions::read()).await.unwrap();
    f.truncate(4).unwrap();
    assert_eq!(f.contents(), b"abcd");
    f.truncate(6).unwrap();
    assert_eq!(f.contents(), b"abcd\0\0", "growth zero-fills");
    assert!(ws.close(f).await.unwrap());

    // Opening with truncate discards existing content.
    let f = ws
        .open(&p("/f.txt"), OpenOptions::create_truncate())
        .await
        .unwrap();
    assert_eq!(f.fstat().size, 0);
    assert!(ws.close(f).await.unwrap());
    ws.publish(None, None).await.unwrap();
    assert_eq!(k.read_head_file(&p("/f.txt")).await.unwrap(), b"");
}

#[tokio::test]
async fn open_rejects_directories_symlinks_and_missing_files() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.mkdir(&p("/dir")).await.unwrap();
    ws.write_file(&p("/real.txt"), b"x").await.unwrap();
    ws.symlink(&p("/link.txt"), "/real.txt").await.unwrap();

    assert!(matches!(
        ws.open(&p("/dir"), OpenOptions::read()).await,
        Err(SfsError::IsADirectory(_))
    ));
    assert!(matches!(
        ws.open(&p("/link.txt"), OpenOptions::read()).await,
        Err(SfsError::InvalidPath(_))
    ));
    assert!(matches!(
        ws.open(&p("/nope.txt"), OpenOptions::read()).await,
        Err(SfsError::NotFound(_))
    ));
    // Creating produces an empty file even with no writes.
    let f = ws
        .open(&p("/fresh.txt"), OpenOptions::create())
        .await
        .unwrap();
    assert!(ws.close(f).await.unwrap());
    assert!(ws.stat(&p("/fresh.txt")).await.unwrap().is_some());
}

/// The documented difference from AgentFS: an unlinked file stays readable through its open
/// handle, and its data is dropped at close rather than resurrecting the file.
#[tokio::test]
async fn an_unlinked_file_stays_readable_through_its_handle() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/tmp/scratch"), b"work in progress")
        .await
        .unwrap();

    let mut f = ws
        .open(&p("/tmp/scratch"), OpenOptions::read())
        .await
        .unwrap();
    ws.unlink(&p("/tmp/scratch")).await.unwrap();

    // AgentFS would return nothing here; the content is still live for this handle.
    assert_eq!(f.pread(0, 4), b"work");
    assert_eq!(f.fstat().size, 16);
    f.pwrite(0, b"MORE").unwrap();
    assert_eq!(f.pread(0, 4), b"MORE");

    // Closing reports that the writes went nowhere, and the file stays deleted.
    assert!(
        !ws.close(f).await.unwrap(),
        "closing an unlinked handle must not resurrect the file"
    );
    assert!(ws.stat(&p("/tmp/scratch")).await.unwrap().is_none());

    ws.publish(None, Some("temp file lifecycle".into()))
        .await
        .unwrap();
    assert!(k.stat_head(&p("/tmp/scratch")).await.unwrap().is_none());
}

/// If the path is replaced while a handle is open, the handle is writing to something that no
/// longer exists. Its writes are dropped rather than clobbering the new file.
#[tokio::test]
async fn a_replaced_file_is_not_clobbered_by_a_stale_handle() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/shared.txt"), b"original").await.unwrap();

    let mut stale = ws
        .open(&p("/shared.txt"), OpenOptions::read())
        .await
        .unwrap();
    stale.pwrite(0, b"STALE___").unwrap();

    // Someone else replaces the file entirely.
    ws.write_file(&p("/shared.txt"), b"replaced by another writer")
        .await
        .unwrap();

    assert!(!ws.close(stale).await.unwrap());
    ws.publish(None, None).await.unwrap();
    assert_eq!(
        k.read_head_file(&p("/shared.txt")).await.unwrap(),
        b"replaced by another writer"
    );
}

/// A handle opened read-only and never written costs nothing at close.
#[tokio::test]
async fn a_clean_handle_makes_no_commit() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/read-me.txt"), b"unchanged")
        .await
        .unwrap();
    ws.publish(None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    let f = ws
        .open(&p("/read-me.txt"), OpenOptions::read())
        .await
        .unwrap();
    assert_eq!(f.pread(0, 9), b"unchanged");
    assert!(ws.close(f).await.unwrap());
    assert!(!ws.is_dirty(), "reading must not dirty the workspace");
    ws.abort("read only").await.unwrap();
}

#[tokio::test]
async fn multi_chunk_files_read_and_write_across_boundaries() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    // Larger than the 256 KiB chunk size, so the handle spans several chunks.
    let body: Vec<u8> = (0..600_000).map(|i| (i % 251) as u8).collect();
    ws.write_file(&p("/big.bin"), &body).await.unwrap();

    let mut f = ws.open(&p("/big.bin"), OpenOptions::read()).await.unwrap();
    assert_eq!(f.fstat().size, 600_000);
    // A read straddling a chunk boundary returns contiguous bytes.
    assert_eq!(f.pread(262_140, 8), body[262_140..262_148].to_vec());
    f.pwrite(262_143, b"BOUNDARY").unwrap();
    assert!(ws.close(f).await.unwrap());
    ws.publish(None, None).await.unwrap();

    let stored = k.read_head_file(&p("/big.bin")).await.unwrap();
    assert_eq!(stored.len(), 600_000);
    assert_eq!(&stored[262_143..262_151], b"BOUNDARY");
}
