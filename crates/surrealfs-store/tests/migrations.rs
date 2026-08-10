//! Reporting migration state without applying it.
//!
//! Opening a repository is what applies migrations, so an inspection that opens normally would
//! only ever report success. These tests check the read-only path actually leaves the ledger
//! alone.

use surrealfs_store::{MigrationState, Store, StoreConfig, StoreEngine};

#[tokio::test]
async fn a_fresh_repository_reports_everything_pending_before_it_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");

    let store = Store::open_with(
        StoreEngine::SurrealKv(path.clone()),
        StoreConfig::read_only(),
    )
    .await
    .unwrap();
    let states = store.migration_status().await.unwrap();
    drop(store);

    assert!(!states.is_empty(), "no migrations are known to this build");
    assert!(
        states
            .iter()
            .all(|s| matches!(s, MigrationState::Pending(_))),
        "a repository that has never been opened cannot have applied anything: {states:?}"
    );
    assert!(
        states.iter().any(|s| s.id() == "0004-encryption"),
        "the newest migration should be listed"
    );
}

/// The property that makes the report worth reading: inspecting must not be the thing that
/// changes the answer.
#[tokio::test]
async fn inspecting_does_not_apply_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");

    for _ in 0..2 {
        let store = Store::open_with(
            StoreEngine::SurrealKv(path.clone()),
            StoreConfig::read_only(),
        )
        .await
        .unwrap();
        let states = store.migration_status().await.unwrap();
        assert!(
            states
                .iter()
                .all(|s| matches!(s, MigrationState::Pending(_))),
            "an inspection applied a migration: {states:?}"
        );
        drop(store);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn an_opened_repository_reports_everything_applied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");

    let store = Store::open(StoreEngine::SurrealKv(path.clone()))
        .await
        .unwrap();
    let states = store.migration_status().await.unwrap();

    assert!(
        states
            .iter()
            .all(|s| matches!(s, MigrationState::Applied(_))),
        "opening should have applied every migration: {states:?}"
    );
    assert!(
        states.iter().all(|s| !s.is_blocking()),
        "a healthy repository must not report a blocking state"
    );
}
