//! Garbage collection.
//!
//! The dangerous failure here is not leaking — it is collecting something still in use. These
//! tests lean on that side: content shared between commits, content only an old commit still
//! references, and content only a savepoint or a fork keeps alive must all survive a sweep.

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{BranchName, RepoPath, RepositoryId};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("gc-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// Sweep with no grace period, as an operator running a manual collection would.
async fn sweep(k: &Kernel) -> surrealfs_store::GcReport {
    k.store().gc(k.repo(), 0).await.unwrap()
}

/// Aborting leaves nothing to collect: a workspace's writes never reach the store, because
/// chunks are staged inside `publish` rather than at write time.
#[tokio::test]
async fn aborting_leaves_no_garbage_at_all() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/keep.txt"), b"published content")
        .await
        .unwrap();
    ws.publish(None, Some("keep".into())).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/discarded.txt"), &vec![9u8; 100_000])
        .await
        .unwrap();
    ws.abort("changed my mind").await.unwrap();

    let report = sweep(&k).await;
    assert_eq!(
        report.chunks_removed, 0,
        "an aborted workspace never staged anything, so there is nothing to reclaim"
    );
    assert_eq!(
        k.read_head_file(&p("/keep.txt")).await.unwrap(),
        b"published content"
    );
}

/// The real orphan window: a publish stages its chunks, then loses the expected-head check.
/// The bytes are in the store with no commit that will ever reference them.
#[tokio::test]
async fn chunks_from_a_failed_publish_are_reclaimed() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/seed.txt"), b"seed").await.unwrap();
    ws.publish(None, None).await.unwrap();

    // Two workspaces from the same head; the second will lose the race.
    let mut winner = k.workspace().await.unwrap();
    let mut loser = k.workspace().await.unwrap();

    winner.write_file(&p("/winner.txt"), b"won").await.unwrap();
    winner.publish(None, Some("first".into())).await.unwrap();

    loser
        .write_file(&p("/loser.bin"), &vec![9u8; 100_000])
        .await
        .unwrap();
    let outcome = loser.publish(None, Some("second".into())).await;
    assert!(
        matches!(outcome, Err(surrealfs_types::SfsError::HeadConflict { .. })),
        "expected the stale workspace to lose the head check, got {outcome:?}"
    );

    // Its 100 KB was staged before the transaction that failed, and is now unreferenced.
    let report = sweep(&k).await;
    assert!(
        report.chunks_removed >= 1,
        "the failed publication's staged chunk must be reclaimed"
    );
    assert!(report.bytes_reclaimed >= 100_000);

    // The winner's commit is untouched.
    assert_eq!(k.read_head_file(&p("/winner.txt")).await.unwrap(), b"won");
    assert_eq!(k.read_head_file(&p("/seed.txt")).await.unwrap(), b"seed");
}

/// The important safety property: history keeps content alive. An old commit's content must
/// survive even though the current head no longer references it.
#[tokio::test]
async fn content_reachable_only_from_history_survives() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/file.txt"), b"first version")
        .await
        .unwrap();
    let first = ws.publish(None, Some("v1".into())).await.unwrap().commit;

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/file.txt"), b"second version")
        .await
        .unwrap();
    ws.publish(None, Some("v2".into())).await.unwrap();

    let report = sweep(&k).await;
    assert_eq!(
        report.chunks_removed, 0,
        "the first version is still reachable from its commit"
    );

    // And a historical read still works after the sweep.
    let (ns, _) = k.state_at(&first).await.unwrap();
    let entry = surrealfs_kernel::view::stat(k.store(), k.repo(), &ns, &p("/file.txt"))
        .await
        .unwrap()
        .unwrap();
    match entry {
        surrealfs_content::tree::Entry::File { extents, .. } => {
            let bytes = k
                .store()
                .fetch_chunk(k.repo(), &extents[0].chunk)
                .await
                .unwrap();
            assert_eq!(bytes, b"first version");
        }
        other => panic!("expected a file, got {other:?}"),
    }
}

/// Content shared by digest between two files must not be collected when one is removed.
#[tokio::test]
async fn shared_content_survives_while_any_reference_remains() {
    let k = kernel().await;
    let body = vec![3u8; 50_000];
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/a.bin"), &body).await.unwrap();
    ws.write_file(&p("/b.bin"), &body).await.unwrap();
    ws.publish(None, None).await.unwrap();

    // Remove one name. The chunk is still referenced by the other, and by history.
    let mut ws = k.workspace().await.unwrap();
    ws.unlink(&p("/a.bin")).await.unwrap();
    ws.publish(None, Some("drop one".into())).await.unwrap();

    let report = sweep(&k).await;
    assert_eq!(report.chunks_removed, 0);
    assert_eq!(k.read_head_file(&p("/b.bin")).await.unwrap(), body);
}

