//! Cross-surface conformance: one workload, three surfaces, one root.
//!
//! The claim this project makes is that there is a single semantic kernel and the surfaces are
//! thin translations over it — not three filesystems that happen to agree today. That claim is
//! only worth anything if it is executable, so this drives the same logical workload through the
//! SDK, through MCP as a client would over JSON-RPC, and through the mount layer as a FUSE or NFS
//! adapter would, then compares the resulting state roots.
//!
//! Roots are pure functions of content, so equality here is a strong statement: every surface
//! produced byte-identical trees, identical modes, identical symlink targets, and identical
//! directory structure. It is deliberately *not* a comparison of commit counts, which differ by
//! design — the SDK publishes once per call, MCP once per tool call, and a mount stages a whole
//! session and publishes once. Same destination, different granularity of history.

use std::sync::Arc;

use serde_json::{json, Value};
use surrealfs_kernel::Kernel;
use surrealfs_mcp::handle_line;
use surrealfs_mount::{MountKernel, ROOT_INODE};
use surrealfs_sdk::{SfsOptions, Surrealfs};
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::{RepositoryId, StateRootId};

async fn kernel(name: &str) -> Arc<Kernel> {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Arc::new(
        Kernel::open(store, RepositoryId::parse(name).unwrap())
            .await
            .unwrap(),
    )
}

async fn mcp_call(k: &Arc<Kernel>, id: u64, name: &str, args: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    let response = handle_line(k, &request.to_string()).await.unwrap();
    assert!(
        response["error"].is_null(),
        "{name} failed: {}",
        response["error"]
    );
    response
}

/// The workload, as prose so the three transcriptions below can be checked against one another:
///
/// 1. `mkdir /src`, write `/src/main.rs`
/// 2. `mkdir /docs`, write `/docs/guide.md`
/// 3. write `/README.md`, then remove it — so the final tree must show no trace of a file that
///    existed partway through
/// 4. rename `/docs` to `/doc` — a subtree move, not a file move
/// 5. symlink `/latest` at `/src/main.rs`
/// 6. chmod `/src/main.rs` to 0o755
const MAIN_RS: &[u8] = b"fn main() { println!(\"hello\"); }";
const GUIDE_MD: &[u8] = b"# Guide\n\nSome prose.\n";
const README: &[u8] = b"this file is removed before the end";

async fn via_sdk() -> StateRootId {
    let sfs = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    let fs = sfs.fs();

    fs.mkdir("/src").await.unwrap();
    fs.write_file("/src/main.rs", MAIN_RS).await.unwrap();
    fs.mkdir("/docs").await.unwrap();
    fs.write_file("/docs/guide.md", GUIDE_MD).await.unwrap();
    fs.write_file("/README.md", README).await.unwrap();
    fs.remove_file("/README.md").await.unwrap();
    fs.rename("/docs", "/doc").await.unwrap();
    fs.symlink("/latest", "/src/main.rs").await.unwrap();
    fs.set_meta("/src/main.rs", Some(0o755), None, None)
        .await
        .unwrap();

    sfs.head().await.unwrap().1
}

async fn via_mcp() -> StateRootId {
    let k = kernel("conformance-mcp").await;

    mcp_call(&k, 1, "fs_mkdir", json!({ "path": "/src" })).await;
    mcp_call(
        &k,
        2,
        "fs_write",
        json!({ "path": "/src/main.rs", "content": String::from_utf8_lossy(MAIN_RS) }),
    )
    .await;
    mcp_call(&k, 3, "fs_mkdir", json!({ "path": "/docs" })).await;
    mcp_call(
        &k,
        4,
        "fs_write",
        json!({ "path": "/docs/guide.md", "content": String::from_utf8_lossy(GUIDE_MD) }),
    )
    .await;
    mcp_call(
        &k,
        5,
        "fs_write",
        json!({ "path": "/README.md", "content": String::from_utf8_lossy(README) }),
    )
    .await;
    mcp_call(&k, 6, "fs_remove", json!({ "path": "/README.md" })).await;
    mcp_call(&k, 7, "fs_rename", json!({ "from": "/docs", "to": "/doc" })).await;
    mcp_call(
        &k,
        8,
        "fs_symlink",
        json!({ "path": "/latest", "target": "/src/main.rs" }),
    )
    .await;
    mcp_call(
        &k,
        9,
        "fs_chmod",
        json!({ "path": "/src/main.rs", "mode": 0o755 }),
    )
    .await;

    k.head().await.unwrap().root
}

