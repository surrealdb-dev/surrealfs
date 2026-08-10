//! Test support: reopen helpers shared by crash and lifecycle tests.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::SfsError;

/// Reopen a SurrealKV store, retrying while a previous owner's engine lock clears.
/// (Awaited shutdown is pinned upstream work; see COMPATIBILITY.md.)
pub async fn reopen_store(db_path: PathBuf) -> Result<Store, SfsError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match Store::open(StoreEngine::SurrealKv(db_path.clone())).await {
            Ok(store) => return Ok(store),
            Err(SfsError::Storage(msg)) if msg.contains("locked") && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(other) => return Err(other),
        }
    }
}
