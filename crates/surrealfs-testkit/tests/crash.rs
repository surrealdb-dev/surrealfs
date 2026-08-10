//! Crash verification: a child process publishes commits and aborts (SIGABRT, no
//! shutdown). After reopen, every acknowledged commit must be complete and verifiable,
//! staged-but-uncommitted content must be invisible, and no partial transaction may
//! appear. This is the Phase 1 "crash/reopen exposes the old or new complete
//! transaction, never a mixture" exit criterion at process granularity.

use std::process::Command;

use surrealfs_kernel::Kernel;
use surrealfs_types::{BranchName, CommitId, RepoPath, RepositoryId, RequestId, StateRootId};

struct Ack {
    commit: CommitId,
    root: StateRootId,
    sequence: u64,
}

fn run_child(db: &std::path::Path, count: usize, mode: &str) -> (Vec<Ack>, Option<String>) {
    let exe = env!("CARGO_BIN_EXE_crash-child");
    let output = Command::new(exe)
        .arg(db)
        .arg(count.to_string())
        .env("SFS_CRASH_MODE", mode)
        .output()
        .expect("spawn crash-child");
    // The child always dies by abort; success is measured by its ACKs, not exit status.
    assert!(!output.status.success(), "child must abort");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut acks = Vec::new();
    let mut staged = None;
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("ACK") => {
                let _i: usize = parts.next().unwrap().parse().unwrap();
                acks.push(Ack {
                    commit: CommitId::parse(parts.next().unwrap()).unwrap(),
                    root: StateRootId::parse(parts.next().unwrap()).unwrap(),
                    sequence: parts.next().unwrap().parse().unwrap(),
                });
            }
            Some("STAGED") => staged = Some(parts.next().unwrap().to_string()),
            _ => {}
        }
    }
    (acks, staged)
}

async fn verify_after_crash(db: std::path::PathBuf, acks: &[Ack]) -> Kernel {
    let repo = RepositoryId::parse("crash").unwrap();
    let store = surrealfs_testkit::reopen_store(db).await.unwrap();
    let kernel = Kernel::open(std::sync::Arc::new(store), repo.clone())
        .await
        .unwrap();

    // Head is exactly the last acknowledged commit.
    let last = acks.last().expect("at least one ack");
    let (head, ns_root, _kv) = kernel.head_state().await.unwrap();
    assert_eq!(head.head, last.commit);
    assert_eq!(head.root, last.root);
    assert_eq!(head.domain_sequence, last.sequence);
    // load_root recomputes the root from the nodes it read, so a successful load is the
    // verification; assert the tree root matches what the commit recorded.
    let (stored_ns, _) = kernel.state_at(&last.commit).await.unwrap();
    assert_eq!(stored_ns, ns_root, "reopened root must verify");

    // Every acknowledged commit is complete: receipt, root, and content all load.
    for (i, ack) in acks.iter().enumerate() {
        let receipt = kernel
            .store()
            .receipt(&repo, &RequestId::parse(&format!("crash-req-{i}")).unwrap())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("receipt for acknowledged commit {i} missing"));
        assert_eq!(receipt.commit, ack.commit);
        // state_at fails if the stored nodes do not re-derive the recorded root.
        let (ns, kv) = kernel.state_at(&ack.commit).await.unwrap();
        let recomputed =
            surrealfs_types::state::root_digest(&ns, &surrealfs_types::state::kv_digest(&kv));
        assert_eq!(recomputed, ack.root, "commit {i} root must verify");
    }

    // Every acknowledged byte is readable.
    for i in 0..acks.len() {
        let path = RepoPath::parse(&format!("/data/f{i}.txt")).unwrap();
        let bytes = kernel.read_head_file(&path).await.unwrap();
        assert_eq!(bytes, format!("payload {i}").into_bytes());
        let value = kernel
            .kv_get_head("crash", &format!("k{i}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value, format!("v{i}").into_bytes());
    }
    kernel
}

#[tokio::test]
async fn acknowledged_commits_survive_abort() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let (acks, _) = run_child(&db, 5, "after_ack");
    assert_eq!(acks.len(), 5);
    verify_after_crash(db, &acks).await;
}

#[tokio::test]
async fn staged_but_uncommitted_content_is_invisible() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let (acks, staged) = run_child(&db, 3, "stage_only");
    assert_eq!(acks.len(), 3);
    let staged = staged.expect("child staged an orphan chunk");

    let kernel = verify_after_crash(db, &acks).await;

    // The orphan chunk is not referenced by any state: head maps know nothing of it.
    let (_, ns_root, kv) = kernel.head_state().await.unwrap();
    let entries = surrealfs_kernel::view::walk_all(
        kernel.store(),
        &RepositoryId::parse("crash").unwrap(),
        &ns_root,
    )
    .await
    .unwrap();
    let referenced: std::collections::BTreeSet<String> = entries
        .iter()
        .filter_map(|(_, e)| match e {
            surrealfs_content::tree::Entry::File { extents, .. } => Some(extents),
            _ => None,
        })
        .flatten()
        .map(|e| e.chunk.to_string())
        .chain(kv.values().map(|d| d.to_string()))
        .collect();
    assert!(
        !referenced.contains(&staged),
        "orphan staged chunk must not be reachable from any root"
    );

    // And the branch did not advance past the last acknowledged commit.
    let head = kernel
        .store()
        .head(&RepositoryId::parse("crash").unwrap(), &BranchName::main())
        .await
        .unwrap();
    assert_eq!(head.domain_sequence, 3);
}

#[tokio::test]
async fn crash_and_continue() {
    // Crash, reopen, keep publishing: sequence continues without gaps or duplicates.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let (acks, _) = run_child(&db, 2, "after_ack");
    let kernel = verify_after_crash(db, &acks).await;

    let mut ws = kernel.workspace().await.unwrap();
    ws.write_file(&RepoPath::parse("/data/after.txt").unwrap(), b"recovered")
        .await
        .unwrap();
    let receipt = ws.publish(None, Some("post-crash".into())).await.unwrap();
    assert_eq!(receipt.domain_sequence, 3);
    let timeline = kernel.timeline(10).await.unwrap();
    assert_eq!(timeline.len(), 4); // genesis + 2 crashed-run commits + this one
}
