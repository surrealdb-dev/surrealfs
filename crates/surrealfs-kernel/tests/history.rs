//! Branches, savepoints, and reverting.
//!
//! The claim under test is that these are constant-time. Immutable content-addressed roots
//! mean a fork or a revert binds a name to state that already exists, so neither writes
//! content however large the repository is. That is what makes "instant snapshot" and
//! "instant fork" true statements rather than marketing.

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{BranchName, RepoPath, RepositoryId, SfsError};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("history-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// Commit a repository of `dirs * 10` files.
async fn populate(k: &Kernel, dirs: usize, body: &str) {
    let mut ws = k.workspace().await.unwrap();
    for d in 0..dirs {
        for f in 0..10 {
            ws.write_file(&p(&format!("/src/mod{d}/file{f}.rs")), body.as_bytes())
                .await
                .unwrap();
        }
    }
    ws.publish(None, Some("populate".into())).await.unwrap();
}

#[tokio::test]
async fn savepoints_name_a_commit_and_resolve_back() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/a.txt"), b"first").await.unwrap();
    let first = ws.publish(None, Some("first".into())).await.unwrap().commit;

    k.savepoint("before-risky-change", None, Some("known good".into()))
        .await
        .unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/a.txt"), b"second").await.unwrap();
    ws.publish(None, Some("second".into())).await.unwrap();

    assert_eq!(
        k.resolve_savepoint("before-risky-change").await.unwrap(),
        first
    );
    let listed = k.savepoints().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "before-risky-change");
    assert_eq!(listed[0].message.as_deref(), Some("known good"));

    assert!(matches!(
        k.resolve_savepoint("no-such-point").await,
        Err(SfsError::NotFound(_))
    ));
}

/// Forking a 400-file repository must store no additional content.
#[tokio::test]
async fn fork_copies_no_state() {
    let k = kernel().await;
    populate(&k, 40, "original").await;
    let base = k.head().await.unwrap().head;

    let before = k.store().state_node_count(k.repo()).await.unwrap();
    let alt = k
        .fork(
            &BranchName::parse("experiment").unwrap(),
            &base,
            Some("try another approach".into()),
        )
        .await
        .unwrap();
    let after = k.store().state_node_count(k.repo()).await.unwrap();

    assert_eq!(
        before,
        after,
        "forking a 400-file repository stored {} new nodes; it must store none",
        after - before
    );

    // The fork sees the same content and is a genuinely independent line of work.
    assert_eq!(alt.head().await.unwrap().head, base);
    assert_eq!(
        alt.read_head_file(&p("/src/mod0/file0.rs")).await.unwrap(),
        b"original"
    );

    let mut ws = alt.workspace().await.unwrap();
    ws.write_file(&p("/src/mod0/file0.rs"), b"experimental")
        .await
        .unwrap();
    ws.publish(None, Some("diverge".into())).await.unwrap();

    assert_eq!(
        alt.read_head_file(&p("/src/mod0/file0.rs")).await.unwrap(),
        b"experimental"
    );
    // main is untouched by the experiment.
    assert_eq!(
        k.read_head_file(&p("/src/mod0/file0.rs")).await.unwrap(),
        b"original"
    );
    assert_eq!(k.head().await.unwrap().head, base);

    let names: Vec<String> = k
        .branches()
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, vec!["experiment".to_string(), "main".to_string()]);
}

#[tokio::test]
async fn forking_a_taken_name_is_refused() {
    let k = kernel().await;
    let head = k.head().await.unwrap().head;
    k.fork(&BranchName::parse("dup").unwrap(), &head, None)
        .await
        .unwrap();
    assert!(matches!(
        k.fork(&BranchName::parse("dup").unwrap(), &head, None)
            .await,
        Err(SfsError::AlreadyExists(_))
    ));
}

/// Reverting restores state exactly, preserves the history it reverses, and — because every
/// node it needs already exists — stores no content.
#[tokio::test]
async fn revert_restores_exactly_without_copying_or_erasing_history() {
    let k = kernel().await;
    populate(&k, 40, "good").await;
    let good = k.head().await.unwrap().head;
    let good_root = k.head().await.unwrap().root;
    k.savepoint("known-good", None, None).await.unwrap();

    // A harmful run rewrites two files and deletes a third.
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/mod1/file1.rs"), b"corrupted")
        .await
        .unwrap();
    ws.write_file(&p("/src/mod2/file2.rs"), b"corrupted")
        .await
        .unwrap();
    ws.unlink(&p("/src/mod3/file3.rs")).await.unwrap();
    let harmful = ws
        .publish(None, Some("harmful run".into()))
        .await
        .unwrap()
        .commit;
    assert_eq!(
        k.read_head_file(&p("/src/mod1/file1.rs")).await.unwrap(),
        b"corrupted"
    );

    let before = k.store().state_node_count(k.repo()).await.unwrap();
    let receipt = k
        .revert_to(&good, Some("undo the harmful run".into()))
        .await
        .unwrap();
    let after = k.store().state_node_count(k.repo()).await.unwrap();

    assert_eq!(
        before,
        after,
        "revert stored {} new nodes; every node it needs already existed",
        after - before
    );

    // State is restored exactly: the same root, not merely equivalent content.
    assert_eq!(receipt.state_root, good_root);
    assert_eq!(
        k.read_head_file(&p("/src/mod1/file1.rs")).await.unwrap(),
        b"good"
    );
    assert_eq!(
        k.read_head_file(&p("/src/mod3/file3.rs")).await.unwrap(),
        b"good"
    );

    // History is preserved, not rewritten: the harmful commit is still there and still
    // explains itself, with the revert recorded after it.
    let timeline = k.timeline(10).await.unwrap();
    assert_eq!(timeline[0].commit, receipt.commit);
    assert_eq!(timeline[1].commit, harmful);

    let history = k.explain("/src/mod1/file1.rs", 10).await.unwrap();
    assert_eq!(history.len(), 3, "populate, harmful run, revert");
    assert_eq!(history[0].message.as_deref(), Some("undo the harmful run"));
    assert_eq!(history[1].message.as_deref(), Some("harmful run"));

    // Reverting to the state we are already in is refused rather than making an empty commit.
    assert!(k.revert_to(&good, None).await.is_err());
}

