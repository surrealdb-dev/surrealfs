//! The host boundary: ingest a real directory, edit inside SurrealFS, apply back out.
//!
//! The property under test throughout is that the project on disk is untouched until
//! `apply` runs, and that `apply` refuses rather than overwrites when the disk has moved.

use std::path::Path;
use std::sync::Arc;

use surrealfs_kernel::host::{self, ApplyOptions, IngestOptions};
use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("host-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// A small project: source files, a nested directory, and an excluded build directory.
fn fixture_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("README.md"), b"# Project\n").unwrap();
    std::fs::write(root.join("src/main.rs"), b"fn main() {}\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), b"pub fn go() {}\n").unwrap();
    std::fs::write(root.join("target/debug/binary"), b"\x7fELF").unwrap();
    std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
    dir
}

fn read(root: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).unwrap()
}

#[tokio::test]
async fn ingest_captures_the_project_and_skips_excluded_directories() {
    let k = kernel().await;
    let project = fixture_project();

    let report = host::ingest(&k, project.path(), &IngestOptions::default())
        .await
        .unwrap();

    assert_eq!(report.files, 3, "README.md, src/main.rs, src/lib.rs");
    assert!(report.commit.is_some());

    // Excluded trees are absent, and reported rather than silently dropped.
    assert!(k.stat_head(&p("/target")).await.unwrap().is_none());
    assert!(k.stat_head(&p("/.git")).await.unwrap().is_none());
    let skipped: Vec<String> = report
        .skipped
        .iter()
        .map(|(path, _)| path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(skipped.contains(&"target".to_string()));
    assert!(skipped.contains(&".git".to_string()));

    // Ingested content round-trips byte for byte.
    assert_eq!(
        k.read_head_file(&p("/src/main.rs")).await.unwrap(),
        b"fn main() {}\n"
    );
}

#[tokio::test]
async fn edits_are_invisible_on_disk_until_apply() {
    let k = kernel().await;
    let project = fixture_project();
    let base = host::ingest(&k, project.path(), &IngestOptions::default())
        .await
        .unwrap()
        .commit
        .unwrap();

    // An agent edits through the workspace.
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/main.rs"), b"fn main() { println!(\"hi\"); }\n")
        .await
        .unwrap();
    ws.write_file(&p("/src/added.rs"), b"pub fn added() {}\n")
        .await
        .unwrap();
    ws.unlink(&p("/src/lib.rs")).await.unwrap();
    let target = ws
        .publish(None, Some("agent edits".into()))
        .await
        .unwrap()
        .commit;

    // The real directory has not moved.
    assert_eq!(read(project.path(), "src/main.rs"), b"fn main() {}\n");
    assert!(project.path().join("src/lib.rs").exists());
    assert!(!project.path().join("src/added.rs").exists());

    // The diff describes exactly the three changes.
    let changes = k.diff_commits(&base, &target).await.unwrap();
    let mut described: Vec<String> = changes
        .iter()
        .map(|c| match c {
            surrealfs_content::tree::Change::Added(p, _) => format!("A {p}"),
            surrealfs_content::tree::Change::Removed(p, _) => format!("D {p}"),
            surrealfs_content::tree::Change::Modified { path, .. } => format!("M {path}"),
        })
        .collect();
    described.sort();
    assert_eq!(
        described,
        vec!["A /src/added.rs", "D /src/lib.rs", "M /src/main.rs"]
    );

    // A dry run still changes nothing.
    let dry = host::apply(
        &k,
        project.path(),
        &base,
        &target,
        &ApplyOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(dry.written.len(), 2);
    assert_eq!(dry.removed.len(), 1);
    assert!(project.path().join("src/lib.rs").exists());

    // Applying for real reconciles the directory exactly.
    let backup = tempfile::tempdir().unwrap();
    let report = host::apply(
        &k,
        project.path(),
        &base,
        &target,
        &ApplyOptions {
            backup_dir: Some(backup.path().to_path_buf()),
            dry_run: false,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        read(project.path(), "src/main.rs"),
        b"fn main() { println!(\"hi\"); }\n"
    );
    assert_eq!(read(project.path(), "src/added.rs"), b"pub fn added() {}\n");
    assert!(!project.path().join("src/lib.rs").exists());
    assert_eq!(read(project.path(), "README.md"), b"# Project\n");

    // Everything overwritten or deleted was backed up first.
    assert_eq!(report.backed_up.len(), 2);
    assert_eq!(read(backup.path(), "src/main.rs"), b"fn main() {}\n");
    assert_eq!(read(backup.path(), "src/lib.rs"), b"pub fn go() {}\n");
}

/// The safety property: if someone edited the project outside SurrealFS, applying would
/// destroy work the system never saw. It must refuse, and refuse before writing anything.
#[tokio::test]
async fn apply_refuses_when_the_host_has_drifted() {
    let k = kernel().await;
    let project = fixture_project();
    let base = host::ingest(&k, project.path(), &IngestOptions::default())
        .await
        .unwrap()
        .commit
        .unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/main.rs"), b"agent version\n")
        .await
        .unwrap();
    ws.write_file(&p("/README.md"), b"# Rewritten\n")
        .await
        .unwrap();
    let target = ws
        .publish(None, Some("agent edits".into()))
        .await
        .unwrap()
        .commit;

    // A human edits one of the same files directly on disk.
    std::fs::write(project.path().join("src/main.rs"), b"human version\n").unwrap();

    let err = host::apply(&k, project.path(), &base, &target, &ApplyOptions::default())
        .await
        .unwrap_err();
    match err {
        SfsError::HostDrift { path, .. } => assert_eq!(path, "/src/main.rs"),
        other => panic!("expected HostDrift, got {other:?}"),
    }

    // Nothing was written: the human's edit survives and the unrelated file is untouched.
    assert_eq!(read(project.path(), "src/main.rs"), b"human version\n");
    assert_eq!(read(project.path(), "README.md"), b"# Project\n");
}

#[tokio::test]
async fn ingest_then_apply_with_no_changes_is_a_no_op() {
    let k = kernel().await;
    let project = fixture_project();
    let base = host::ingest(&k, project.path(), &IngestOptions::default())
        .await
        .unwrap()
        .commit
        .unwrap();

    let report = host::apply(&k, project.path(), &base, &base, &ApplyOptions::default())
        .await
        .unwrap();
    assert!(report.written.is_empty());
    assert!(report.removed.is_empty());
    assert_eq!(read(project.path(), "src/main.rs"), b"fn main() {}\n");
}

/// Hard links survive a full round trip through the repository and back to disk.
///
/// This is where `dofs` deliberately gives up: its coalescer emits one entry per name and the
/// apply side calls `writeFile` for each, so a linked pair arrives as two independent files.
/// Preserving it here is what keeps a pnpm-style tree from silently inflating into copies.
#[cfg(unix)]
#[tokio::test]
async fn hard_links_survive_ingest_and_apply() {
    use std::os::unix::fs::MetadataExt;

    let k = kernel().await;
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("pkg")).unwrap();
    std::fs::write(project.path().join("pkg/real.js"), b"module.exports = 1;\n").unwrap();
    std::fs::hard_link(
        project.path().join("pkg/real.js"),
        project.path().join("pkg/alias.js"),
    )
    .unwrap();
    // A third file with identical bytes but no link relationship.
    std::fs::write(project.path().join("pkg/copy.js"), b"module.exports = 1;\n").unwrap();

    let base = host::ingest(&k, project.path(), &IngestOptions::default())
        .await
        .unwrap()
        .commit
        .unwrap();

    // The link is recorded; the coincidental duplicate is not.
    let mut ws = k.workspace().await.unwrap();
    assert_eq!(ws.link_count(&p("/pkg/real.js")).await.unwrap(), 2);
    assert_eq!(ws.link_count(&p("/pkg/alias.js")).await.unwrap(), 2);
    assert_eq!(
        ws.link_count(&p("/pkg/copy.js")).await.unwrap(),
        1,
        "identical bytes alone must not create a link"
    );
    ws.abort("inspection only").await.unwrap();

    // Apply into a fresh directory and confirm the two names are one file again.
    let restored = tempfile::tempdir().unwrap();
    let empty = k.first_commit().await.unwrap();
    host::apply(&k, restored.path(), &empty, &base, &ApplyOptions::default())
        .await
        .unwrap();

    let real = std::fs::metadata(restored.path().join("pkg/real.js")).unwrap();
    let alias = std::fs::metadata(restored.path().join("pkg/alias.js")).unwrap();
    let copy = std::fs::metadata(restored.path().join("pkg/copy.js")).unwrap();
    assert_eq!(
        real.ino(),
        alias.ino(),
        "the linked pair must be one file on disk again"
    );
    assert_eq!(real.nlink(), 2);
    assert_ne!(copy.ino(), real.ino(), "the duplicate stays independent");
    assert_eq!(
        std::fs::read(restored.path().join("pkg/alias.js")).unwrap(),
        b"module.exports = 1;\n"
    );
}
