//! Phase 1 transaction proof: atomic publication, deterministic receipts,
//! expected-head compare-and-swap, staged-chunk verification, and budgets.

use surrealfs_content::tree::{self, Entry, MemNodes, Meta, TreeWriter};
use surrealfs_store::{CommitPlan, ReceiptOutcome, Store, StoreEngine};
use surrealfs_types::canonical::chunk_digest;
use surrealfs_types::state::{Extent, KvMap, Mutation};
use surrealfs_types::{BranchName, ChunkDigest, RepoPath, RepositoryId, RequestId, SfsError};

fn repo() -> RepositoryId {
    RepositoryId::parse("proof").unwrap()
}

/// Build a plan that writes `/hello.txt` with `bytes` on top of an empty base.
fn hello_plan(
    head: &surrealfs_store::HeadInfo,
    request: &str,
    bytes: &[u8],
) -> (CommitPlan, ChunkDigest) {
    let path = RepoPath::parse("/hello.txt").unwrap();
    let digest = ChunkDigest(chunk_digest(bytes));
    let extents = vec![Extent {
        file_offset: 0,
        length: bytes.len() as u64,
        chunk: digest.clone(),
    }];
    let mem = MemNodes::default();
    let mut writer = TreeWriter::new(&mem);
    let namespace_root = writer
        .insert(
            &tree::empty_root(),
            &path,
            Entry::File {
                meta: Meta::file(),
                size: bytes.len() as u64,
                extents: extents.clone(),
                links: Vec::new(),
            },
        )
        .unwrap();
    let new_nodes = writer.into_new_nodes();
    let plan = CommitPlan {
        repository: repo(),
        branch: BranchName::main(),
        request_id: RequestId::parse(request).unwrap(),
        expected_head: head.head.clone(),
        base_root: head.root.clone(),
        namespace_root,
        new_nodes,
        kv: KvMap::new(),
        mutations: vec![Mutation::WriteFile {
            path,
            size: bytes.len() as u64,
            content: extents,
        }],
        author_span: None,
        workspace: None,
        message: Some("write hello".into()),
    };
    (plan, digest)
}

#[tokio::test]
async fn atomic_publication_and_receipt() {
    let store = Store::open(StoreEngine::Memory).await.unwrap();
    let head = store.ensure_repository(&repo()).await.unwrap();
    assert_eq!(head.domain_sequence, 0);

    let body = b"hello world".to_vec();
    let (plan, digest) = hello_plan(&head, "req-1", &body);
    store
        .stage_chunks(&repo(), &[(digest.clone(), body.clone())])
        .await
        .unwrap();

    let receipt = store.publish(&plan).await.unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::Applied);
    assert_eq!(receipt.domain_sequence, 1);
    assert_eq!(receipt.previous_head, head.head);

    // State, provenance, head, and receipt all landed atomically.
    let new_head = store.head(&repo(), &BranchName::main()).await.unwrap();
    assert_eq!(new_head.head, receipt.commit);
    assert_eq!(new_head.root, receipt.state_root);
    // load_root re-derives the root from what it read; a mismatch would be corruption.
    let (ns_root, _kv) = store.load_root(&repo(), &new_head.root).await.unwrap();
    assert_eq!(ns_root, plan.namespace_root);
    let fetched = store.fetch_chunk(&repo(), &digest).await.unwrap();
    assert_eq!(fetched, body);
    let timeline = store.timeline(&repo(), 10).await.unwrap();
    assert_eq!(timeline.len(), 2);
    assert_eq!(timeline[0].commit, receipt.commit);
    let mutations = store
        .mutations_of_commit(&repo(), &receipt.commit)
        .await
        .unwrap();
    assert_eq!(mutations.len(), 1);
}

