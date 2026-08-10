//! Resolving a moment to the commit that was current then.
//!
//! These deliberately avoid `sleep`. Sleeping to separate commits in time makes a test slow and
//! flaky in exchange for nothing: the commits already carry real timestamps, so the test reads
//! them back and queries the midpoints between them. That is deterministic, and it also checks
//! the stored values rather than assuming what they must be.

use std::sync::Arc;
use std::time::Duration;

use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::time::parse_rfc3339;
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

async fn kernel(name: &str) -> Arc<Kernel> {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Arc::new(
        Kernel::open(store, RepositoryId::parse(name).unwrap())
            .await
            .unwrap(),
    )
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// Publish `body` and return the resulting commit with the time it was published.
async fn publish(k: &Kernel, body: &[u8]) -> (surrealfs_types::CommitId, std::time::SystemTime) {
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/f.txt"), body).await.unwrap();
    let receipt = ws
        .publish(None, Some(String::from_utf8_lossy(body).to_string()))
        .await
        .unwrap();
    let stamp = k.committed_at(&receipt.commit).await.unwrap().unwrap();
    let at = parse_rfc3339(&stamp).expect("the database's own timestamp must parse");
    (receipt.commit, at)
}

#[tokio::test]
async fn a_timestamp_resolves_to_the_commit_that_was_current_then() {
    let k = kernel("time-resolve").await;
    let (first, t1) = publish(&k, b"one").await;
    let (second, t2) = publish(&k, b"two").await;
    let (third, t3) = publish(&k, b"three").await;

    // At each commit's own instant, that commit is the answer.
    assert_eq!(k.commit_at_or_before(t1).await.unwrap(), first);
    assert_eq!(k.commit_at_or_before(t3).await.unwrap(), third);

    // Between two commits, the earlier one is still current — which is the whole point: a
    // moment resolves to the state the repository was actually in.
    if t2 > t1 + Duration::from_micros(1) {
        assert_eq!(
            k.commit_at_or_before(t2 - Duration::from_micros(1))
                .await
                .unwrap(),
            first,
            "an instant before the second commit must still see the first"
        );
    }
    if t3 > t2 + Duration::from_micros(1) {
        assert_eq!(
            k.commit_at_or_before(t3 - Duration::from_micros(1))
                .await
                .unwrap(),
            second
        );
    }

    // Far in the future, the newest commit is current.
    assert_eq!(
        k.commit_at_or_before(t3 + Duration::from_secs(3600))
            .await
            .unwrap(),
        third
    );
}

/// A reference before the repository existed must say so, and say when it does begin, rather
/// than returning an empty result the caller has to interpret.
#[tokio::test]
async fn a_time_before_the_first_commit_is_a_clear_not_found() {
    let k = kernel("time-before").await;
    let (_, t1) = publish(&k, b"one").await;

    let err = k
        .commit_at_or_before(t1 - Duration::from_secs(86_400))
        .await
        .unwrap_err();
    let SfsError::NotFound(message) = &err else {
        panic!("expected NotFound, got {err:?}");
    };
    assert!(
        message.contains("begins at"),
        "the message should name the repository's earliest moment: {message}"
    );
}

/// Commits published in the same instant — which happens on a coarse clock, and can also happen
/// across a clock adjustment — must resolve by publication order, not arbitrarily.
#[tokio::test]
async fn commits_sharing_a_timestamp_resolve_by_publication_order() {
    let k = kernel("time-tiebreak").await;
    let mut latest = None;
    let mut at = None;
    for i in 0..6 {
        let (commit, stamp) = publish(&k, format!("body {i}").as_bytes()).await;
        latest = Some(commit);
        at = Some(stamp);
    }
    let (latest, at) = (latest.unwrap(), at.unwrap());

    // Several of these almost certainly share a timestamp at second or microsecond granularity.
    // Whatever the clock did, resolving at the newest stamp must give the newest commit, because
    // domain_sequence breaks the tie.
    assert_eq!(
        k.commit_at_or_before(at).await.unwrap(),
        latest,
        "a shared timestamp resolved to something other than the last commit published"
    );
}

/// The state at a moment is the state that was live then, not the state now.
#[tokio::test]
async fn reading_at_a_moment_returns_what_was_current_then() {
    let k = kernel("time-read").await;
    let (_, t1) = publish(&k, b"original").await;
    publish(&k, b"replaced").await;

    let then = k.commit_at_or_before(t1).await.unwrap();
    let root = k.store().root_of_commit(k.repo(), &then).await.unwrap();
    assert_eq!(
        k.read_file_at(&root, &p("/f.txt")).await.unwrap(),
        b"original"
    );
    assert_eq!(k.read_head_file(&p("/f.txt")).await.unwrap(), b"replaced");
}
