//! The bound on unpublished staged bytes.
//!
//! A workspace that is never published on its own is exactly what a long-lived mount is, so the
//! staging buffer is the thing that grows without limit if nothing stops it. ContextFS declares a
//! cap of this kind and never checks it — the constant exists, the branch does not — so these
//! tests drive the limit rather than asserting the number.

use std::sync::Arc;

use surrealfs_kernel::{Kernel, DEFAULT_STAGED_LIMIT};
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

async fn kernel() -> Arc<Kernel> {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Arc::new(
        Kernel::open(store, RepositoryId::parse("staging-test").unwrap())
            .await
            .unwrap(),
    )
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

#[tokio::test]
async fn staging_past_the_limit_is_refused_rather_than_growing() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.set_staged_limit(64 * 1024);

    // Comfortably inside the limit.
    ws.write_file(&p("/a.bin"), &vec![b'a'; 32 * 1024])
        .await
        .unwrap();
    assert_eq!(ws.staged_bytes(), 32 * 1024);

    // The write that would cross it fails, and says what to do about it.
    let err = ws
        .write_file(&p("/b.bin"), &vec![b'b'; 64 * 1024])
        .await
        .unwrap_err();
    let SfsError::OverBudget(message) = &err else {
        panic!("expected OverBudget, got {err:?}");
    };
    assert!(message.contains("publish"), "{message}");
    assert!(message.contains("abort"), "{message}");

    // A refused write must not have consumed any budget on its way out.
    assert_eq!(
        ws.staged_bytes(),
        32 * 1024,
        "a rejected write left bytes staged"
    );
}

/// The advantage content addressing gives us here: `dofs` buffers raw per-file bytes and charges
/// for every one, so rewriting a file N times costs N copies. A chunk we already hold is free.
#[tokio::test]
async fn rewriting_the_same_content_does_not_consume_more_budget() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.set_staged_limit(128 * 1024);

    let body = vec![b'x'; 64 * 1024];
    ws.write_file(&p("/one.bin"), &body).await.unwrap();
    let after_first = ws.staged_bytes();
    assert_eq!(after_first, 64 * 1024);

    // Same bytes, again, and at a second path: identical chunks, so nothing new to hold.
    for _ in 0..20 {
        ws.write_file(&p("/one.bin"), &body).await.unwrap();
    }
    ws.write_file(&p("/two.bin"), &body).await.unwrap();
    assert_eq!(
        ws.staged_bytes(),
        after_first,
        "identical chunks were charged more than once"
    );
}

#[tokio::test]
async fn publishing_releases_the_budget() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.set_staged_limit(64 * 1024);
    ws.write_file(&p("/big.bin"), &vec![b'z'; 48 * 1024])
        .await
        .unwrap();
    assert!(ws.staged_bytes() > 0);

    ws.publish(None, Some("flush".into())).await.unwrap();

    // A fresh workspace starts empty, and the earlier work is durable.
    let ws2 = k.workspace().await.unwrap();
    assert_eq!(ws2.staged_bytes(), 0);
    assert_eq!(
        k.read_head_file(&p("/big.bin")).await.unwrap().len(),
        48 * 1024
    );
}

#[tokio::test]
async fn aborting_releases_the_budget_too() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/scratch.bin"), &vec![b'q'; 4096])
        .await
        .unwrap();
    assert!(ws.staged_bytes() > 0);
    ws.abort("cancelled").await.unwrap();

    let ws2 = k.workspace().await.unwrap();
    assert_eq!(ws2.staged_bytes(), 0);
}

/// KV values stage through the same budget as file bytes, because they occupy the same buffer.
#[tokio::test]
async fn kv_values_are_charged_against_the_same_budget() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.set_staged_limit(8 * 1024);

    ws.kv_set("agent", "small", &[1u8; 1024]).unwrap();
    assert_eq!(ws.staged_bytes(), 1024);

    let err = ws.kv_set("agent", "huge", &vec![2u8; 16 * 1024]);
    assert!(
        matches!(err, Err(SfsError::OverBudget(_))),
        "an oversized KV value must be refused like an oversized write"
    );
}

#[tokio::test]
async fn a_new_workspace_starts_at_the_default_limit() {
    let k = kernel().await;
    let ws = k.workspace().await.unwrap();
    assert_eq!(ws.staged_limit(), DEFAULT_STAGED_LIMIT);
    assert_eq!(ws.staged_bytes(), 0);
}

/// The second tier: an open handle materialises the whole file, so a sparse truncate is a way to
/// demand an arbitrary allocation without writing anything. It must be refused before the
/// zero-fill, not after.
#[tokio::test]
async fn a_sparse_truncate_cannot_demand_an_arbitrary_allocation() {
    use surrealfs_kernel::OpenOptions;

    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    let mut handle = ws
        .open(&p("/sparse.bin"), OpenOptions::create_truncate())
        .await
        .unwrap();

    let err = handle.truncate(100 << 30).unwrap_err();
    assert!(
        matches!(err, SfsError::OverBudget(_)),
        "expected OverBudget, got {err:?}"
    );
    assert!(
        handle.contents().is_empty(),
        "the refused truncate allocated anyway"
    );

    // A size inside the ceiling still works.
    handle.truncate(4096).unwrap();
    assert_eq!(handle.contents().len(), 4096);
}
