//! Filesystem operations beyond create and remove: rename, copy, symlinks, metadata.
//!
//! Identity in this design is the path, so a rename leaves no trace in the state root — the
//! tree just sees one entry gone and another arrived. The relationship is recorded as intent
//! in the mutation log, and these tests hold that line: the root must stay a pure function of
//! content, while `explain` must still be able to say a rename happened.

use std::sync::Arc;

use surrealfs_content::tree::Entry;
use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("fsops-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

#[tokio::test]
async fn rename_moves_a_file_and_records_the_relationship() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/old.rs"), b"contents").await.unwrap();
    ws.publish(None, Some("create".into())).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.rename(&p("/src/old.rs"), &p("/src/new.rs"))
        .await
        .unwrap();
    ws.publish(None, Some("rename".into())).await.unwrap();

    assert!(k.stat_head(&p("/src/old.rs")).await.unwrap().is_none());
    assert_eq!(
        k.read_head_file(&p("/src/new.rs")).await.unwrap(),
        b"contents"
    );

    // The destination's history reports a rename, not an unexplained appearance.
    let history = k.explain("/src/new.rs", 10).await.unwrap();
    assert_eq!(history[0].kind, "RENAME");
}

/// A rename must not change the root beyond what the content change implies: reaching a tree
/// by renaming must give the same digest as building it directly.
#[tokio::test]
async fn rename_leaves_a_pure_content_root() {
    let renamed = {
        let k = kernel().await;
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/a/temp.txt"), b"body").await.unwrap();
        ws.rename(&p("/a/temp.txt"), &p("/a/final.txt"))
            .await
            .unwrap();
        ws.publish(None, None).await.unwrap().state_root
    };
    let direct = {
        let k = kernel().await;
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/a/final.txt"), b"body").await.unwrap();
        ws.publish(None, None).await.unwrap().state_root
    };
    assert_eq!(
        renamed, direct,
        "how a tree was reached must not affect its root"
    );
}

#[tokio::test]
async fn rename_moves_a_whole_directory() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/old/a.txt"), b"one").await.unwrap();
    ws.write_file(&p("/old/nested/b.txt"), b"two")
        .await
        .unwrap();
    ws.publish(None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.rename(&p("/old"), &p("/new")).await.unwrap();
    ws.publish(None, Some("move dir".into())).await.unwrap();

    assert!(k.stat_head(&p("/old")).await.unwrap().is_none());
    assert_eq!(k.read_head_file(&p("/new/a.txt")).await.unwrap(), b"one");
    assert_eq!(
        k.read_head_file(&p("/new/nested/b.txt")).await.unwrap(),
        b"two"
    );
}

#[tokio::test]
async fn rename_refuses_unsafe_moves() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/dir/file.txt"), b"x").await.unwrap();
    ws.write_file(&p("/other/keep.txt"), b"y").await.unwrap();

    // A directory cannot be moved inside itself; that would detach the subtree.
    assert!(matches!(
        ws.rename(&p("/dir"), &p("/dir/inner")).await,
        Err(SfsError::InvalidPath(_))
    ));
    // Replacing a non-empty directory is refused.
    assert!(matches!(
        ws.rename(&p("/dir"), &p("/other")).await,
        Err(SfsError::DirectoryNotEmpty(_))
    ));
    // A missing source is a typed not-found.
    assert!(matches!(
        ws.rename(&p("/nope"), &p("/somewhere")).await,
        Err(SfsError::NotFound(_))
    ));
    // Renaming onto itself is a no-op rather than an error.
    ws.rename(&p("/dir/file.txt"), &p("/dir/file.txt"))
        .await
        .unwrap();
}

/// Copying shares content by digest, so a copied file costs no new chunks.
#[tokio::test]
async fn copy_shares_content_rather_than_duplicating_it() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/original.bin"), &vec![7u8; 300_000])
        .await
        .unwrap();
    ws.publish(None, None).await.unwrap();

    let nodes_before = k.store().state_node_count(k.repo()).await.unwrap();
    let mut ws = k.workspace().await.unwrap();
    ws.copy(&p("/original.bin"), &p("/duplicate.bin"))
        .await
        .unwrap();
    ws.publish(None, Some("copy".into())).await.unwrap();
    let nodes_after = k.store().state_node_count(k.repo()).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/duplicate.bin")).await.unwrap(),
        vec![7u8; 300_000]
    );
    // Only the root node changes; the 300 KB of content is referenced, not re-stored.
    assert!(
        nodes_after - nodes_before <= 1,
        "copying stored {} new nodes",
        nodes_after - nodes_before
    );
}

#[tokio::test]
async fn symlinks_round_trip_without_being_followed() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/real.txt"), b"target contents")
        .await
        .unwrap();
    ws.symlink(&p("/link.txt"), "/real.txt").await.unwrap();
    ws.symlink(&p("/dangling.txt"), "/does/not/exist")
        .await
        .unwrap();
    ws.publish(None, Some("links".into())).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    assert_eq!(ws.readlink(&p("/link.txt")).await.unwrap(), "/real.txt");
    // A dangling target is stored as written; symlinks are not resolved at write time.
    assert_eq!(
        ws.readlink(&p("/dangling.txt")).await.unwrap(),
        "/does/not/exist"
    );
    assert!(matches!(
        ws.readlink(&p("/real.txt")).await,
        Err(SfsError::InvalidPath(_))
    ));
    assert!(matches!(
        ws.symlink(&p("/link.txt"), "/elsewhere").await,
        Err(SfsError::AlreadyExists(_))
    ));
}

#[tokio::test]
async fn metadata_changes_without_touching_content() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/script.sh"), b"#!/bin/sh\n")
        .await
        .unwrap();
    ws.publish(None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.set_meta(&p("/script.sh"), Some(0o755), None, None)
        .await
        .unwrap();
    ws.publish(None, Some("make executable".into()))
        .await
        .unwrap();

    let entry = k.stat_head(&p("/script.sh")).await.unwrap().unwrap();
    assert_eq!(entry.meta().mode, 0o755);
    assert_eq!(entry.meta().uid, 0, "unspecified fields are left alone");
    // Content is unchanged, and still readable.
    assert_eq!(
        k.read_head_file(&p("/script.sh")).await.unwrap(),
        b"#!/bin/sh\n"
    );

    let history = k.explain("/script.sh", 10).await.unwrap();
    assert_eq!(history[0].kind, "SET_META");

    // Setting the same values again is a no-op rather than an empty commit.
    let mut ws = k.workspace().await.unwrap();
    ws.set_meta(&p("/script.sh"), Some(0o755), None, None)
        .await
        .unwrap();
    assert!(!ws.is_dirty());
    ws.abort("no change").await.unwrap();
}

#[tokio::test]
async fn metadata_survives_on_directories_too() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.mkdir(&p("/private")).await.unwrap();
    ws.set_meta(&p("/private"), Some(0o700), Some(501), Some(20))
        .await
        .unwrap();
    ws.publish(None, None).await.unwrap();

    let entry = k.stat_head(&p("/private")).await.unwrap().unwrap();
    assert!(matches!(entry, Entry::Dir { .. }));
    assert_eq!(entry.meta().mode, 0o700);
    assert_eq!(entry.meta().uid, 501);
    assert_eq!(entry.meta().gid, 20);
}
