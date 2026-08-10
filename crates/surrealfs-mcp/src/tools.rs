//! Tool definitions and dispatch.
//!
//! Every mutating tool runs inside `with_span`, which opens a tool-call span, attributes the
//! resulting commit to it, and closes the span with the outcome. A tool cannot mutate the
//! repository without being recorded, because the recording is how the mutation is published.
//!
//! Every advertised tool has a dispatch arm and a test. The AgentFS baseline advertises
//! `kv_list` without implementing it; that gap is not reproduced here.

use std::sync::Arc;

use serde_json::{json, Value};
use surrealfs_kernel::Kernel;
use surrealfs_types::RepoPath;

/// The advertised tool list. Kept in one place so `tools/list` and dispatch cannot drift.
pub fn tool_definitions() -> Vec<Value> {
    let path_arg = |desc: &str| {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": desc } },
            "required": ["path"],
        })
    };
    vec![
        tool(
            "fs_read",
            "Read a file from the workspace",
            path_arg("Repository path to read"),
        ),
        tool(
            "fs_write",
            "Create or overwrite a file. Publishes one commit attributed to this call.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                },
                "required": ["path", "content"],
            }),
        ),
        tool("fs_list", "List a directory", path_arg("Directory to list")),
        tool("fs_stat", "Describe one path", path_arg("Path to describe")),
        tool(
            "fs_mkdir",
            "Create a directory",
            path_arg("Directory to create"),
        ),
        tool(
            "fs_remove",
            "Remove a file or empty directory",
            path_arg("Path to remove"),
        ),
        tool(
            "fs_rename",
            "Move a file, symlink, or directory. Recorded as a rename.",
            json!({
                "type": "object",
                "properties": { "from": { "type": "string" }, "to": { "type": "string" } },
                "required": ["from", "to"],
            }),
        ),
        tool(
            "fs_copy",
            "Copy a file or symlink; content is shared, not duplicated",
            json!({
                "type": "object",
                "properties": { "from": { "type": "string" }, "to": { "type": "string" } },
                "required": ["from", "to"],
            }),
        ),
        tool(
            "fs_symlink",
            "Create a symlink",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" }, "target": { "type": "string" } },
                "required": ["path", "target"],
            }),
        ),
        tool(
            "fs_chmod",
            "Change a path's mode bits",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "mode": { "type": "integer", "description": "Octal mode, e.g. 493 for 0o755" },
                },
                "required": ["path", "mode"],
            }),
        ),
        tool(
            "kv_get",
            "Read a key",
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"],
            }),
        ),
        tool(
            "kv_set",
            "Write a key. Publishes one commit attributed to this call.",
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" }, "value": { "type": "string" } },
                "required": ["key", "value"],
            }),
        ),
        tool(
            "kv_delete",
            "Delete a key",
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"],
            }),
        ),
        tool(
            "kv_list",
            "List keys with a prefix",
            json!({
                "type": "object",
                "properties": { "prefix": { "type": "string" } },
            }),
        ),
        tool(
            "timeline",
            "Recent commits, newest first",
            json!({
                "type": "object",
                "properties": { "limit": { "type": "integer" } },
            }),
        ),
        tool(
            "tool_stats",
            "Per-tool call counts, outcomes, and durations",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "explain",
            "Which tool calls changed this path, newest first",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "integer" },
                },
                "required": ["path"],
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": schema })
}

const KV_NAMESPACE: &str = "default";