/// The recovery workflow end to end: mark a good point, run something harmful, fork the good
/// point to try an alternative, and compare — all without copying the repository.
#[tokio::test]
async fn fork_from_a_savepoint_to_retry_an_alternative() {
    let k = kernel().await;
    populate(&k, 5, "v1").await;
    k.savepoint("pre-run", None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/mod0/file0.rs"), b"approach-a")
        .await
        .unwrap();
    ws.publish(None, Some("approach A".into())).await.unwrap();

    // Fork the known-good point and try something else, in parallel with main.
    let good = k.resolve_savepoint("pre-run").await.unwrap();
    let alt = k
        .fork(&BranchName::parse("approach-b").unwrap(), &good, None)
        .await
        .unwrap();
    let mut ws = alt.workspace().await.unwrap();
    ws.write_file(&p("/src/mod0/file0.rs"), b"approach-b")
        .await
        .unwrap();
    ws.publish(None, Some("approach B".into())).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/src/mod0/file0.rs")).await.unwrap(),
        b"approach-a"
    );
    assert_eq!(
        alt.read_head_file(&p("/src/mod0/file0.rs")).await.unwrap(),
        b"approach-b"
    );

    // The two approaches differ in exactly one file, and the diff proves it without reading
    // the 49 files they share.
    let changes = k
        .diff_commits(
            &k.head().await.unwrap().head,
            &alt.head().await.unwrap().head,
        )
        .await
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path().as_str(), "/src/mod0/file0.rs");
}

/// Forking a moment rather than a named commit.
///
/// AgentFS's README advertises WAL-based time-travel forking and its source contains none; this
/// is the same capability made real. It is deliberately not a new fork mechanism — resolving a
/// time to a commit is the whole feature, and the fork underneath is the one already tested
/// above.
#[tokio::test]
async fn forking_at_a_moment_reproduces_that_moment() {
    let k = kernel().await;

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/config.toml"), b"version = 1")
        .await
        .unwrap();
    let first = ws.publish(None, Some("v1".into())).await.unwrap();
    let then = surrealfs_types::time::parse_rfc3339(
        &k.committed_at(&first.commit).await.unwrap().unwrap(),
    )
    .unwrap();

    // Move on, twice, so the moment is genuinely in the past.
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/config.toml"), b"version = 2")
        .await
        .unwrap();
    ws.publish(None, Some("v2".into())).await.unwrap();
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/config.toml"), b"version = 3")
        .await
        .unwrap();
    ws.publish(None, Some("v3".into())).await.unwrap();
    assert_eq!(
        k.read_head_file(&p("/config.toml")).await.unwrap(),
        b"version = 3"
    );

    // Fork the moment the first commit was current.
    let at = k.commit_at_or_before(then).await.unwrap();
    assert_eq!(at, first.commit);
    let forked = k
        .fork(
            &BranchName::parse("as-it-was").unwrap(),
            &at,
            Some("recover the first configuration".into()),
        )
        .await
        .unwrap();

    assert_eq!(
        forked.read_head_file(&p("/config.toml")).await.unwrap(),
        b"version = 1",
        "the fork does not hold the state that was live at that moment"
    );
    // And the branch it forked from is untouched.
    assert_eq!(
        k.read_head_file(&p("/config.toml")).await.unwrap(),
        b"version = 3"
    );
}

/// Time forking must stay constant-time. Resolving a moment is a lookup; if it ever became a
/// copy, this is what would catch it.
#[tokio::test]
async fn forking_at_a_moment_still_copies_no_state() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    for i in 0..40 {
        ws.write_file(&p(&format!("/f{i}.txt")), format!("body {i}").as_bytes())
            .await
            .unwrap();
    }
    let receipt = ws.publish(None, Some("forty files".into())).await.unwrap();
    let then = surrealfs_types::time::parse_rfc3339(
        &k.committed_at(&receipt.commit).await.unwrap().unwrap(),
    )
    .unwrap();

    let before = k.store().state_node_count(k.repo()).await.unwrap();
    let at = k.commit_at_or_before(then).await.unwrap();
    k.fork(&BranchName::parse("moment").unwrap(), &at, None)
        .await
        .unwrap();
    let after = k.store().state_node_count(k.repo()).await.unwrap();

    assert_eq!(
        before,
        after,
        "forking a moment wrote {} new state nodes; it must copy nothing",
        after - before
    );
}
