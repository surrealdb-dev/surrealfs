//! Kernel semantics: private staging, publish/abort, and reference-model cross-checks.

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_model::RefModel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

async fn kernel() -> Kernel {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Kernel::open(store, RepositoryId::parse("kernel-test").unwrap())
        .await
        .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

#[tokio::test]
async fn staged_changes_are_invisible_until_publish() {
    let k = kernel().await;
    let (base, base_ns, _) = k.head_state().await.unwrap();
    assert!(k.list_head(&p("/")).await.unwrap().is_empty());
    let _ = base_ns;

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/notes/plan.md"), b"draft").await.unwrap();
    ws.kv_set("app", "phase", b"2").unwrap();

    // Another reader still sees the empty base.
    let (head_now, _, kv_now) = k.head_state().await.unwrap();
    assert_eq!(head_now.head, base.head);
    assert!(k.list_head(&p("/")).await.unwrap().is_empty());
    assert!(kv_now.is_empty());

    // Workspace sees its own staged state.
    assert_eq!(ws.read_file(&p("/notes/plan.md")).await.unwrap(), b"draft");
    assert_eq!(ws.kv_get("app", "phase").await.unwrap().unwrap(), b"2");

    let receipt = ws.publish(None, Some("first".into())).await.unwrap();
    let (head_after, _, _) = k.head_state().await.unwrap();
    assert_eq!(head_after.head, receipt.commit);
    let root_entries = k.list_head(&p("/")).await.unwrap();
    assert_eq!(root_entries.len(), 1);
    assert_eq!(root_entries[0].name, "notes");
    assert!(root_entries[0].is_dir);
    assert_eq!(
        k.read_head_file(&p("/notes/plan.md")).await.unwrap(),
        b"draft"
    );
    assert_eq!(k.kv_get_head("app", "phase").await.unwrap().unwrap(), b"2");
}

#[tokio::test]
async fn abort_leaves_no_logical_state() {
    let k = kernel().await;
    let (base, _, _) = k.head_state().await.unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/tmp.txt"), b"scratch").await.unwrap();
    ws.abort("test abort").await.unwrap();
    assert!(matches!(
        ws.write_file(&p("/tmp.txt"), b"more").await,
        Err(SfsError::WorkspaceClosed { .. })
    ));

    let (head, _, _) = k.head_state().await.unwrap();
    assert_eq!(head.head, base.head);
    assert!(k.list_head(&p("/")).await.unwrap().is_empty());
}

#[tokio::test]
async fn filesystem_error_semantics() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();

    ws.write_file(&p("/a/b/f.txt"), b"x").await.unwrap();
    // Writing over a directory fails.
    assert!(matches!(
        ws.write_file(&p("/a/b"), b"nope").await,
        Err(SfsError::IsADirectory(_))
    ));
    // A file cannot be a parent.
    assert!(matches!(
        ws.write_file(&p("/a/b/f.txt/child"), b"nope").await,
        Err(SfsError::NotADirectory(_))
    ));
    // rmdir of a non-empty directory fails; unlink then rmdir chain works.
    assert!(matches!(
        ws.rmdir(&p("/a/b")).await,
        Err(SfsError::DirectoryNotEmpty(_))
    ));
    ws.unlink(&p("/a/b/f.txt")).await.unwrap();
    ws.rmdir(&p("/a/b")).await.unwrap();
    ws.rmdir(&p("/a")).await.unwrap();
    // Nothing left: publishing an effectively-empty state is still a valid commit.
    let receipt = ws.publish(None, None).await.unwrap();
    let (ns_root, _) = k.state_at(&receipt.commit).await.unwrap();
    assert_eq!(ns_root, surrealfs_content::tree::empty_root());

    // KV delete of a missing key is typed.
    let mut ws = k.workspace().await.unwrap();
    assert!(matches!(
        ws.kv_delete("app", "missing"),
        Err(SfsError::NotFound(_))
    ));
    ws.abort("done").await.unwrap();
}

