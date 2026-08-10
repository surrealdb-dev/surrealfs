//! Reproduces the documented AgentFS README SDK flow in Rust (Phase 2 demo), plus the
//! SurrealFS-native extras: atomic multi-op publication, timeline, and root verification
//! across close/reopen.

use anyhow::Result;
use surrealfs_sdk::{SfsOptions, Surrealfs};

#[tokio::main]
async fn main() -> Result<()> {
    let base = std::env::current_dir()?;

    // Open a persistent repository under ./.surrealfs/research-agent/
    let sfs = Surrealfs::open(SfsOptions::with_id_in(&base, "research-agent")).await?;

    // Key-value operations
    sfs.kv()
        .set("user:preferences", br#"{"theme":"dark"}"#)
        .await?;
    let prefs = sfs.kv().get("user:preferences").await?;
    println!(
        "kv user:preferences = {}",
        String::from_utf8_lossy(&prefs.unwrap())
    );

    // Filesystem operations
    sfs.fs()
        .write_file(
            "/output/report.md",
            b"# Findings\n\nSurrealFS vertical slice works.\n",
        )
        .await?;
    let files = sfs.fs().readdir("/output").await?;
    println!(
        "readdir /output -> {:?}",
        files.iter().map(|e| &e.name).collect::<Vec<_>>()
    );

    // Tool call tracking
    sfs.tools()
        .record("web_search", Some(r#"{"query":"AI"}"#), Some("3 results"))
        .await?;
    for call in sfs.tools().recent(5).await? {
        println!("tool {} [{}]", call.tool_name, call.status);
    }

    // SurrealFS extras: one atomic commit for file + KV, attributed to a tool call.
    let tool = sfs.tools().start("summarize", None).await?;
    let mut ws = sfs.workspace().await?;
    ws.attribute_to(tool.span_key());
    ws.write_file(
        &surrealfs_sdk::RepoPath::parse("/output/summary.md")?,
        b"# Summary\n",
    )
    .await?;
    ws.kv_set("default", "stage", b"summarized")?;
    let receipt = ws.publish(None, Some("summarize results".into())).await?;
    sfs.tools().success(&tool, Some("done")).await?;
    println!(
        "published commit {} (sequence {}) root {}",
        receipt.commit, receipt.domain_sequence, receipt.state_root
    );

    // Timeline
    println!("timeline:");
    for entry in sfs.timeline(10).await? {
        println!(
            "  #{} {} {}",
            entry.domain_sequence,
            entry.commit,
            entry.message.as_deref().unwrap_or("-")
        );
    }

    // Close, reopen, and prove persistence + byte-for-byte root verification.
    let (commit_before, root_before) = sfs.head().await?;
    sfs.close().await?;

    let sfs = Surrealfs::open(SfsOptions::with_id_in(&base, "research-agent")).await?;
    let (commit_after, root_after) = sfs.head().await?;
    assert_eq!(commit_before, commit_after, "head must survive reopen");
    assert_eq!(
        root_before, root_after,
        "state root must verify after reopen"
    );
    println!("reopen verified: head {commit_after} root {root_after}");
    sfs.close().await?;
    Ok(())
}
