//! MCP protocol conformance, and the property the server exists for: an agent that reaches
//! the repository through this transport cannot produce an unattributed change.

use std::sync::Arc;

use serde_json::{json, Value};
use surrealfs_kernel::Kernel;
use surrealfs_mcp::{handle_line, tool_definitions};
use surrealfs_store::{Store, StoreEngine};
use surrealfs_types::RepositoryId;

async fn kernel() -> Arc<Kernel> {
    let store = Arc::new(Store::open(StoreEngine::Memory).await.unwrap());
    Arc::new(
        Kernel::open(store, RepositoryId::parse("mcp-test").unwrap())
            .await
            .unwrap(),
    )
}

async fn call(k: &Arc<Kernel>, id: u64, name: &str, args: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    handle_line(k, &request.to_string()).await.unwrap()
}

fn body(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn is_error(response: &Value) -> bool {
    response["result"]["isError"].as_bool().unwrap_or(false)
}

#[tokio::test]
async fn initialize_and_list_tools() {
    let k = kernel().await;
    let init = handle_line(
        &k,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "surrealfs");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    let listed = handle_line(
        &k,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string(),
    )
    .await
    .unwrap();
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"fs_write".to_string()));
    assert!(names.contains(&"explain".to_string()));
    // Advertised in the AgentFS baseline but never dispatched there.
    assert!(names.contains(&"kv_list".to_string()));
}

/// Every advertised tool must dispatch. This is the specific baseline defect being avoided.
#[tokio::test]
async fn every_advertised_tool_dispatches() {
    let k = kernel().await;
    for definition in tool_definitions() {
        let name = definition["name"].as_str().unwrap();
        let response = call(&k, 99, name, json!({})).await;
        let message = body(&response);
        assert!(
            !message.starts_with("unknown tool"),
            "{name} is advertised but has no dispatch arm"
        );
    }
}

#[tokio::test]
async fn notifications_get_no_reply() {
    let k = kernel().await;
    let notification = json!({ "jsonrpc": "2.0", "method": "initialized" });
    assert!(handle_line(&k, &notification.to_string()).await.is_none());
}

#[tokio::test]
async fn filesystem_and_kv_round_trip() {
    let k = kernel().await;

    call(
        &k,
        1,
        "fs_write",
        json!({ "path": "/src/main.rs", "content": "fn main() {}" }),
    )
    .await;
    assert_eq!(
        body(&call(&k, 2, "fs_read", json!({ "path": "/src/main.rs" })).await),
        "fn main() {}"
    );

    call(&k, 3, "fs_mkdir", json!({ "path": "/docs" })).await;
    let listing = body(&call(&k, 4, "fs_list", json!({ "path": "/" })).await);
    assert!(listing.contains("docs"));
    assert!(listing.contains("src"));

    assert!(
        body(&call(&k, 5, "fs_stat", json!({ "path": "/src/main.rs" })).await).contains("12 bytes")
    );

    call(&k, 6, "kv_set", json!({ "key": "phase", "value": "m1b" })).await;
    assert_eq!(
        body(&call(&k, 7, "kv_get", json!({ "key": "phase" })).await),
        "m1b"
    );
    assert!(body(&call(&k, 8, "kv_list", json!({ "prefix": "ph" })).await).contains("phase"));

    call(&k, 9, "fs_remove", json!({ "path": "/src/main.rs" })).await;
    assert!(is_error(
        &call(&k, 10, "fs_read", json!({ "path": "/src/main.rs" })).await
    ));
}

/// A failing tool call is a result with `isError`, not a transport fault, and it must still
/// close its span so an interrupted call is visible rather than silently absent.
#[tokio::test]
async fn failures_are_reported_without_faulting_the_connection() {
    let k = kernel().await;
    let response = call(&k, 1, "fs_read", json!({ "path": "/missing.txt" })).await;
    assert!(is_error(&response));
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response.get("error").is_none());

    let response = call(&k, 2, "fs_remove", json!({ "path": "/also-missing" })).await;
    assert!(is_error(&response));

    // The failed mutation left no commit behind: only the genesis commit exists.
    assert_eq!(k.timeline(10).await.unwrap().len(), 1);
    // ...but the attempt is recorded as a FAILED tool call rather than vanishing.
    let recent = k.tool_recent(10).await.unwrap();
    assert!(recent.iter().any(|t| t.tool_name == "fs_remove"));
    assert_eq!(recent[0].status, "FAILED");
}

/// The flagship property: provenance is a consequence of using the transport, not something
/// the agent has to opt into. Ask which tool call produced a file and get an answer.
#[tokio::test]
async fn every_change_names_the_tool_call_that_caused_it() {
    let k = kernel().await;

    call(
        &k,
        1,
        "fs_write",
        json!({ "path": "/config.toml", "content": "debug = false" }),
    )
    .await;
    call(
        &k,
        2,
        "fs_write",
        json!({ "path": "/config.toml", "content": "debug = true" }),
    )
    .await;
    call(&k, 3, "kv_set", json!({ "key": "release", "value": "0.1" })).await;

    let explained = body(&call(&k, 4, "explain", json!({ "path": "/config.toml" })).await);
    let rows: Vec<Value> = serde_json::from_str(&explained).unwrap();

    assert_eq!(rows.len(), 2, "both writes to the path are attributed");
    for row in &rows {
        assert_eq!(row["tool"], "fs_write");
        assert_eq!(row["operation"], "WRITE_FILE");
        assert_eq!(row["tool_status"], "SUCCEEDED");
        assert!(row["commit"].as_str().unwrap().len() == 64);
    }

    // KV changes are attributed through the same column, under a kv: prefix.
    let kv_explained = body(&call(&k, 5, "explain", json!({ "path": "kv:default/release" })).await);
    let kv_rows: Vec<Value> = serde_json::from_str(&kv_explained).unwrap();
    assert_eq!(kv_rows.len(), 1);
    assert_eq!(kv_rows[0]["tool"], "kv_set");

    // And nothing is attributed to a path that was never touched.
    let none = body(&call(&k, 6, "explain", json!({ "path": "/never.txt" })).await);
    assert_eq!(serde_json::from_str::<Vec<Value>>(&none).unwrap().len(), 0);
}
