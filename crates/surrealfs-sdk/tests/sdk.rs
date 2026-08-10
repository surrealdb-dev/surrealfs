//! SDK lifecycle: open/create by id, exclusive ownership, persistence across reopen.

use surrealfs_sdk::{SfsError, SfsOptions, Surrealfs};

#[tokio::test]
async fn ephemeral_flow() {
    let sfs = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    sfs.fs().write_file("/a.txt", b"alpha").await.unwrap();
    sfs.kv().set("k", b"v").await.unwrap();
    assert_eq!(sfs.fs().read_file("/a.txt").await.unwrap(), b"alpha");
    assert_eq!(sfs.kv().get("k").await.unwrap().unwrap(), b"v");
    assert_eq!(sfs.kv().keys("").await.unwrap(), vec!["k"]);
    sfs.close().await.unwrap();
}

#[tokio::test]
async fn workspace_multi_op_atomic_publication() {
    let sfs = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    let tool = sfs.tools().start("refactor", Some("input")).await.unwrap();

    let mut ws = sfs.workspace().await.unwrap();
    ws.attribute_to(tool.span_key());
    ws.write_file(&p("/src/one.rs"), b"one").await.unwrap();
    ws.write_file(&p("/src/two.rs"), b"two").await.unwrap();
    ws.kv_set("default", "progress", b"done").unwrap();
    let receipt = ws.publish(None, Some("refactor".into())).await.unwrap();
    sfs.tools().success(&tool, Some("ok")).await.unwrap();

    let timeline = sfs.timeline(10).await.unwrap();
    assert_eq!(timeline[0].commit, receipt.commit);
    assert_eq!(timeline[0].mutation_count, 3);
    sfs.close().await.unwrap();
}

fn p(s: &str) -> surrealfs_sdk::RepoPath {
    surrealfs_sdk::RepoPath::parse(s).unwrap()
}

#[tokio::test]
async fn persistent_reopen_and_exclusive_lock() {
    let dir = tempfile::tempdir().unwrap();

    let (commit, root) = {
        let sfs = Surrealfs::open(SfsOptions::with_id_in(dir.path(), "agent-1"))
            .await
            .unwrap();

        // A second owner is refused while the first is open.
        match Surrealfs::open(SfsOptions::with_id_in(dir.path(), "agent-1")).await {
            Err(SfsError::StoreLocked(_)) => {}
            other => panic!("expected StoreLocked, got {:?}", other.map(|_| "opened")),
        }

        sfs.fs()
            .write_file("/report/summary.md", b"# Summary")
            .await
            .unwrap();
        sfs.kv().set("stage", b"reported").await.unwrap();
        sfs.tools()
            .record("summarize", None, Some("ok"))
            .await
            .unwrap();
        let head = sfs.head().await.unwrap();
        sfs.close().await.unwrap();
        head
    };

    // Reopen: same head, same verified root, same bytes.
    let sfs = Surrealfs::open(SfsOptions::with_id_in(dir.path(), "agent-1"))
        .await
        .unwrap();
    let (head_commit, head_root) = sfs.head().await.unwrap();
    assert_eq!(head_commit, commit);
    assert_eq!(head_root, root);
    assert_eq!(
        sfs.fs().read_file("/report/summary.md").await.unwrap(),
        b"# Summary"
    );
    assert_eq!(sfs.kv().get("stage").await.unwrap().unwrap(), b"reported");
    let tools = sfs.tools().recent(10).await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool_name, "summarize");
    sfs.close().await.unwrap();
}