pub async fn call_tool(kernel: &Arc<Kernel>, name: &str, args: Value) -> Result<Value, String> {
    match name {
        // ---- reads: recorded as spans too, so the timeline shows what was looked at ----
        "fs_read" => {
            let path = repo_path(&args, "path")?;
            let bytes = kernel.read_head_file(&path).await.map_err(str_err)?;
            Ok(text(String::from_utf8_lossy(&bytes).to_string()))
        }
        "fs_list" => {
            let path = repo_path(&args, "path")?;
            let entries = kernel.list_head(&path).await.map_err(str_err)?;
            let listed: Vec<Value> = entries
                .iter()
                .map(|e| json!({ "name": e.name, "is_dir": e.is_dir, "size": e.size }))
                .collect();
            Ok(text(
                serde_json::to_string_pretty(&listed).unwrap_or_default(),
            ))
        }
        "fs_stat" => {
            let path = repo_path(&args, "path")?;
            match kernel.stat_head(&path).await.map_err(str_err)? {
                None => Ok(text(format!("{path}: not found"))),
                Some(entry) => Ok(text(describe(&path, &entry))),
            }
        }
        "kv_get" => {
            let key = string_arg(&args, "key")?;
            match kernel
                .kv_get_head(KV_NAMESPACE, &key)
                .await
                .map_err(str_err)?
            {
                None => Ok(text(format!("{key}: not found"))),
                Some(bytes) => Ok(text(String::from_utf8_lossy(&bytes).to_string())),
            }
        }
        "kv_list" => {
            let prefix = args
                .get("prefix")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (_, _, kv) = kernel.head_state().await.map_err(str_err)?;
            let keys: Vec<&String> = kv
                .keys()
                .filter(|(ns, key)| ns == KV_NAMESPACE && key.starts_with(&prefix))
                .map(|(_, key)| key)
                .collect();
            Ok(text(
                serde_json::to_string_pretty(&keys).unwrap_or_default(),
            ))
        }
        "timeline" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let commits = kernel.timeline(limit).await.map_err(str_err)?;
            let rows: Vec<Value> = commits
                .iter()
                .map(|c| {
                    json!({
                        "commit": c.commit.to_string(),
                        "at": c.committed_at,
                        "message": c.message,
                        "mutations": c.mutation_count,
                    })
                })
                .collect();
            Ok(text(
                serde_json::to_string_pretty(&rows).unwrap_or_default(),
            ))
        }
        "tool_stats" => {
            let stats = kernel.tool_stats().await.map_err(str_err)?;
            let rows: Vec<Value> = stats
                .iter()
                .map(|s| {
                    json!({
                        "tool": s.tool_name,
                        "calls": s.calls,
                        "succeeded": s.succeeded,
                        "failed": s.failed,
                        "running": s.running,
                        "avg_duration_ms": s.avg_duration_ms,
                    })
                })
                .collect();
            Ok(text(
                serde_json::to_string_pretty(&rows).unwrap_or_default(),
            ))
        }
        "explain" => {
            let path = string_arg(&args, "path")?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
            let history = kernel.explain(&path, limit).await.map_err(str_err)?;
            let rows: Vec<Value> = history
                .iter()
                .map(|p| {
                    json!({
                        "commit": p.commit.to_string(),
                        "operation": p.kind,
                        "at": p.committed_at,
                        "message": p.message,
                        "tool": p.tool_name,
                        "tool_status": p.tool_status,
                    })
                })
                .collect();
            Ok(text(
                serde_json::to_string_pretty(&rows).unwrap_or_default(),
            ))
        }

        // ---- mutations: each is one commit, attributed to its own span ----
        "fs_write" => {
            let path = repo_path(&args, "path")?;
            let content = string_arg(&args, "content")?;
            with_span(kernel, "fs_write", &path.to_string(), |ws| {
                let path = path.clone();
                let content = content.clone();
                Box::pin(async move { ws_write(ws, path, content).await })
            })
            .await
        }
        "fs_mkdir" => {
            let path = repo_path(&args, "path")?;
            with_span(kernel, "fs_mkdir", &path.to_string(), |mut ws| {
                let path = path.clone();
                Box::pin(async move {
                    let r = ws.mkdir(&path).await;
                    (ws, r)
                })
            })
            .await
        }
        "fs_remove" => {
            let path = repo_path(&args, "path")?;
            with_span(kernel, "fs_remove", &path.to_string(), |ws| {
                let path = path.clone();
                Box::pin(async move { ws_remove(ws, path).await })
            })
            .await
        }
        "fs_rename" => {
            let from = repo_path(&args, "from")?;
            let to = repo_path(&args, "to")?;
            let label = format!("{from} -> {to}");
            with_span(kernel, "fs_rename", &label, move |mut ws| {
                Box::pin(async move {
                    let r = ws.rename(&from, &to).await;
                    (ws, r)
                })
            })
            .await
        }
        "fs_copy" => {
            let from = repo_path(&args, "from")?;
            let to = repo_path(&args, "to")?;
            let label = format!("{from} -> {to}");
            with_span(kernel, "fs_copy", &label, move |mut ws| {
                Box::pin(async move {
                    let r = ws.copy(&from, &to).await;
                    (ws, r)
                })
            })
            .await
        }
        "fs_symlink" => {
            let path = repo_path(&args, "path")?;
            let target = string_arg(&args, "target")?;
            with_span(kernel, "fs_symlink", &path.to_string(), move |mut ws| {
                Box::pin(async move {
                    let r = ws.symlink(&path, &target).await;
                    (ws, r)
                })
            })
            .await
        }
        "fs_chmod" => {
            let path = repo_path(&args, "path")?;
            let mode = args
                .get("mode")
                .and_then(Value::as_u64)
                .ok_or_else(|| "missing required argument: mode".to_string())?
                as u32;
            with_span(kernel, "fs_chmod", &path.to_string(), move |mut ws| {
                Box::pin(async move {
                    let r = ws.set_meta(&path, Some(mode), None, None).await;
                    (ws, r)
                })
            })
            .await
        }
        "kv_set" => {
            let key = string_arg(&args, "key")?;
            let value = string_arg(&args, "value")?;
            with_span(kernel, "kv_set", &key.clone(), |mut ws| {
                let key = key.clone();
                let value = value.clone();
                Box::pin(async move {
                    let r = ws.kv_set(KV_NAMESPACE, &key, value.as_bytes());
                    (ws, r)
                })
            })
            .await
        }
        "kv_delete" => {
            let key = string_arg(&args, "key")?;
            with_span(kernel, "kv_delete", &key.clone(), |mut ws| {
                let key = key.clone();
                Box::pin(async move {
                    let r = ws.kv_delete(KV_NAMESPACE, &key);
                    (ws, r)
                })
            })
            .await
        }

        other => Err(format!("unknown tool: {other}")),
    }
}

type WsFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = (
                    surrealfs_kernel::Workspace,
                    Result<(), surrealfs_types::SfsError>,
                ),
            > + Send
            + 'a,
    >,
>;

/// Run a mutation as a recorded tool call.
///
/// The span is opened before any work, the commit is attributed to it, and the span is closed
/// with the outcome — including on failure, so an interrupted call is visible as such rather
/// than silently absent.
async fn with_span<F>(
    kernel: &Arc<Kernel>,
    tool_name: &str,
    target: &str,
    op: F,
) -> Result<Value, String>
where
    F: FnOnce(surrealfs_kernel::Workspace) -> WsFuture<'static>,
{
    let span = kernel
        .tool_start(tool_name, Some(target.to_string()))
        .await
        .map_err(str_err)?;

    let mut ws = kernel.workspace().await.map_err(str_err)?;
    ws.attribute_to(&span);
    let (mut ws, result) = op(ws).await;

    if let Err(err) = result {
        let _ = ws.abort("tool call failed").await;
        let message = err.to_string();
        let _ = kernel.tool_finish(&span, None, Some(message.clone())).await;
        return Err(message);
    }

    let receipt = match ws
        .publish(None, Some(format!("{tool_name} {target}")))
        .await
    {
        Ok(receipt) => receipt,
        Err(err) => {
            let message = err.to_string();
            let _ = kernel.tool_finish(&span, None, Some(message.clone())).await;
            return Err(message);
        }
    };

    let summary = format!("{tool_name} {target} -> commit {}", receipt.commit);
    kernel
        .tool_finish(&span, Some(summary.clone()), None)
        .await
        .map_err(str_err)?;
    Ok(text(summary))
}

async fn ws_write(
    mut ws: surrealfs_kernel::Workspace,
    path: RepoPath,
    content: String,
) -> (
    surrealfs_kernel::Workspace,
    Result<(), surrealfs_types::SfsError>,
) {
    let r = ws.write_file(&path, content.as_bytes()).await;
    (ws, r)
}

/// `fs_remove` covers both files and empty directories, matching what an agent expects from a
/// single "remove" verb.
async fn ws_remove(
    mut ws: surrealfs_kernel::Workspace,
    path: RepoPath,
) -> (
    surrealfs_kernel::Workspace,
    Result<(), surrealfs_types::SfsError>,
) {
    let is_dir = matches!(
        ws.stat(&path).await,
        Ok(Some(surrealfs_content::tree::Entry::Dir { .. }))
    );
    let r = if is_dir {
        ws.rmdir(&path).await
    } else {
        ws.unlink(&path).await
    };
    (ws, r)
}

fn describe(path: &RepoPath, entry: &surrealfs_content::tree::Entry) -> String {
    use surrealfs_content::tree::Entry;
    match entry {
        Entry::Dir { .. } => format!("{path}: directory"),
        Entry::File { size, .. } => format!("{path}: file, {size} bytes"),
        Entry::Symlink { target, .. } => format!("{path}: symlink -> {target}"),
    }
}

fn text(body: String) -> Value {
    json!({ "content": [{ "type": "text", "text": body }], "isError": false })
}

fn str_err(err: surrealfs_types::SfsError) -> String {
    err.to_string()
}

fn string_arg(args: &Value, name: &str) -> Result<String, String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required argument: {name}"))
}

fn repo_path(args: &Value, name: &str) -> Result<RepoPath, String> {
    RepoPath::parse(&string_arg(args, name)?).map_err(|e| e.to_string())
}
