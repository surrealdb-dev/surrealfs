//! Chunk-body encryption, against a real on-disk store.
//!
//! The load-bearing test here is `plaintext_never_reaches_the_database_files`: everything else
//! checks that the plumbing round-trips, but only a scan of the actual bytes on disk shows that
//! content is unreadable. It is paired with a plaintext store asserting the marker *is* found —
//! without that pair, a scanner that quietly matched nothing would let the encrypted case pass
//! for the wrong reason. That is exactly how ContextFS ships a write-buffer cap that exists as a
//! constant and is never checked.

use std::path::Path;
use std::sync::Arc;

use surrealfs_kernel::Kernel;
use surrealfs_store::cipher::ChunkKey;
use surrealfs_store::{Store, StoreConfig, StoreEngine};
use surrealfs_types::{RepoPath, RepositoryId, SfsError};

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_KEY: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn key(hex: &str) -> ChunkKey {
    ChunkKey::from_hex(hex).unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// Open a store on disk, retrying while a previous owner's engine lock drains.
///
/// These tests reopen the same directory in one process to check what a *second* session sees.
/// SurrealKV releases its lock on drop rather than on await, so the next open can briefly race
/// the last one — `SfsCore::open_dir` carries the same bounded retry for the same reason. Only
/// the lock error is retried; an encryption mismatch is returned immediately, which is the whole
/// point of these tests.
async fn kernel_at(dir: &Path, name: &str, hex: Option<&str>) -> Result<Arc<Kernel>, SfsError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let store = loop {
        let config = match hex {
            Some(hex) => StoreConfig::with_key(key(hex)),
            None => StoreConfig::default(),
        };
        match Store::open_with(StoreEngine::SurrealKv(dir.to_path_buf()), config).await {
            Ok(store) => break store,
            Err(SfsError::Storage(msg))
                if msg.contains("locked") && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(other) => return Err(other),
        }
    };
    Ok(Arc::new(
        Kernel::open(Arc::new(store), RepositoryId::parse(name).unwrap()).await?,
    ))
}

async fn memory_kernel(name: &str, hex: Option<&str>) -> Arc<Kernel> {
    let config = match hex {
        Some(hex) => StoreConfig::with_key(key(hex)),
        None => StoreConfig::default(),
    };
    let store = Arc::new(Store::open_with(StoreEngine::Memory, config).await.unwrap());
    Arc::new(
        Kernel::open(store, RepositoryId::parse(name).unwrap())
            .await
            .unwrap(),
    )
}

/// Every byte of every file under `dir`, concatenated. Small stores only.
fn all_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p) {
                out.extend_from_slice(&bytes);
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn a_round_trip_through_an_encrypted_store_returns_the_original_bytes() {
    let k = memory_kernel("enc-roundtrip", Some(KEY)).await;
    let mut ws = k.workspace().await.unwrap();
    ws.write_file(&p("/secret.txt"), b"classified content")
        .await
        .unwrap();
    ws.kv_set("agent", "token", b"kv values are chunks too")
        .unwrap();
    ws.publish(None, Some("write".into())).await.unwrap();

    assert_eq!(
        k.read_head_file(&p("/secret.txt")).await.unwrap(),
        b"classified content"
    );
    assert_eq!(
        k.kv_get_head("agent", "token").await.unwrap().unwrap(),
        b"kv values are chunks too"
    );
}

/// The test that proves the feature. Both halves matter: the encrypted store must not contain
/// the marker, and the plaintext store must — otherwise the scan proves nothing.
#[tokio::test]
async fn plaintext_never_reaches_the_database_files() {
    const MARKER: &[u8] = b"CANARY-e7f2a1-do-not-store-in-the-clear";

    let encrypted_dir = tempfile::tempdir().unwrap();
    {
        let k = kernel_at(encrypted_dir.path(), "enc-leak", Some(KEY))
            .await
            .unwrap();
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/canary.txt"), MARKER).await.unwrap();
        ws.publish(None, Some("canary".into())).await.unwrap();
    }

    let plain_dir = tempfile::tempdir().unwrap();
    {
        let k = kernel_at(plain_dir.path(), "plain-leak", None)
            .await
            .unwrap();
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/canary.txt"), MARKER).await.unwrap();
        ws.publish(None, Some("canary".into())).await.unwrap();
    }

    let plain_bytes = all_bytes(plain_dir.path());
    assert!(
        contains(&plain_bytes, MARKER),
        "the scan cannot find a marker that is definitely there, so it proves nothing about the \
         encrypted case"
    );

    let encrypted_bytes = all_bytes(encrypted_dir.path());
    assert!(
        !contains(&encrypted_bytes, MARKER),
        "content was written to disk in the clear"
    );
}

/// The documented limit, asserted so it stays a known property rather than becoming a surprise.
#[tokio::test]
async fn metadata_is_deliberately_not_encrypted() {
    let dir = tempfile::tempdir().unwrap();
    {
        let k = kernel_at(dir.path(), "enc-metadata", Some(KEY))
            .await
            .unwrap();
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/an-unusual-filename-xyzzy.txt"), b"body")
            .await
            .unwrap();
        ws.publish(None, Some("a distinctive commit message plugh".into()))
            .await
            .unwrap();
    }
    let bytes = all_bytes(dir.path());
    assert!(
        contains(&bytes, b"an-unusual-filename-xyzzy.txt"),
        "paths are expected to be readable; if this changed, the docs must change with it"
    );
    assert!(
        contains(&bytes, b"a distinctive commit message plugh"),
        "commit messages are expected to be readable"
    );
}

#[tokio::test]
async fn a_wrong_key_is_an_encryption_error_not_corruption() {
    let dir = tempfile::tempdir().unwrap();
    {
        let k = kernel_at(dir.path(), "enc-wrong", Some(KEY)).await.unwrap();
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/f.txt"), b"content").await.unwrap();
        ws.publish(None, Some("write".into())).await.unwrap();
    }

    let k = kernel_at(dir.path(), "enc-wrong", Some(OTHER_KEY))
        .await
        .unwrap();
    let err = k.read_head_file(&p("/f.txt")).await.unwrap_err();
    assert!(
        matches!(err, SfsError::Encryption(_)),
        "a wrong key must not be reported as corruption: {err:?}"
    );
}

#[tokio::test]
async fn opening_an_encrypted_repository_without_a_key_fails_at_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let k = kernel_at(dir.path(), "enc-nokey", Some(KEY)).await.unwrap();
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/f.txt"), b"content").await.unwrap();
        ws.publish(None, Some("write".into())).await.unwrap();
    }

    // The failure lands at open, before any read, so the message names the real problem.
    // `Kernel` is not `Debug`, so the result is matched rather than unwrapped.
    let Err(err) = kernel_at(dir.path(), "enc-nokey", None).await else {
        panic!("opening an encrypted repository without a key should have failed");
    };
    let SfsError::Encryption(message) = &err else {
        panic!("expected an encryption error, got {err:?}");
    };
    assert!(
        message.contains("SURREALFS_KEY") || message.contains("--key"),
        "the message should say how to fix it: {message}"
    );
}