async fn via_mount() -> StateRootId {
    let k = kernel("conformance-mount").await;
    let m = MountKernel::new(k.clone()).await.unwrap();

    let src = m.mkdir(ROOT_INODE, "src").await.unwrap();
    let (main_rs, fh) = m.create(src.inode, "main.rs").await.unwrap();
    m.write(fh, 0, MAIN_RS).await.unwrap();
    m.release(fh).await.unwrap();

    let docs = m.mkdir(ROOT_INODE, "docs").await.unwrap();
    let (_, fh) = m.create(docs.inode, "guide.md").await.unwrap();
    m.write(fh, 0, GUIDE_MD).await.unwrap();
    m.release(fh).await.unwrap();

    let (_, fh) = m.create(ROOT_INODE, "README.md").await.unwrap();
    m.write(fh, 0, README).await.unwrap();
    m.release(fh).await.unwrap();
    m.unlink(ROOT_INODE, "README.md").await.unwrap();

    m.rename(ROOT_INODE, "docs", ROOT_INODE, "doc")
        .await
        .unwrap();
    m.symlink(ROOT_INODE, "latest", "/src/main.rs")
        .await
        .unwrap();
    m.setattr(main_rs.inode, Some(0o755), None, None)
        .await
        .unwrap();

    // A mount stages the whole session and publishes once, which is the point of decision 9.
    m.publish(Some("conformance run".into())).await.unwrap();
    k.head().await.unwrap().root
}

#[tokio::test]
async fn every_surface_produces_the_same_state_root() {
    let sdk = via_sdk().await;
    let mcp = via_mcp().await;
    let mount = via_mount().await;

    assert_eq!(
        sdk, mcp,
        "the SDK and MCP disagree — a surface is doing its own semantics"
    );
    assert_eq!(
        sdk, mount,
        "the SDK and the mount layer disagree — a surface is doing its own semantics"
    );
}

/// The counterpart to the test above: the roots match, and the *histories* deliberately do not.
/// If these ever converged it would mean a mount had started publishing on its own.
#[tokio::test]
async fn the_surfaces_agree_on_state_and_differ_on_history() {
    let sfs = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    sfs.fs().mkdir("/a").await.unwrap();
    sfs.fs().write_file("/a/one.txt", b"1").await.unwrap();
    sfs.fs().write_file("/a/two.txt", b"2").await.unwrap();
    let sdk_root = sfs.head().await.unwrap().1;
    let sdk_commits = sfs.timeline(50).await.unwrap().len();

    let k = kernel("conformance-history").await;
    let m = MountKernel::new(k.clone()).await.unwrap();
    let a = m.mkdir(ROOT_INODE, "a").await.unwrap();
    for (name, body) in [("one.txt", &b"1"[..]), ("two.txt", &b"2"[..])] {
        let (_, fh) = m.create(a.inode, name).await.unwrap();
        m.write(fh, 0, body).await.unwrap();
        m.release(fh).await.unwrap();
    }
    m.publish(Some("one turn".into())).await.unwrap();
    let mount_root = k.head().await.unwrap().root;
    let mount_commits = k.timeline(50).await.unwrap().len();

    assert_eq!(sdk_root, mount_root, "same work must reach the same state");
    assert!(
        mount_commits < sdk_commits,
        "a mount stages a session into one commit ({mount_commits}) where the SDK publishes per \
         call ({sdk_commits}); converging would mean the mount started committing on its own"
    );
}

