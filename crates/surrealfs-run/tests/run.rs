//! Running a command and recording what it changed.
//!
//! The claim under test is not "a command ran". It is that after it ran, the repository can say
//! which command produced which byte — and that the command could not have changed anything the
//! repository does not know about.

use std::fs;
use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_run::{run_in, RunOptions};
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::RepositoryId;

async fn kernel(name: &str) -> Arc<Kernel> {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Arc::new(
        Kernel::open(store, RepositoryId::parse(name).unwrap())
            .await
            .unwrap(),
    )
}

fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

#[tokio::test]
async fn a_command_that_writes_a_file_produces_an_attributed_commit() {
    let k = kernel("run-basic").await;
    let (_dir, root) = project();
    fs::write(root.join("input.txt"), b"seed").unwrap();

    let report = run_in(
        &k,
        &root,
        "/bin/sh",
        &["-c", "echo generated > output.txt"],
        &RunOptions::default(),
    )
    .await
    .unwrap();

    assert!(report.succeeded(), "exit {:?}", report.exit_code);
    assert!(report.confined);
    let commit = report.commit.expect("a run that changed files must commit");

    // The file is in the repository...
    let body = k
        .read_head_file(&surrealfs_types::RepoPath::parse("/output.txt").unwrap())
        .await
        .unwrap();
    assert_eq!(body, b"generated\n");

    // ...and the repository can name the command that produced it. This is the whole point: the
    // provenance edge runs from the byte back to the tool call, not merely to a timestamp.
    let provenance = k.explain("/output.txt", 10).await.unwrap();
    assert!(!provenance.is_empty(), "no provenance recorded");
    assert_eq!(provenance[0].commit, commit);
    assert_eq!(
        provenance[0].tool_name.as_deref(),
        Some("run"),
        "the change must be attributed to a tool call"
    );
    // `Provenance` carries the tool's name and status but not its input, so the command line
    // reaches a reader through the commit message. That is a real limit of the projection rather
    // than of the data — the span holds the full input, as the next assertion shows.
    assert!(
        provenance[0]
            .message
            .as_deref()
            .unwrap_or("")
            .contains("echo generated"),
        "the commit message was {:?}",
        provenance[0].message
    );
    let call = k
        .tool_recent(10)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.tool_name == "run")
        .expect("no run span");
    assert!(
        call.input_preview
            .as_deref()
            .unwrap_or("")
            .contains("echo generated"),
        "the span did not record the command line: {:?}",
        call.input_preview
    );
}

/// A failing command still changed the directory, and that is exactly when the record matters
/// most. The exit code is reported; it does not suppress the commit.
#[tokio::test]
async fn a_failing_command_still_records_what_it_changed() {
    let k = kernel("run-failure").await;
    let (_dir, root) = project();

    let report = run_in(
        &k,
        &root,
        "/bin/sh",
        &["-c", "echo half > partial.txt; exit 3"],
        &RunOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(report.exit_code, Some(3));
    assert!(!report.succeeded());
    assert!(
        report.commit.is_some(),
        "a failed run that wrote files must still be recorded"
    );
    assert_eq!(
        k.read_head_file(&surrealfs_types::RepoPath::parse("/partial.txt").unwrap())
            .await
            .unwrap(),
        b"half\n"
    );

    // The failure is on the span, so a reader can tell a clean run from a broken one.
    let recent = k.tool_recent(10).await.unwrap();
    let call = recent
        .iter()
        .find(|c| c.tool_name == "run")
        .expect("no run span");
    assert!(
        call.error_message.as_deref().unwrap_or("").contains('3'),
        "the exit status is not on the span: {:?}",
        call.error_message
    );
}

/// A command that changes nothing must not manufacture a commit — a history of no-ops is a history
/// nobody can read.
#[tokio::test]
async fn a_command_that_changes_nothing_commits_nothing() {
    let k = kernel("run-noop").await;
    let (_dir, root) = project();
    fs::write(root.join("stable.txt"), b"unchanged").unwrap();

    // Seed the repository so the second run has nothing new to say.
    run_in(
        &k,
        &root,
        "/bin/sh",
        &["-c", "true"],
        &RunOptions::default(),
    )
    .await
    .unwrap();
    let commits_before = k.timeline(50).await.unwrap().len();

    let report = run_in(
        &k,
        &root,
        "/bin/sh",
        &["-c", "cat stable.txt > /dev/null"],
        &RunOptions::default(),
    )
    .await
    .unwrap();

    assert!(report.succeeded());
    assert!(report.commit.is_none(), "a no-op run invented a commit");
    assert_eq!(k.timeline(50).await.unwrap().len(), commits_before);
}

/// Confinement and the record are the same guarantee: a command that could write outside the
/// directory would produce a commit that is silently incomplete.
#[tokio::test]
async fn a_confined_command_cannot_write_outside_the_project() {
    let k = kernel("run-confined").await;
    let (_dir, root) = project();
    let project_dir = root.join("project");
    fs::create_dir(&project_dir).unwrap();
    let outside = root.join("outside.txt");

    let report = run_in(
        &k,
        &project_dir,
        "/bin/sh",
        &[
            "-c",
            &format!("echo inside > in.txt; echo escaped > {}", outside.display()),
        ],
        &RunOptions::default(),
    )
    .await
    .unwrap();

    // The write inside landed and is recorded.
    assert_eq!(
        k.read_head_file(&surrealfs_types::RepoPath::parse("/in.txt").unwrap())
            .await
            .unwrap(),
        b"inside\n"
    );
    // The write outside did not happen at all.
    assert!(
        !outside.exists(),
        "the command escaped the project directory, so the commit is incomplete"
    );
    assert!(!report.succeeded(), "the escape attempt should have failed");
}

/// Turning confinement off is allowed and must be visible afterwards, because a reader of the
/// commit needs to know whether the record can be trusted as complete.
#[tokio::test]
async fn an_unconfined_run_is_marked_as_such() {
    let k = kernel("run-unconfined").await;
    let (_dir, root) = project();

    let report = run_in(
        &k,
        &root,
        "/bin/sh",
        &["-c", "echo x > f.txt"],
        &RunOptions {
            confine: false,
            ..RunOptions::default()
        },
    )
    .await
    .unwrap();

    assert!(report.succeeded());
    assert!(
        !report.confined,
        "an unconfined run must not report itself as confined"
    );
    assert!(report.commit.is_some());
}

/// Two runs, two commits, and `explain` distinguishes them — the query the whole design exists to
/// make possible.
#[tokio::test]
async fn successive_runs_are_individually_attributable() {
    let k = kernel("run-history").await;
    let (_dir, root) = project();

    run_in(
        &k,
        &root,
        "/bin/sh",
        &["-c", "echo first > f.txt"],
        &RunOptions::default(),
    )
    .await
    .unwrap();
    let second = run_in(
        &k,
        &root,
        "/bin/sh",
        &["-c", "echo second > f.txt"],
        &RunOptions::default(),
    )
    .await
    .unwrap();

    let provenance = k.explain("/f.txt", 10).await.unwrap();
    assert!(
        provenance.len() >= 2,
        "expected both runs in the history, got {}",
        provenance.len()
    );
    // Newest first, so the most recent entry is the second run.
    assert_eq!(provenance[0].commit, second.commit.unwrap());
    assert!(provenance[0]
        .message
        .as_deref()
        .unwrap_or("")
        .contains("second"));
    assert!(provenance[1]
        .message
        .as_deref()
        .unwrap_or("")
        .contains("first"));
    assert!(provenance
        .iter()
        .all(|p| p.tool_name.as_deref() == Some("run")));
}