/// A savepoint and a fork are both roots of reachability, not decoration.
#[tokio::test]
async fn savepoints_and_branches_keep_content_alive() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/pinned.txt"), b"referenced only by a savepoint")
        .await
        .unwrap();
    ws.publish(None, None).await.unwrap();
    k.savepoint("pinned", None, None).await.unwrap();
    let pinned_commit = k.resolve_savepoint("pinned").await.unwrap();

    k.fork(&BranchName::parse("side").unwrap(), &pinned_commit, None)
        .await
        .unwrap();

    // Move main well past it.
    for i in 0..3 {
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/pinned.txt"), format!("rewrite {i}").as_bytes())
            .await
            .unwrap();
        ws.publish(None, None).await.unwrap();
    }

    let report = sweep(&k).await;
    assert_eq!(report.chunks_removed, 0);

    // Both the savepoint and the fork still resolve to readable content.
    let side = k.on_branch(BranchName::parse("side").unwrap());
    assert_eq!(
        side.read_head_file(&p("/pinned.txt")).await.unwrap(),
        b"referenced only by a savepoint"
    );
}

/// The grace period protects freshly staged content from a sweep that runs at the wrong moment.
#[tokio::test]
async fn the_grace_period_protects_recent_objects() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/seed.txt"), b"seed").await.unwrap();
    ws.publish(None, None).await.unwrap();

    // Produce a genuine orphan the same way a lost race does.
    let mut winner = k.workspace().await.unwrap();
    let mut loser = k.workspace().await.unwrap();
    winner.write_file(&p("/w.txt"), b"won").await.unwrap();
    winner.publish(None, None).await.unwrap();
    loser
        .write_file(&p("/orphan.bin"), &vec![1u8; 10_000])
        .await
        .unwrap();
    assert!(loser.publish(None, None).await.is_err());

    // With a generous grace period nothing is collected, but the orphan is counted.
    let report = k.store().gc(k.repo(), 3600).await.unwrap();
    assert_eq!(report.chunks_removed, 0);
    assert!(report.kept_within_grace >= 1);

    // With no grace period the same object goes.
    let report = sweep(&k).await;
    assert!(report.chunks_removed >= 1);
}

/// A sweep must be safe to run repeatedly, and must not disturb a healthy repository.
#[tokio::test]
async fn repeated_sweeps_are_stable() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/a.rs"), b"one").await.unwrap();
    ws.write_file(&p("/src/b.rs"), b"two").await.unwrap();
    ws.kv_set("app", "key", b"value").unwrap();
    ws.publish(None, None).await.unwrap();

    let first = sweep(&k).await;
    let second = sweep(&k).await;
    assert_eq!(second.chunks_removed, 0, "a second sweep finds nothing new");
    assert_eq!(second.nodes_removed, 0);
    let _ = first;

    // Everything still reads, including the KV value whose chunk is referenced only by the
    // KV node rather than by the namespace tree.
    assert_eq!(k.read_head_file(&p("/src/a.rs")).await.unwrap(), b"one");
    assert_eq!(k.read_head_file(&p("/src/b.rs")).await.unwrap(), b"two");
    assert_eq!(
        k.kv_get_head("app", "key").await.unwrap().unwrap(),
        b"value"
    );
}

/// Intermediate roots produced while editing are unreferenced once the commit lands.
#[tokio::test]
async fn intermediate_tree_nodes_are_reclaimed() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/a.txt"), b"one").await.unwrap();
    ws.link(&p("/a.txt"), &p("/b.txt")).await.unwrap();
    ws.link(&p("/a.txt"), &p("/c.txt")).await.unwrap();
    // Each of these rewrites the root, leaving earlier roots behind.
    ws.unlink(&p("/b.txt")).await.unwrap();
    ws.publish(None, Some("churn".into())).await.unwrap();

    let before = k.store().state_node_count(k.repo()).await.unwrap();
    let report = sweep(&k).await;
    let after = k.store().state_node_count(k.repo()).await.unwrap();

    assert!(
        report.nodes_removed > 0,
        "the intermediate roots from this edit should be collectible"
    );
    assert_eq!(after, before - report.nodes_removed);

    // The surviving state is intact.
    assert_eq!(k.read_head_file(&p("/a.txt")).await.unwrap(), b"one");
    assert_eq!(k.read_head_file(&p("/c.txt")).await.unwrap(), b"one");
    assert!(k.stat_head(&p("/b.txt")).await.unwrap().is_none());
}
