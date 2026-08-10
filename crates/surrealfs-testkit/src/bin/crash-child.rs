//! Crash-harness child: publishes commits against a SurrealKV store and then kills
//! itself at a controlled point via `std::process::abort()` (no destructors, no flush).
//!
//! Usage: crash-child <db-path> <publish-count>
//! Env:   SFS_CRASH_MODE = after_ack | stage_only
//!   after_ack:  publish <count> commits, ACK each on stdout, then abort.
//!   stage_only: publish <count> commits, then stage orphan chunks for an extra commit
//!               and abort BEFORE publishing it (the classic staged-not-committed window).

use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, RequestId};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let db_path: std::path::PathBuf = args.next().expect("db path").into();
    let count: usize = args.next().expect("publish count").parse().expect("count");
    let mode = std::env::var("SFS_CRASH_MODE").unwrap_or_else(|_| "after_ack".into());

    let repo = RepositoryId::parse("crash").unwrap();
    let store = Arc::new(Store::open(StoreEngine::SurrealKv(db_path)).await.unwrap());
    let kernel = Kernel::open(store.clone(), repo.clone()).await.unwrap();

    for i in 0..count {
        let mut ws = kernel.workspace().await.unwrap();
        let path = RepoPath::parse(&format!("/data/f{i}.txt")).unwrap();
        ws.write_file(&path, format!("payload {i}").as_bytes())
            .await
            .unwrap();
        ws.kv_set("crash", &format!("k{i}"), format!("v{i}").as_bytes())
            .unwrap();
        let receipt = ws
            .publish(
                Some(&RequestId::parse(&format!("crash-req-{i}")).unwrap()),
                Some(format!("commit {i}")),
            )
            .await
            .unwrap();
        // The ACK line is the durability contract under test: everything printed here
        // must be intact after abort + reopen.
        println!(
            "ACK {i} {} {} {}",
            receipt.commit, receipt.state_root, receipt.domain_sequence
        );
    }

    if mode == "stage_only" {
        // Stage chunks that no commit will ever reference, then die.
        let orphan = b"orphan bytes never committed".to_vec();
        let digest =
            surrealfs_types::ChunkDigest(surrealfs_types::canonical::chunk_digest(&orphan));
        store
            .stage_chunks(&repo, &[(digest.clone(), orphan)])
            .await
            .unwrap();
        println!("STAGED {digest}");
    }

    // Flush stdout, then die without running any destructor or engine shutdown.
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    std::process::abort();
}
