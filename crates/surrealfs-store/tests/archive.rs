//! Portable session archives: export, verify, import.
//!
//! The point of an explicit archive rather than "copy the database file" is that everything in
//! it is checked on the way back in. These tests cover both halves: a good archive reproduces
//! the session exactly, and a damaged one is refused before it can put anything in a store.

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_store::{read_archive, Store, StoreEngine};
use surrealfs_types::{BranchName, RepoPath, RepositoryId, SfsError};

async fn kernel(name: &str) -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse(name).unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// A session with content, history, branches, savepoints, and provenance.
async fn build_session(k: &Kernel) {
    let span = k
        .tool_start("fs_write", Some("/src/main.rs".into()))
        .await
        .unwrap();
    let mut ws = k.workspace().await.unwrap();
    ws.attribute_to(&span);
    ws.write_file(&p("/src/main.rs"), b"fn main() {}\n")
        .await
        .unwrap();
    ws.write_file(&p("/README.md"), b"# Project\n")
        .await
        .unwrap();
    ws.kv_set("app", "phase", b"m3").unwrap();
    ws.publish(None, Some("initial".into())).await.unwrap();
    k.tool_finish(&span, Some("ok".into()), None).await.unwrap();

    k.savepoint("known-good", None, Some("before edits".into()))
        .await
        .unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/main.rs"), b"fn main() { work(); }\n")
        .await
        .unwrap();
    ws.link(&p("/README.md"), &p("/docs.md")).await.unwrap();
    ws.publish(None, Some("second".into())).await.unwrap();

    let head = k.head().await.unwrap().head;
    k.fork(&BranchName::parse("experiment").unwrap(), &head, None)
        .await
        .unwrap();
}