/// Equal *logical* state must give an equal root even when the two repositories were built by
/// different routes — this is what makes the roots above meaningful rather than coincidental.
#[tokio::test]
async fn different_routes_to_the_same_tree_agree() {
    // Route one: write the final content directly.
    let a = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    a.fs().mkdir("/d").await.unwrap();
    a.fs().write_file("/d/f.txt", b"final").await.unwrap();

    // Route two: get there through edits, a detour, and a rename.
    let b = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    b.fs().mkdir("/tmp-name").await.unwrap();
    b.fs()
        .write_file("/tmp-name/f.txt", b"draft")
        .await
        .unwrap();
    b.fs()
        .write_file("/tmp-name/f.txt", b"final")
        .await
        .unwrap();
    b.fs().write_file("/tmp-name/scratch", b"x").await.unwrap();
    b.fs().remove_file("/tmp-name/scratch").await.unwrap();
    b.fs().rename("/tmp-name", "/d").await.unwrap();

    assert_eq!(
        a.head().await.unwrap().1,
        b.head().await.unwrap().1,
        "equal logical state must produce an equal root regardless of how it was reached"
    );
}

/// A conformance test that cannot fail proves nothing, so this pins the sensitivity of the
/// comparison itself: each single-field divergence from the shared workload must change the root.
/// If any of these ever stop differing, the equality assertions above have gone blind.
#[tokio::test]
async fn the_root_comparison_actually_detects_divergence() {
    let baseline = via_sdk().await;

    // A different mode on one file.
    let mode = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    {
        let fs = mode.fs();
        fs.mkdir("/src").await.unwrap();
        fs.write_file("/src/main.rs", MAIN_RS).await.unwrap();
        fs.mkdir("/docs").await.unwrap();
        fs.write_file("/docs/guide.md", GUIDE_MD).await.unwrap();
        fs.rename("/docs", "/doc").await.unwrap();
        fs.symlink("/latest", "/src/main.rs").await.unwrap();
        fs.set_meta("/src/main.rs", Some(0o644), None, None)
            .await
            .unwrap();
    }
    assert_ne!(
        baseline,
        mode.head().await.unwrap().1,
        "a mode difference must change the root"
    );

    // A different symlink target.
    let target = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    {
        let fs = target.fs();
        fs.mkdir("/src").await.unwrap();
        fs.write_file("/src/main.rs", MAIN_RS).await.unwrap();
        fs.mkdir("/docs").await.unwrap();
        fs.write_file("/docs/guide.md", GUIDE_MD).await.unwrap();
        fs.rename("/docs", "/doc").await.unwrap();
        fs.symlink("/latest", "/doc/guide.md").await.unwrap();
        fs.set_meta("/src/main.rs", Some(0o755), None, None)
            .await
            .unwrap();
    }
    assert_ne!(
        baseline,
        target.head().await.unwrap().1,
        "a symlink target difference must change the root"
    );

    // The removed file left behind — the case a naive implementation gets wrong.
    let leftover = Surrealfs::open(SfsOptions::ephemeral()).await.unwrap();
    {
        let fs = leftover.fs();
        fs.mkdir("/src").await.unwrap();
        fs.write_file("/src/main.rs", MAIN_RS).await.unwrap();
        fs.mkdir("/docs").await.unwrap();
        fs.write_file("/docs/guide.md", GUIDE_MD).await.unwrap();
        fs.write_file("/README.md", README).await.unwrap();
        fs.rename("/docs", "/doc").await.unwrap();
        fs.symlink("/latest", "/src/main.rs").await.unwrap();
        fs.set_meta("/src/main.rs", Some(0o755), None, None)
            .await
            .unwrap();
    }
    assert_ne!(
        baseline,
        leftover.head().await.unwrap().1,
        "a file that should have been removed must change the root"
    );
}
