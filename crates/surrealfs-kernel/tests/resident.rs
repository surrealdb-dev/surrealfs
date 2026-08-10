//! The resident node tier.
//!
//! Two things must hold together: repeated reads stop going to the store, and the answers do
//! not change. A cache that is fast and wrong is worse than no cache, so every test here
//! checks the result as well as the counters.

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("resident-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// A deep path costs one store read per component the first time and none afterwards.
#[tokio::test]
async fn a_repeated_deep_read_stops_touching_the_store() {
    let k = kernel().await;
    let deep = "/a/b/c/d/e/f/file.txt";
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p(deep), b"deep content").await.unwrap();
    ws.publish(None, None).await.unwrap();

    // Warm: this walk populates the cache along the whole route.
    assert_eq!(k.read_head_file(&p(deep)).await.unwrap(), b"deep content");
    let after_warm = k.store().cache_stats();
    assert!(
        after_warm.misses > 0,
        "the first walk must have gone to the store"
    );

    // Repeat the same lookup several times.
    let before = k.store().cache_stats();
    for _ in 0..5 {
        assert!(k.stat_head(&p(deep)).await.unwrap().is_some());
    }
    let after = k.store().cache_stats();

    assert_eq!(
        after.misses, before.misses,
        "a repeated read of an unchanged path must not go to the store at all"
    );
    assert!(
        after.hits > before.hits,
        "and it must have been served from the resident tier"
    );
}

/// Nodes are shared by digest, so warming one path warms the directories its siblings share.
#[tokio::test]
async fn sibling_paths_reuse_the_shared_route() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    for i in 0..5 {
        ws.write_file(&p(&format!("/src/deep/nest/file{i}.rs")), b"body")
            .await
            .unwrap();
    }
    ws.publish(None, None).await.unwrap();

    // Warm on the first sibling.
    k.read_head_file(&p("/src/deep/nest/file0.rs"))
        .await
        .unwrap();
    let before = k.store().cache_stats();

    // The other four share /, /src, /src/deep and /src/deep/nest.
    for i in 1..5 {
        assert_eq!(
            k.read_head_file(&p(&format!("/src/deep/nest/file{i}.rs")))
                .await
                .unwrap(),
            b"body"
        );
    }
    let after = k.store().cache_stats();
    assert_eq!(
        after.misses, before.misses,
        "siblings share every directory on the route, so none of it should be re-read"
    );
}

/// A commit rewrites the nodes on its route, giving them new digests. The cache must serve the
/// new content, not the old — which is automatic when the key is the content hash.
#[tokio::test]
async fn a_write_is_visible_immediately_despite_the_cache() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/config/app.toml"), b"version = 1")
        .await
        .unwrap();
    ws.publish(None, None).await.unwrap();

    // Warm the route.
    assert_eq!(
        k.read_head_file(&p("/config/app.toml")).await.unwrap(),
        b"version = 1"
    );

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/config/app.toml"), b"version = 2")
        .await
        .unwrap();
    ws.publish(None, Some("bump".into())).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/config/app.toml")).await.unwrap(),
        b"version = 2",
        "a cached route must never mask a newer commit"
    );

    // Historical reads still resolve the old content, because the old nodes are still valid
    // under their own digests.
    let first = k.first_commit().await.unwrap();
    let timeline = k.timeline(10).await.unwrap();
    let original = timeline
        .iter()
        .find(|c| c.commit != timeline[0].commit && c.commit != first)
        .expect("the first content commit");
    let (ns, _) = k.state_at(&original.commit).await.unwrap();
    let entry = surrealfs_kernel::view::stat(k.store(), k.repo(), &ns, &p("/config/app.toml"))
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
            assert_eq!(bytes, b"version = 1");
        }
        other => panic!("expected a file, got {other:?}"),
    }
}

/// The tier is a cache, so losing it must cost latency and nothing else.
#[tokio::test]
async fn dropping_the_cache_changes_no_answer() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/a/b/one.txt"), b"one").await.unwrap();
    ws.write_file(&p("/a/b/two.txt"), b"two").await.unwrap();
    ws.kv_set("app", "k", b"v").unwrap();
    ws.publish(None, None).await.unwrap();

    let warm_listing = k.list_head(&p("/a/b")).await.unwrap();
    let warm_file = k.read_head_file(&p("/a/b/one.txt")).await.unwrap();

    // Simulate losing the tier entirely, as a restart or an eviction storm would.
    k.store().clear_resident_cache();
    assert_eq!(k.store().cache_stats().resident, 0);

    assert_eq!(k.list_head(&p("/a/b")).await.unwrap(), warm_listing);
    assert_eq!(
        k.read_head_file(&p("/a/b/one.txt")).await.unwrap(),
        warm_file
    );
    assert_eq!(k.kv_get_head("app", "k").await.unwrap().unwrap(), b"v");
}