/// The more important of the two mismatches: it stops someone believing their data is encrypted
/// when it is not.
#[tokio::test]
async fn a_key_on_a_plaintext_repository_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    {
        let k = kernel_at(dir.path(), "plain-then-key", None).await.unwrap();
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/f.txt"), b"written in the clear")
            .await
            .unwrap();
        ws.publish(None, Some("write".into())).await.unwrap();
    }

    let Err(err) = kernel_at(dir.path(), "plain-then-key", Some(KEY)).await else {
        panic!("opening a plaintext repository with a key should have failed");
    };
    assert!(
        matches!(err, SfsError::Encryption(_)),
        "opening a plaintext repository with a key must be refused: {err:?}"
    );
}

/// Digests are over plaintext, so encryption is invisible to identity. This is what lets an
/// archive move between an encrypted repository and a plaintext one.
#[tokio::test]
async fn an_encrypted_store_produces_the_same_state_root_as_a_plaintext_one() {
    async fn build(hex: Option<&str>, name: &str) -> surrealfs_types::StateRootId {
        let k = memory_kernel(name, hex).await;
        let mut ws = k.workspace().await.unwrap();
        ws.mkdir(&p("/src")).await.unwrap();
        ws.write_file(&p("/src/main.rs"), b"fn main() {}")
            .await
            .unwrap();
        ws.kv_set("agent", "k", b"v").unwrap();
        ws.publish(None, Some("same work".into())).await.unwrap();
        k.head().await.unwrap().root
    }

    assert_eq!(
        build(Some(KEY), "root-enc").await,
        build(None, "root-plain").await,
        "encryption changed the state root, so it is not invisible to identity"
    );
}

/// Two keys, same content: the roots still match, because identity never depends on the key.
#[tokio::test]
async fn two_different_keys_produce_the_same_state_root() {
    async fn build(hex: &str, name: &str) -> surrealfs_types::StateRootId {
        let k = memory_kernel(name, Some(hex)).await;
        let mut ws = k.workspace().await.unwrap();
        ws.write_file(&p("/f.txt"), b"identical content")
            .await
            .unwrap();
        ws.publish(None, Some("write".into())).await.unwrap();
        k.head().await.unwrap().root
    }
    assert_eq!(
        build(KEY, "twokeys-a").await,
        build(OTHER_KEY, "twokeys-b").await
    );
}

/// Re-writing identical content must not re-seal it into a second row: dedup keys on the
/// plaintext digest, never on the stored bytes, which vary per write because the nonce does.
#[tokio::test]
async fn identical_content_still_deduplicates_under_encryption() {
    let k = memory_kernel("enc-dedup", Some(KEY)).await;
    let mut ws = k.workspace().await.unwrap();
    let body = vec![b'x'; 4096];
    ws.write_file(&p("/a.txt"), &body).await.unwrap();
    ws.write_file(&p("/b.txt"), &body).await.unwrap();
    ws.publish(None, Some("two names, one body".into()))
        .await
        .unwrap();

    assert_eq!(k.read_head_file(&p("/a.txt")).await.unwrap(), body);
    assert_eq!(k.read_head_file(&p("/b.txt")).await.unwrap(), body);
}
