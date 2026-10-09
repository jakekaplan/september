//! Storage contracts and HTTP acceptance tests using synthetic summaries.

#![expect(
    clippy::unwrap_used,
    reason = "test failures should identify broken invariants"
)]

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use september::{
    Error,
    archive::{Kind, Message, Source},
    jobs::{Claim, Completion, Input},
    router,
    snapshots::{Detail, Snapshot},
    storage::{Archive, InMemory, Jobs},
};
use september_memory::{Budget, Node};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

fn message(entry: usize, text: &str) -> Message {
    Message {
        source: Source {
            harness: "pi".into(),
            session: "a".into(),
            entry: entry.to_string(),
            part: 0,
        },
        project: "september".into(),
        branch: "main".into(),
        timestamp_ms: 1000,
        kind: Kind::User,
        call_id: None,
        text: text.into(),
    }
}

fn node(start: u64, length: u64) -> Node {
    Node::new(start, length).unwrap()
}

fn view(snapshot: Snapshot) -> (u64, String) {
    match snapshot {
        Snapshot::Ready { cutoff, view, .. } => (cutoff, view),
        Snapshot::Pending { .. } => panic!("expected ready snapshot"),
    }
}

async fn publish(storage: &impl Jobs, claim: &Claim, text: &str) -> Result<(), Error> {
    storage
        .complete(Completion {
            range: claim.job.range,
            token: claim.token,
            text: text.into(),
        })
        .await
}

#[tokio::test]
async fn source_retries_are_idempotent_and_snapshots_retrieve_originals() {
    let storage = InMemory::default();
    let original = message(0, "Use Postgres later.");
    let first = storage.ingest(original.clone()).await.unwrap();
    let retry = storage.ingest(original.clone()).await.unwrap();
    assert_eq!(first.id, retry.id);
    assert!(retry.duplicate);
    let mut conflict = original.clone();
    conflict.branch = "experiment".into();
    assert!(matches!(
        storage.ingest(conflict).await,
        Err(Error::Conflict)
    ));
    let snapshot = Uuid::new_v4();
    assert_eq!(view(storage.prepare(snapshot).await.unwrap()).0, 1);
    let Detail::Message {
        id,
        message: retrieved,
    } = storage.zoom(snapshot, node(0, 1)).await.unwrap()
    else {
        panic!("expected original");
    };
    assert_eq!(id, first.id);
    assert_eq!(retrieved, original);
    storage
        .ingest(message(1, "Other session's update"))
        .await
        .unwrap();
    assert_eq!(view(storage.prepare(snapshot).await.unwrap()).0, 1);
    assert!(matches!(
        storage.zoom(snapshot, node(1, 1)).await,
        Err(Error::OutsideSnapshot)
    ));
}

#[tokio::test]
async fn concurrent_writers_assign_contiguous_ids_and_deduplicate() {
    let storage = Arc::new(InMemory::default());
    let mut tasks = Vec::new();
    for entry in 0..32 {
        for _ in 0..2 {
            let storage = storage.clone();
            tasks.push(tokio::spawn(async move {
                storage.ingest(message(entry, "hello")).await.unwrap()
            }));
        }
    }
    let mut ids = BTreeSet::new();
    let mut duplicates = 0;
    for task in tasks {
        let receipt = task.await.unwrap();
        ids.insert(receipt.id);
        duplicates += usize::from(receipt.duplicate);
    }
    assert_eq!(ids, (0..32).collect());
    assert_eq!(duplicates, 32);
    assert_eq!(view(storage.prepare(Uuid::new_v4()).await.unwrap()).0, 32);
}

#[tokio::test]
async fn pending_snapshots_keep_their_cutoff_across_out_of_order_completion() {
    let storage = InMemory::default();
    storage.ingest(message(0, &"a".repeat(600))).await.unwrap();
    let first = Uuid::new_v4();
    assert!(matches!(
        storage.prepare(first).await.unwrap(),
        Snapshot::Pending { cutoff: 1, .. }
    ));
    assert!(matches!(
        storage.zoom(first, node(0, 1)).await,
        Err(Error::NotReady)
    ));
    storage.ingest(message(1, &"b".repeat(600))).await.unwrap();
    let second = Uuid::new_v4();
    storage.prepare(second).await.unwrap();
    let a = storage.claim().await.unwrap().unwrap();
    let b = storage.claim().await.unwrap().unwrap();
    assert!(matches!(&a.job.input, Input::Message { message } if message.text.len() == 600));
    // Too long to join verbatim, so their parent becomes a job.
    let padding = "-".repeat(300);
    publish(&storage, &b, &format!("second summary {padding}"))
        .await
        .unwrap();
    assert!(matches!(
        storage.snapshot(first).await.unwrap(),
        Snapshot::Pending { cutoff: 1, .. }
    ));
    assert!(storage.claim().await.unwrap().is_none()); // parent needs both children
    publish(&storage, &a, &format!("first summary {padding}"))
        .await
        .unwrap();
    let (cutoff, first_view) = view(storage.snapshot(first).await.unwrap());
    assert_eq!(cutoff, 1);
    assert!(first_view.contains("first summary"));
    assert!(!first_view.contains("second summary"));
    assert_eq!(view(storage.snapshot(second).await.unwrap()).0, 2);
    let parent = storage.claim().await.unwrap().unwrap();
    assert_eq!(Node::try_from(parent.job.range).unwrap(), node(0, 2));
    assert!(matches!(parent.job.input, Input::Children { .. }));
}

