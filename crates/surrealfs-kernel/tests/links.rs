//! Hard links.
//!
//! The design constraint: support two names for one file without allocated inode numbers,
//! because an allocated identity would make the state root depend on the history that produced
//! it rather than on the content it holds. Membership is therefore stored as content — the
//! sorted list of member paths on every member's entry — and these tests hold both halves of
//! that bargain: links behave like links, and the root stays a pure function of logical state.

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("link-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

#[tokio::test]
async fn a_link_is_a_second_name_for_one_file() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/original.txt"), b"shared body")
        .await
        .unwrap();
    ws.link(&p("/original.txt"), &p("/alias.txt"))
        .await
        .unwrap();
    ws.publish(None, Some("link".into())).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/alias.txt")).await.unwrap(),
        b"shared body"
    );
    let mut ws = k.workspace().await.unwrap();
    assert_eq!(ws.link_count(&p("/original.txt")).await.unwrap(), 2);
    assert_eq!(ws.link_count(&p("/alias.txt")).await.unwrap(), 2);

    // An unlinked file reports one link, not zero.
    ws.write_file(&p("/solo.txt"), b"alone").await.unwrap();
    assert_eq!(ws.link_count(&p("/solo.txt")).await.unwrap(), 1);
}

/// The defining property of a hard link: a write through either name is visible through both.
#[tokio::test]
async fn a_write_through_one_name_is_visible_through_the_other() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/a.txt"), b"first").await.unwrap();
    ws.link(&p("/a.txt"), &p("/b.txt")).await.unwrap();
    ws.publish(None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/b.txt"), b"written through b")
        .await
        .unwrap();
    ws.publish(None, Some("write via b".into())).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/a.txt")).await.unwrap(),
        b"written through b",
        "a write through one name must reach the file, not just that name"
    );
    assert_eq!(
        k.read_head_file(&p("/b.txt")).await.unwrap(),
        b"written through b"
    );
    // Still one file under two names.
    let mut ws = k.workspace().await.unwrap();
    assert_eq!(ws.link_count(&p("/a.txt")).await.unwrap(), 2);
}

#[tokio::test]
async fn removing_one_name_leaves_the_file_under_the_others() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/one.txt"), b"content").await.unwrap();
    ws.link(&p("/one.txt"), &p("/two.txt")).await.unwrap();
    ws.link(&p("/one.txt"), &p("/three.txt")).await.unwrap();
    ws.publish(None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    assert_eq!(ws.link_count(&p("/one.txt")).await.unwrap(), 3);
    ws.unlink(&p("/two.txt")).await.unwrap();
    ws.publish(None, Some("drop one name".into()))
        .await
        .unwrap();

    assert!(k.stat_head(&p("/two.txt")).await.unwrap().is_none());
    assert_eq!(k.read_head_file(&p("/one.txt")).await.unwrap(), b"content");
    let mut ws = k.workspace().await.unwrap();
    assert_eq!(
        ws.link_count(&p("/one.txt")).await.unwrap(),
        2,
        "the count must drop with the name"
    );

    // Removing down to one name leaves an ordinary file.
    ws.unlink(&p("/three.txt")).await.unwrap();
    assert_eq!(ws.link_count(&p("/one.txt")).await.unwrap(), 1);
    // And removing the last name removes the file.
    ws.unlink(&p("/one.txt")).await.unwrap();
    assert!(ws.stat(&p("/one.txt")).await.unwrap().is_none());
}

/// The root must not encode how a state was reached. Two files that merely hold identical
/// bytes are not linked, and that difference has to show up in the digest.
#[tokio::test]
async fn linked_and_merely_identical_files_have_different_roots() {
    let linked = {
        let k = kernel().await;
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/a.txt"), b"same bytes").await.unwrap();
        ws.link(&p("/a.txt"), &p("/b.txt")).await.unwrap();
        ws.publish(None, None).await.unwrap().state_root
    };
    let coincidental = {
        let k = kernel().await;
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/a.txt"), b"same bytes").await.unwrap();
        ws.write_file(&p("/b.txt"), b"same bytes").await.unwrap();
        ws.publish(None, None).await.unwrap().state_root
    };
    assert_ne!(
        linked, coincidental,
        "two names for one file is not the same state as two files with equal content"
    );

    // And the same link structure reached two ways gives the same root.
    let other_order = {
        let k = kernel().await;
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/b.txt"), b"same bytes").await.unwrap();
        ws.link(&p("/b.txt"), &p("/a.txt")).await.unwrap();
        ws.publish(None, None).await.unwrap().state_root
    };
    assert_eq!(
        linked, other_order,
        "the root must not depend on which name was created first"
    );
}

#[tokio::test]
async fn a_copy_is_independent_of_the_group() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src.txt"), b"original").await.unwrap();
    ws.link(&p("/src.txt"), &p("/link.txt")).await.unwrap();
    ws.copy(&p("/src.txt"), &p("/copy.txt")).await.unwrap();
    ws.publish(None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    assert_eq!(ws.link_count(&p("/copy.txt")).await.unwrap(), 1);
    assert_eq!(ws.link_count(&p("/src.txt")).await.unwrap(), 2);

    // Writing to the copy leaves the linked pair alone.
    ws.write_file(&p("/copy.txt"), b"diverged").await.unwrap();
    ws.publish(None, None).await.unwrap();
    assert_eq!(k.read_head_file(&p("/src.txt")).await.unwrap(), b"original");
    assert_eq!(
        k.read_head_file(&p("/link.txt")).await.unwrap(),
        b"original"
    );
    assert_eq!(
        k.read_head_file(&p("/copy.txt")).await.unwrap(),
        b"diverged"
    );
}

#[tokio::test]
async fn link_refuses_directories_symlinks_and_occupied_names() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/file.txt"), b"x").await.unwrap();
    ws.write_file(&p("/taken.txt"), b"y").await.unwrap();
    ws.mkdir(&p("/dir")).await.unwrap();
    ws.symlink(&p("/sym.txt"), "/file.txt").await.unwrap();

    assert!(matches!(
        ws.link(&p("/dir"), &p("/dir-link")).await,
        Err(SfsError::IsADirectory(_))
    ));
    assert!(matches!(
        ws.link(&p("/sym.txt"), &p("/sym-link")).await,
        Err(SfsError::InvalidPath(_))
    ));
    assert!(matches!(
        ws.link(&p("/file.txt"), &p("/taken.txt")).await,
        Err(SfsError::AlreadyExists(_))
    ));
    assert!(matches!(
        ws.link(&p("/missing.txt"), &p("/nope")).await,
        Err(SfsError::NotFound(_))
    ));
}

/// Linking records intent, so `explain` reports it rather than showing an unexplained file.
#[tokio::test]
async fn a_link_is_recorded_as_a_link() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/target.txt"), b"body").await.unwrap();
    ws.publish(None, None).await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.link(&p("/target.txt"), &p("/second-name.txt"))
        .await
        .unwrap();
    ws.publish(None, Some("add a name".into())).await.unwrap();

    let history = k.explain("/second-name.txt", 10).await.unwrap();
    assert_eq!(history[0].kind, "LINK");
}
