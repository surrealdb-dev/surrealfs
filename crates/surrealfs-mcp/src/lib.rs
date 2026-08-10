//! MCP server over the SurrealFS kernel.
//!
//! Every tool call opens a span, publishes its work attributed to that span, and closes it.
//! Provenance is therefore a property of the transport rather than something the agent has to
//! remember to record: if a change reached the repository through this server, the commit that
//! carries it names the tool call that caused it.
//!
//! That is the difference from the AgentFS baseline, whose shipped MCP server, FUSE, and NFS
//! paths never write `tool_calls` at all — its audit log is empty unless the embedding
//! application populates it by hand.
//!
//! Transport is newline-delimited JSON-RPC 2.0 on stdio.

mod tools;

use std::sync::Arc;

use serde_json::{json, Value};
use surrealfs_kernel::Kernel;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub use tools::{call_tool, tool_definitions};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve MCP on stdin/stdout until the client closes the stream.
pub async fn serve_stdio(kernel: Arc<Kernel>) -> std::io::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle_line(&kernel, &line).await else {
            continue; // notification: no reply
        };
        stdout.write_all(format!("{response}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

/// Handle one JSON-RPC message. `None` means the message was a notification.
pub async fn handle_line(kernel: &Arc<Kernel>, line: &str) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            ))
        }
    };
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(json!({}));

    // No id means a notification; the spec forbids replying.
    let id = id?;

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "surrealfs", "version": env!("CARGO_PKG_VERSION") },
        })),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(kernel, &name, args).await
        }
        "ping" => Ok(json!({})),
        other => Err(format!("unknown method: {other}")),
    };

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        // A failed tool call is a result with isError, not a protocol error: the agent needs
        // to see the message and decide what to do, not have the connection fault.
        Err(message) if method == "tools/call" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": message }],
                "isError": true,
            },
        }),
        Err(message) => error_response(id, -32601, &message),
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