#[tokio::test]
async fn batch_merges_change_future_views_but_not_frozen_navigation() {
    let storage = InMemory::new(Budget::new(80, 300).unwrap());
    // Each fits verbatim, but not both together, so their parent is a job.
    let padding = "-".repeat(300);
    storage
        .ingest(message(0, &format!("Use Postgres {padding}")))
        .await
        .unwrap();
    storage
        .ingest(message(1, &format!("Do not add Redis {padding}")))
        .await
        .unwrap();
    let old = Uuid::new_v4();
    let old_view = view(storage.prepare(old).await.unwrap());
    assert!(matches!(
        storage.zoom(old, node(0, 2)).await,
        Err(Error::OutsideSnapshot)
    ));
    let parent = storage.claim().await.unwrap().unwrap();
    publish(
        &storage,
        &parent,
        "pi/a, september/main: Postgres; no Redis.",
    )
    .await
    .unwrap();
    let new = Uuid::new_v4();
    let (_, new_view) = view(storage.prepare(new).await.unwrap());
    assert!(new_view.contains("0+2|"));
    assert!(new_view.len() <= 80);
    assert_eq!(view(storage.snapshot(old).await.unwrap()), old_view);
    assert!(matches!(
        storage.zoom(old, node(0, 2)).await,
        Err(Error::OutsideSnapshot)
    ));
    let Detail::Children { summaries } = storage.zoom(new, node(0, 2)).await.unwrap() else {
        panic!("expected children");
    };
    assert!(summaries[0].text.contains("Use Postgres"));
    assert!(summaries[1].text.contains("Do not add Redis"));
    assert!(matches!(
        storage.zoom(new, node(1, 1)).await.unwrap(),
        Detail::Message { id: 1, .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn expired_claims_are_recovered_and_stale_workers_cannot_publish() {
    let storage = InMemory::default();
    storage.ingest(message(0, &"x".repeat(600))).await.unwrap();
    let old = storage.claim().await.unwrap().unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(matches!(
        publish(&storage, &old, "stale").await,
        Err(Error::ClaimLost)
    ));
    let new = storage.claim().await.unwrap().unwrap();
    assert_ne!(old.token, new.token);
    assert!(matches!(
        publish(&storage, &old, "stale").await,
        Err(Error::ClaimLost)
    ));
    publish(&storage, &new, "complete").await.unwrap();
    publish(&storage, &new, "complete").await.unwrap(); // response-loss retry
    assert!(matches!(
        publish(&storage, &new, "rewrite").await,
        Err(Error::ClaimLost)
    ));
    assert!(storage.claim().await.unwrap().is_none());
}

#[tokio::test]
async fn publication_schedules_each_parent_once_and_merges_through_multiple_levels() {
    let storage = InMemory::new(Budget::new(30, 80).unwrap());
    // Every message and summary fits alone, but no pair joins verbatim.
    let long = "s".repeat(300);
    for entry in 0..4 {
        storage.ingest(message(entry, &long)).await.unwrap();
    }
    let left = storage.claim().await.unwrap().unwrap();
    let right = storage.claim().await.unwrap().unwrap();
    assert!(storage.claim().await.unwrap().is_none());
    publish(&storage, &left, &long).await.unwrap();
    publish(&storage, &left, &long).await.unwrap();
    assert!(storage.claim().await.unwrap().is_none());
    publish(&storage, &right, &long).await.unwrap();
    let root = storage.claim().await.unwrap().unwrap();
    assert_eq!(Node::try_from(root.job.range).unwrap(), node(0, 4));
    publish(&storage, &root, "summary").await.unwrap();
    assert!(storage.claim().await.unwrap().is_none());
    let id = Uuid::new_v4();
    assert_eq!(
        view(storage.prepare(id).await.unwrap()),
        (4, "<chat>\n0+4|summary\n</chat>".into())
    );
    for start in 0..4 {
        assert!(matches!(
            storage.zoom(id, node(start, 1)).await.unwrap(),
            Detail::Message { .. }
        ));
    }
}

#[tokio::test]
async fn claims_are_bounded_and_oversized_completion_preserves_the_claim() {
    let storage = InMemory::default();
    for entry in 0..9 {
        storage
            .ingest(message(entry, &"x".repeat(600)))
            .await
            .unwrap();
    }
    let mut claims = Vec::new();
    for _ in 0..8 {
        claims.push(storage.claim().await.unwrap().unwrap());
    }
    assert!(storage.claim().await.unwrap().is_none());
    assert!(matches!(
        publish(&storage, &claims[0], &"🙂".repeat(257)).await,
        Err(Error::Invalid)
    ));
    publish(&storage, &claims[0], &"🙂".repeat(256))
        .await
        .unwrap();
    assert!(storage.claim().await.unwrap().is_some());
}

#[tokio::test]
async fn invalid_messages_do_not_consume_ids_or_create_work() {
    let storage = InMemory::default();
    for text in [String::new(), "x".repeat(65_537)] {
        assert!(matches!(
            storage.ingest(message(0, &text)).await,
            Err(Error::Invalid)
        ));
    }
    let mut tool = message(0, "tool call");
    tool.kind = Kind::ToolCall;
    assert!(matches!(
        storage.ingest(tool.clone()).await,
        Err(Error::Invalid)
    ));
    assert!(storage.claim().await.unwrap().is_none());
    tool.call_id = Some("call-1".into());
    assert_eq!(storage.ingest(tool).await.unwrap().id, 0);
}

#[tokio::test]
async fn fresh_backends_are_empty_and_capacity_rejections_leave_retries_working() {
    let storage = InMemory::default();
    let id = Uuid::new_v4();
    storage.prepare(id).await.unwrap();
    assert!(matches!(
        InMemory::default().snapshot(id).await,
        Err(Error::NotFound)
    ));
    for _ in 1..128 {
        storage.prepare(Uuid::new_v4()).await.unwrap();
    }
    assert!(matches!(
        storage.prepare(Uuid::new_v4()).await,
        Err(Error::Capacity)
    ));
    assert_eq!(view(storage.prepare(id).await.unwrap()).0, 0);
    for entry in 0..1024 {
        storage
            .ingest(message(entry, &"x".repeat(600)))
            .await
            .unwrap();
    }
    assert!(matches!(
        storage.ingest(message(1024, "full")).await,
        Err(Error::Capacity)
    ));
    assert!(
        storage
            .ingest(message(0, &"x".repeat(600)))
            .await
            .unwrap()
            .duplicate
    );
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let payload = body.map_or_else(String::new, |v| v.to_string());
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["x-september-durability"], "volatile");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn http_ingestion_freezing_zoom_and_error_boundaries() {
    let app = router(Arc::new(InMemory::default()));
    let original = serde_json::to_value(message(0, "Remember this")).unwrap();
    assert_eq!(
        request(&app, "POST", "/v1/messages", Some(original.clone()))
            .await
            .0,
        StatusCode::CREATED
    );
    assert_eq!(
        request(&app, "POST", "/v1/messages", Some(original))
            .await
            .0,
        StatusCode::OK
    );
    let path = format!("/v1/snapshots/{}", Uuid::new_v4());
    let (status, snapshot) = request(&app, "PUT", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["cutoff"], 1);
    let (_, detail) = request(&app, "GET", &format!("{path}/zoom?start=0&length=1"), None).await;
    assert_eq!(detail["message"]["text"], "Remember this");
    for (range, status) in [
        ("start=1&length=1", StatusCode::FORBIDDEN),
        ("start=0&length=3", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            request(&app, "GET", &format!("{path}/zoom?{range}"), None)
                .await
                .0,
            status
        );
    }
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/v1/snapshots/{}", Uuid::new_v4()),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let mut reasoning = serde_json::to_value(message(1, "secret reasoning")).unwrap();
    reasoning["kind"] = json!("reasoning");
    assert_eq!(
        request(&app, "POST", "/v1/messages", Some(reasoning))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let oversized = serde_json::to_value(message(1, &"x".repeat(512 * 1024))).unwrap();
    assert_eq!(
        request(&app, "POST", "/v1/messages", Some(oversized))
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn http_pending_snapshot_becomes_ready_after_explicit_summary_completion() {
    let app = router(Arc::new(InMemory::default()));
    let original = serde_json::to_value(message(0, &"a".repeat(600))).unwrap();
    request(&app, "POST", "/v1/messages", Some(original)).await;
    let path = format!("/v1/snapshots/{}", Uuid::new_v4());
    assert_eq!(
        request(&app, "PUT", &path, None).await.0,
        StatusCode::ACCEPTED
    );
    let (_, claim) = request(&app, "POST", "/v1/jobs/claim", None).await;
    let completion =
        json!({"range": claim["range"], "token": claim["token"], "text": "A summary."});
    assert_eq!(
        request(&app, "POST", "/v1/jobs/complete", Some(completion))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (status, snapshot) = request(&app, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["view"], "<chat>\n0+1|A summary.\n</chat>");
    assert_eq!(
        request(&app, "POST", "/v1/jobs/claim", None).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn http_rejects_browser_origins_and_nonlocal_hosts() {
    let app = router(Arc::new(InMemory::default()));
    for (host, origin) in [
        ("attacker.example", None),
        ("localhost", Some("https://attacker.example")),
    ] {
        let mut request = Request::builder().uri("/health").header("host", host);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