#[tokio::test]
async fn same_request_returns_same_receipt_and_changed_input_is_rejected() {
    let store = Store::open(StoreEngine::Memory).await.unwrap();
    let head = store.ensure_repository(&repo()).await.unwrap();

    let body = b"idempotent".to_vec();
    let (plan, digest) = hello_plan(&head, "req-same", &body);
    store
        .stage_chunks(&repo(), &[(digest, body)])
        .await
        .unwrap();

    let first = store.publish(&plan).await.unwrap();
    assert_eq!(first.outcome, ReceiptOutcome::Applied);

    // Same request id + same command: replayed receipt, no new commit.
    let replay = store.publish(&plan).await.unwrap();
    assert_eq!(replay.outcome, ReceiptOutcome::Replayed);
    assert_eq!(replay.commit, first.commit);
    assert_eq!(replay.domain_sequence, first.domain_sequence);
    assert_eq!(
        store.head(&repo(), &BranchName::main()).await.unwrap().head,
        first.commit
    );

    // Same request id + different command: typed rejection.
    let body2 = b"tampered".to_vec();
    let (mut plan2, digest2) = hello_plan(&head, "req-same", &body2);
    plan2.expected_head = first.commit.clone();
    store
        .stage_chunks(&repo(), &[(digest2, body2)])
        .await
        .unwrap();
    match store.publish(&plan2).await {
        Err(SfsError::RequestMismatch { request_id }) => assert_eq!(request_id, "req-same"),
        other => panic!("expected RequestMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn expected_head_conflict_is_typed() {
    let store = Store::open(StoreEngine::Memory).await.unwrap();
    let head = store.ensure_repository(&repo()).await.unwrap();

    let body = b"first".to_vec();
    let (plan, digest) = hello_plan(&head, "req-a", &body);
    store
        .stage_chunks(&repo(), &[(digest, body)])
        .await
        .unwrap();
    let first = store.publish(&plan).await.unwrap();

    // A second plan still based on the stale head must fail with a typed conflict.
    let body2 = b"second".to_vec();
    let (plan2, digest2) = hello_plan(&head, "req-b", &body2);
    store
        .stage_chunks(&repo(), &[(digest2, body2)])
        .await
        .unwrap();
    match store.publish(&plan2).await {
        Err(SfsError::HeadConflict {
            expected, actual, ..
        }) => {
            assert_eq!(expected, head.head.to_string());
            assert_eq!(actual, first.commit.to_string());
        }
        other => panic!("expected HeadConflict, got {other:?}"),
    }
}

#[tokio::test]
async fn unstaged_chunks_are_rejected() {
    let store = Store::open(StoreEngine::Memory).await.unwrap();
    let head = store.ensure_repository(&repo()).await.unwrap();
    let (plan, _digest) = hello_plan(&head, "req-unstaged", b"never staged");
    match store.publish(&plan).await {
        Err(SfsError::Corruption(msg)) => assert!(msg.contains("staged")),
        other => panic!("expected Corruption for missing chunk, got {other:?}"),
    }
    // The failed publication left no state behind.
    let after = store.head(&repo(), &BranchName::main()).await.unwrap();
    assert_eq!(after.head, head.head);
    assert!(store.timeline(&repo(), 10).await.unwrap().len() == 1);
}

#[tokio::test]
async fn concurrent_head_campaign_has_one_winner_per_round() {
    let store = std::sync::Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    let mut head = store.ensure_repository(&repo()).await.unwrap();

    // 100 randomized concurrent campaigns: every round races 4 writers on the same
    // expected head; exactly one must win, the rest must fail with a typed conflict
    // (or replay-mismatch style errors, never partial state).
    for round in 0..100 {
        let mut handles = Vec::new();
        for writer in 0..4 {
            let store = store.clone();
            let head = head.clone();
            handles.push(tokio::spawn(async move {
                let body = format!("round {round} writer {writer}").into_bytes();
                let (plan, digest) = hello_plan(&head, &format!("req-{round}-{writer}"), &body);
                store
                    .stage_chunks(&RepositoryId::parse("proof").unwrap(), &[(digest, body)])
                    .await
                    .unwrap();
                store.publish(&plan).await
            }));
        }
        let mut winners = 0;
        let mut conflicts = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(receipt) => {
                    assert_eq!(receipt.domain_sequence, head.domain_sequence + 1);
                    winners += 1;
                }
                Err(SfsError::HeadConflict { .. }) => conflicts += 1,
                Err(other) => panic!("round {round}: unexpected error {other:?}"),
            }
        }
        assert_eq!(winners, 1, "round {round} must have exactly one winner");
        assert_eq!(conflicts, 3, "round {round} must reject the other writers");
        head = store.head(&repo(), &BranchName::main()).await.unwrap();
        assert_eq!(head.domain_sequence, (round + 1) as u64);
    }
}

#[tokio::test]
async fn over_budget_is_rejected_before_transaction() {
    let store = Store::open(StoreEngine::Memory).await.unwrap();
    let head = store.ensure_repository(&repo()).await.unwrap();
    let mut plan = hello_plan(&head, "req-budget", b"x").0;
    plan.mutations = (0..10_001)
        .map(|i| Mutation::KvDelete {
            namespace: "n".into(),
            key: format!("k{i}"),
        })
        .collect();
    match store.publish(&plan).await {
        Err(SfsError::OverBudget(_)) => {}
        other => panic!("expected OverBudget, got {other:?}"),
    }
}

async fn reopen_with_retry(path: std::path::PathBuf) -> Store {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match Store::open(StoreEngine::SurrealKv(path.clone())).await {
            Ok(store) => return store,
            Err(SfsError::Storage(msg))
                if msg.contains("locked") && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(other) => panic!("reopen failed: {other:?}"),
        }
    }
}

#[tokio::test]
async fn surrealkv_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");

    let body = b"durable bytes".to_vec();
    let (receipt, digest, plan_ns_root) = {
        let store = Store::open(StoreEngine::SurrealKv(db_path.clone()))
            .await
            .unwrap();
        let head = store.ensure_repository(&repo()).await.unwrap();
        let (plan, digest) = hello_plan(&head, "req-durable", &body);
        store
            .stage_chunks(&repo(), &[(digest.clone(), body.clone())])
            .await
            .unwrap();
        (
            store.publish(&plan).await.unwrap(),
            digest,
            plan.namespace_root.clone(),
        )
        // Store dropped here; drop-based shutdown (upstream awaited-shutdown gap is a
        // known Phase 3 deliverable — see COMPATIBILITY.md).
    };

    // Drop-based shutdown releases the directory lock asynchronously (the missing
    // awaited-shutdown API is owned upstream work; see COMPATIBILITY.md). Poll-open
    // within a bounded window, as the runtime lifecycle will.
    let store = reopen_with_retry(db_path).await;
    let head = store.ensure_repository(&repo()).await.unwrap();
    assert_eq!(head.head, receipt.commit);
    assert_eq!(head.root, receipt.state_root);
    let (ns_root, _kv) = store.load_root(&repo(), &head.root).await.unwrap();
    assert_eq!(ns_root, plan_ns_root);
    assert_eq!(store.fetch_chunk(&repo(), &digest).await.unwrap(), body);
    // Ambiguous-outcome resolution: the receipt is queryable after reopen.
    let stored = store
        .receipt(&repo(), &RequestId::parse("req-durable").unwrap())
        .await
        .unwrap()
        .expect("receipt must survive reopen");
    assert_eq!(stored.commit, receipt.commit);
}