#[tokio::test]
async fn list_dir_and_stat() {
    let k = kernel().await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/main.rs"), b"fn main() {}")
        .await
        .unwrap();
    ws.write_file(&p("/src/lib.rs"), b"pub fn x() {}")
        .await
        .unwrap();
    ws.mkdir(&p("/src/tests")).await.unwrap();
    ws.write_file(&p("/readme.md"), b"# hi").await.unwrap();

    let root = ws.list_dir(&p("/")).await.unwrap();
    let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["readme.md", "src"]);

    let src = ws.list_dir(&p("/src")).await.unwrap();
    let names: Vec<_> = src.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["lib.rs", "main.rs", "tests"]);
    assert!(!src[0].is_dir);
    assert!(src[2].is_dir);

    let entry = ws.stat(&p("/src/main.rs")).await.unwrap().unwrap();
    assert!(matches!(
        entry,
        surrealfs_content::tree::Entry::File { size: 12, .. }
    ));
    assert!(ws.stat(&p("/nope")).await.unwrap().is_none());
}

/// Deterministic pseudo-random op sequence applied to both the kernel workspace and the
/// independent reference model; roots must match after every publish.
#[tokio::test]
async fn generated_sequence_matches_reference_model() {
    let k = kernel().await;
    let mut model = RefModel::new();
    let mut rng: u64 = 0x5eed_cafe_f00d_0001;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let dirs = ["/d1", "/d2", "/d1/sub"];
    let files = ["/d1/a.txt", "/d1/sub/b.bin", "/d2/c.log", "/top.txt"];
    let kv_keys = ["alpha", "beta", "gamma"];

    for round in 0..10 {
        let mut ws = k.workspace().await.unwrap();
        for step in 0..20 {
            let r = next();
            match r % 6 {
                0 => {
                    let path = p(files[(r >> 8) as usize % files.len()]);
                    let body = format!("round {round} step {step} value {r}").into_bytes();
                    let kernel_result = ws.write_file(&path, &body).await;
                    let model_result = model.write_file(&path, &body);
                    assert_eq!(kernel_result.is_ok(), model_result.is_ok());
                }
                1 => {
                    let path = p(dirs[(r >> 8) as usize % dirs.len()]);
                    let kernel_result = ws.mkdir(&path).await;
                    let model_result = model.mkdir(&path);
                    assert_eq!(kernel_result.is_ok(), model_result.is_ok());
                }
                2 => {
                    let path = p(files[(r >> 8) as usize % files.len()]);
                    let kernel_result = ws.unlink(&path).await;
                    let model_result = model.unlink(&path);
                    assert_eq!(kernel_result.is_ok(), model_result.is_ok());
                }
                3 => {
                    let path = p(dirs[(r >> 8) as usize % dirs.len()]);
                    let kernel_result = ws.rmdir(&path).await;
                    let model_result = model.rmdir(&path);
                    assert_eq!(kernel_result.is_ok(), model_result.is_ok());
                }
                4 => {
                    let key = kv_keys[(r >> 8) as usize % kv_keys.len()];
                    let value = format!("v{r}").into_bytes();
                    ws.kv_set("gen", key, &value).unwrap();
                    model.kv_set("gen", key, &value);
                }
                _ => {
                    let key = kv_keys[(r >> 8) as usize % kv_keys.len()];
                    let kernel_result = ws.kv_delete("gen", key);
                    let model_result = model.kv_delete("gen", key);
                    assert_eq!(kernel_result.is_ok(), model_result.is_ok());
                }
            }
        }
        let receipt = ws
            .publish(None, Some(format!("round {round}")))
            .await
            .unwrap();
        assert_eq!(
            receipt.state_root.as_str(),
            model.root_digest().unwrap().as_str(),
            "kernel and reference model diverged in round {round}"
        );
        // And the stored state re-verifies: load_root recomputes the root from what it read.
        let (ns_root, _) = k.state_at(&receipt.commit).await.unwrap();
        assert_eq!(ns_root, model.namespace_root().unwrap());
    }
}

#[tokio::test]
async fn tool_attribution_and_atomic_publication() {
    let k = kernel().await;
    let span = k
        .tool_start("write_config", Some("{\"path\": \"/cfg\"}".into()))
        .await
        .unwrap();

    let mut ws = k.workspace().await.unwrap();
    ws.attribute_to(&span);
    ws.write_file(&p("/cfg.toml"), b"debug = true")
        .await
        .unwrap();
    ws.kv_set("meta", "last_tool", b"write_config").unwrap();
    let receipt = ws.publish(None, Some("tool commit".into())).await.unwrap();

    k.tool_finish(&span, Some("ok".into()), None).await.unwrap();

    // File + KV landed in one commit with two mutations.
    let mutations = k
        .store()
        .mutations_of_commit(k.repo(), &receipt.commit)
        .await
        .unwrap();
    assert_eq!(mutations.len(), 2);

    let recent = k.tool_recent(10).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].tool_name, "write_config");
    assert_eq!(recent[0].status, "SUCCEEDED");

    // Interrupted tool calls stay RUNNING (never fabricated success/failure).
    let span2 = k.tool_start("crashy", None).await.unwrap();
    let _ = span2;
    let recent = k.tool_recent(10).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].status, "RUNNING");
}