async fn export_to_bytes(k: &Kernel) -> Vec<u8> {
    let mut buf = Vec::new();
    k.store().export_archive(k.repo(), &mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn a_session_survives_export_and_import() {
    let source = kernel("archive-src").await;
    build_session(&source).await;
    let source_head = source.head().await.unwrap();
    let archive = export_to_bytes(&source).await;

    // Import into a completely separate store, as sharing a session would.
    let target = kernel("archive-dst").await;
    let contents = read_archive(&archive[..]).unwrap();
    let stats = target
        .store()
        .import_archive(target.repo(), contents)
        .await
        .unwrap();
    assert!(stats.chunks > 0 && stats.commits > 0);

    // Content is byte-identical, at the same root.
    let restored_head = target
        .store()
        .head(target.repo(), &BranchName::main())
        .await;
    // `main` is restored from the archive's branch record.
    let restored_head = restored_head.unwrap();
    assert_eq!(restored_head.head, source_head.head);
    assert_eq!(restored_head.root, source_head.root);
    assert_eq!(
        target.read_head_file(&p("/src/main.rs")).await.unwrap(),
        b"fn main() { work(); }\n"
    );
    assert_eq!(
        target.kv_get_head("app", "phase").await.unwrap().unwrap(),
        b"m3"
    );

    // History, savepoints, branches, and hard links all come across.
    assert_eq!(target.timeline(10).await.unwrap().len(), 3);
    assert_eq!(
        target.resolve_savepoint("known-good").await.unwrap(),
        source.resolve_savepoint("known-good").await.unwrap()
    );
    let branches: Vec<String> = target
        .branches()
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert!(branches.contains(&"experiment".to_string()));
    let mut ws = target.workspace().await.unwrap();
    assert_eq!(ws.link_count(&p("/README.md")).await.unwrap(), 2);
    ws.abort("inspection").await.unwrap();
}

/// Provenance is most of the point of this system, so it has to be in the archive.
#[tokio::test]
async fn provenance_survives_the_round_trip() {
    let source = kernel("prov-src").await;
    build_session(&source).await;
    let archive = export_to_bytes(&source).await;

    let target = kernel("prov-dst").await;
    target
        .store()
        .import_archive(target.repo(), read_archive(&archive[..]).unwrap())
        .await
        .unwrap();

    let history = target.explain("/src/main.rs", 10).await.unwrap();
    assert_eq!(
        history.len(),
        2,
        "both writes to the path are still recorded"
    );
    // The tool call that caused the first write came across with it.
    assert!(history
        .iter()
        .any(|h| h.tool_name.as_deref() == Some("fs_write")));

    let calls = target.tool_recent(10).await.unwrap();
    assert!(calls.iter().any(|c| c.tool_name == "fs_write"));
}

#[tokio::test]
async fn importing_the_same_archive_twice_is_idempotent() {
    let source = kernel("idem-src").await;
    build_session(&source).await;
    let archive = export_to_bytes(&source).await;

    let target = kernel("idem-dst").await;
    for _ in 0..2 {
        target
            .store()
            .import_archive(target.repo(), read_archive(&archive[..]).unwrap())
            .await
            .unwrap();
    }
    // Records are content-addressed, so a second import overwrites rather than duplicating.
    assert_eq!(target.timeline(20).await.unwrap().len(), 3);
    assert_eq!(
        target.read_head_file(&p("/src/main.rs")).await.unwrap(),
        b"fn main() { work(); }\n"
    );
}

#[tokio::test]
async fn a_corrupted_archive_is_refused() {
    let source = kernel("corrupt-src").await;
    build_session(&source).await;
    let archive = export_to_bytes(&source).await;

    // Flip a byte in the middle, which lands in chunk or node payload.
    let mut damaged = archive.clone();
    let midpoint = damaged.len() / 2;
    damaged[midpoint] ^= 0xff;
    match read_archive(&damaged[..]) {
        Err(SfsError::Corruption(_)) | Err(SfsError::Storage(_)) | Err(SfsError::Io(_)) => {}
        Err(other) => panic!("expected a corruption error, got {other:?}"),
        Ok(_) => panic!("a damaged archive must not verify"),
    }

    // Truncation is caught too, rather than importing a partial session.
    let truncated = &archive[..archive.len() / 2];
    assert!(read_archive(truncated).is_err());

    // And something that is not an archive at all.
    assert!(matches!(
        read_archive(&b"not an archive at all, just some bytes"[..]),
        Err(SfsError::Corruption(_))
    ));
}

#[tokio::test]
async fn an_empty_repository_round_trips() {
    let source = kernel("empty-src").await;
    let archive = export_to_bytes(&source).await;

    let target = kernel("empty-dst").await;
    target
        .store()
        .import_archive(target.repo(), read_archive(&archive[..]).unwrap())
        .await
        .unwrap();
    assert!(target.list_head(&p("/")).await.unwrap().is_empty());
}

/// Publication times must survive a round trip, or every time reference built on the history
/// silently points somewhere else.
///
/// Import used to stamp `time::now()` on every commit, which relocated an entire history to the
/// moment of import. Nothing failed visibly — the commits, roots, and content were all correct —
/// so this is exactly the kind of loss that only an explicit assertion catches.
#[tokio::test]
async fn commit_timestamps_survive_a_round_trip() {
    let source = kernel("archive-times-src").await;
    let mut ws = source.workspace().await.unwrap();
    ws.write_file(&p("/f.txt"), b"content").await.unwrap();
    let receipt = ws.publish(None, Some("first".into())).await.unwrap();
    let original = source
        .committed_at(&receipt.commit)
        .await
        .unwrap()
        .expect("a published commit has a time");

    let mut bytes = Vec::new();
    source
        .store()
        .export_archive(source.repo(), &mut bytes)
        .await
        .unwrap();

    let target = kernel("archive-times-dst").await;
    target
        .store()
        .import_archive(target.repo(), read_archive(&bytes[..]).unwrap())
        .await
        .unwrap();

    let imported = target
        .committed_at(&receipt.commit)
        .await
        .unwrap()
        .expect("the imported commit kept its identity");
    assert_eq!(
        imported, original,
        "import relocated the commit in time, so any time reference into this history is wrong"
    );

    // And the resolver agrees: the original moment still finds the original commit.
    let at = surrealfs_types::time::parse_rfc3339(&original).unwrap();
    assert_eq!(
        target.commit_at_or_before(at).await.unwrap(),
        receipt.commit
    );
}
