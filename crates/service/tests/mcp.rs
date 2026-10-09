//! MCP retrieval through the HTTP router, bound to frozen snapshots.

#![expect(
    clippy::unwrap_used,
    reason = "test failures should identify broken invariants"
)]

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use september::{
    archive::{Kind, Message, Source},
    router,
    snapshots::Snapshot,
    storage::{Archive, InMemory},
};
use september_memory::Budget;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

fn message(entry: u64, text: &str) -> Message {
    Message {
        source: Source {
            harness: "pi".into(),
            session: "demo".into(),
            entry: entry.to_string(),
            part: 0,
        },
        project: "september".into(),
        branch: "main".into(),
        timestamp_ms: 86_400_000,
        kind: Kind::User,
        call_id: None,
        text: text.into(),
    }
}

/// Four short messages whose small view merges into one `0+4` line.
async fn frozen() -> (Router, Uuid) {
    let storage = Arc::new(InMemory::new(Budget::new(30, 60).unwrap()));
    for (entry, text) in (0..).zip(["alpha", "beta", "gamma", "delta"]) {
        storage.ingest(message(entry, text)).await.unwrap();
    }
    let snapshot = Uuid::new_v4();
    let Snapshot::Ready { nodes, .. } = storage.prepare(snapshot).await.unwrap() else {
        panic!("short messages are ready at once");
    };
    assert_eq!((nodes[0].start, nodes[0].length), (0, 4));
    (router(storage), snapshot)
}

async fn rpc(app: &Router, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let response = app
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["result"].clone()
}

async fn call(app: &Router, tool: &str, arguments: Value) -> (bool, String) {
    let result = rpc(
        app,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    )
    .await;
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    (result["isError"] == true, text)
}

#[tokio::test]
async fn zoom_opens_a_frozen_line_down_to_the_original_message() {
    let (app, snapshot) = frozen().await;
    assert_eq!(
        call(
            &app,
            "zoom",
            json!({"snapshot": snapshot, "start": 0, "length": 4})
        )
        .await,
        (
            false,
            "0+2|user: alpha user: beta\n2+2|user: gamma user: delta".into()
        )
    );
    assert_eq!(
        call(
            &app,
            "zoom",
            json!({"snapshot": snapshot, "start": 2, "length": 1})
        )
        .await,
        (
            false,
            "[pi · session demo · september@main]\nuser: gamma".into()
        )
    );
}

#[tokio::test]
async fn date_reports_the_message_time_in_utc() {
    let (app, snapshot) = frozen().await;
    assert_eq!(
        call(&app, "date", json!({"snapshot": snapshot, "id": 3})).await,
        (false, "1970-01-02T00:00:00Z".into())
    );
}

#[tokio::test]
async fn retrieval_outside_the_snapshot_or_without_one_is_a_tool_error() {
    let (app, snapshot) = frozen().await;
    for (arguments, reason) in [
        (
            json!({"snapshot": snapshot, "start": 0, "length": 8}),
            "that line is not in your memory view",
        ),
        (
            json!({"snapshot": snapshot, "start": 1, "length": 2}),
            "not a line: n must be a power of 2 and id a multiple of n",
        ),
        (
            json!({"start": 0, "length": 1}),
            "no snapshot for this interaction; the harness adapter supplies it",
        ),
    ] {
        assert_eq!(call(&app, "zoom", arguments).await, (true, reason.into()));
    }
}

#[tokio::test]
async fn tools_list_both_tools_with_the_snapshot_optional() {
    let (app, _) = frozen().await;
    let tools = rpc(&app, "tools/list", json!({})).await["tools"].clone();
    let names: Vec<_> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["date", "zoom"]);
    for tool in tools.as_array().unwrap() {
        let required = tool["inputSchema"]["required"].as_array().unwrap();
        assert!(!required.contains(&json!("snapshot")));
    }
}