/// The property M1a exists for, asserted through the real store rather than the in-memory
/// tree: after a large repository is committed, changing one file must persist a number of
/// nodes proportional to the change, not to the size of the namespace.
#[tokio::test]
async fn commit_cost_is_proportional_to_change_not_repository_size() {
    let k = kernel().await;

    // A namespace with 400 files across 40 directories.
    let mut ws = k.workspace().await.unwrap();
    for d in 0..40 {
        for f in 0..10 {
            ws.write_file(&p(&format!("/src/mod{d}/file{f}.rs")), b"initial")
                .await
                .unwrap();
        }
    }
    ws.publish(None, Some("bulk import".into())).await.unwrap();

    let after_import = k.store().state_node_count(k.repo()).await.unwrap();
    assert!(
        after_import >= 40,
        "expected a node per directory, got {after_import}"
    );

    // Touch exactly one file.
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/src/mod20/file5.rs"), b"changed")
        .await
        .unwrap();
    ws.publish(None, Some("one file".into())).await.unwrap();

    let after_edit = k.store().state_node_count(k.repo()).await.unwrap();
    let written = after_edit - after_import;
    assert!(
        written <= 3,
        "editing one file in a 400-file repository stored {written} new nodes; only the \
         root, /src and /src/mod20 should change"
    );

    // The other 399 files still read correctly through the shared subtrees.
    assert_eq!(
        k.read_head_file(&p("/src/mod0/file0.rs")).await.unwrap(),
        b"initial"
    );
    assert_eq!(
        k.read_head_file(&p("/src/mod20/file5.rs")).await.unwrap(),
        b"changed"
    );
}

/// Tool statistics: the last of AgentFS's get/recent/statistics trio.
///
/// The property worth guarding is that an interrupted call is counted but kept out of the
/// duration aggregates. A call that never finished has no duration, and treating it as instant
/// would quietly flatter every average.
#[tokio::test]
async fn tool_statistics_aggregate_outcomes_and_exclude_unfinished_calls() {
    let k = kernel().await;

    for i in 0..3 {
        let span = k
            .tool_start("fs_write", Some(format!("call {i}")))
            .await
            .unwrap();
        k.tool_finish(&span, Some("ok".into()), None).await.unwrap();
    }
    let failing = k.tool_start("fs_write", None).await.unwrap();
    k.tool_finish(&failing, None, Some("disk on fire".into()))
        .await
        .unwrap();

    let reader = k.tool_start("fs_read", None).await.unwrap();
    k.tool_finish(&reader, Some("ok".into()), None)
        .await
        .unwrap();

    // Started and never finished, as an interrupted run leaves behind.
    let _abandoned = k.tool_start("fs_read", None).await.unwrap();

    let stats = k.tool_stats().await.unwrap();
    assert_eq!(stats.len(), 2);

    // Busiest tool first.
    let writes = &stats[0];
    assert_eq!(writes.tool_name, "fs_write");
    assert_eq!(writes.calls, 4);
    assert_eq!(writes.succeeded, 3);
    assert_eq!(writes.failed, 1);
    assert_eq!(writes.running, 0);
    assert!(writes.avg_duration_ms.is_some());

    let reads = &stats[1];
    assert_eq!(reads.tool_name, "fs_read");
    assert_eq!(reads.calls, 2);
    assert_eq!(reads.succeeded, 1);
    assert_eq!(reads.failed, 0);
    assert_eq!(reads.running, 1, "the abandoned call is counted");
    // One finished call, so min and max describe it and the unfinished call contributes
    // nothing rather than a zero.
    assert_eq!(reads.min_duration_ms, reads.max_duration_ms);
    assert!(reads.min_duration_ms.is_some());
}
