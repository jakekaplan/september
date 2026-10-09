use std::time::Duration;

use axum::{body::Body, http::Request};
use september_memory::SUMMARY_BYTES;
use tower::ServiceExt;

use crate::archive::Kind;

use super::*;

fn message(entry: u64, text: String) -> Message {
    Message {
        source: Source {
            harness: "test".into(),
            session: "test".into(),
            entry: entry.to_string(),
            part: 0,
        },
        project: "test".into(),
        branch: "test".into(),
        timestamp_ms: 0,
        kind: Kind::User,
        call_id: None,
        text,
    }
}

async fn with_job() -> Result<InMemory, Error> {
    let storage = InMemory::default();
    storage.ingest(message(0, "x".repeat(600))).await?;
    Ok(storage)
}

fn node(start: u64, length: u64) -> Node {
    Node::new(start, length).unwrap()
}

async fn summary(storage: &InMemory, node: Node) -> Option<String> {
    let state = storage.state.lock().await;
    state
        .summaries
        .get(&node)
        .map(|completed| completed.text.to_string())
}

#[tokio::test(start_paused = true)]
async fn renewal_moves_expiry_and_never_revives_a_replaced_claim() {
    let storage = with_job().await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let node = Node::try_from(claim.job.range).unwrap();
    tokio::time::advance(Duration::from_secs(40)).await;
    storage.renew(node, claim.token).await.unwrap();
    tokio::time::advance(Duration::from_secs(21)).await;
    assert!(storage.claim().await.unwrap().is_none());
    tokio::time::advance(Duration::from_secs(39)).await;
    assert!(matches!(
        storage.renew(node, claim.token).await,
        Err(Error::ClaimLost)
    ));
    let replacement = storage.claim().await.unwrap().unwrap();
    assert_ne!(replacement.token, claim.token);
    assert!(matches!(
        storage.renew(node, claim.token).await,
        Err(Error::ClaimLost)
    ));
    storage
        .complete(Completion {
            range: replacement.job.range,
            token: replacement.token,
            text: "summary".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        storage.renew(node, replacement.token).await,
        Err(Error::ClaimLost)
    ));
}

#[tokio::test]
async fn short_messages_and_pairs_publish_verbatim_without_jobs() {
    let storage = InMemory::default();
    for (entry, text) in (0..).zip(["first", "second", "third", "fourth"]) {
        storage.ingest(message(entry, text.into())).await.unwrap();
    }
    assert!(storage.claim().await.unwrap().is_none());
    assert_eq!(summary(&storage, node(0, 1)).await.unwrap(), "user: first");
    assert_eq!(
        summary(&storage, node(0, 2)).await.unwrap(),
        "user: first\nuser: second"
    );
    assert_eq!(
        summary(&storage, node(0, 4)).await.unwrap(),
        "user: first\nuser: second\nuser: third\nuser: fourth"
    );
}

#[tokio::test]
async fn the_first_parent_too_long_to_join_becomes_the_only_job() {
    let storage = InMemory::default();
    for entry in 0..2 {
        storage
            .ingest(message(entry, "y".repeat(300)))
            .await
            .unwrap();
    }
    let claim = storage.claim().await.unwrap().unwrap();
    assert_eq!(Node::try_from(claim.job.range).unwrap(), node(0, 2));
    assert!(matches!(claim.job.input, Input::Children { .. }));
    assert!(storage.claim().await.unwrap().is_none());
    assert_eq!(summary(&storage, node(0, 2)).await, None);
}

#[tokio::test]
async fn completions_may_exceed_the_target_but_not_the_ceiling() {
    let storage = with_job().await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let completion = |text: String| Completion {
        range: claim.job.range,
        token: claim.token,
        text,
    };
    assert!(matches!(
        storage
            .complete(completion("z".repeat(MAX_SUMMARY_BYTES + 1)))
            .await,
        Err(Error::Invalid)
    ));
    let oversized = "z".repeat(SUMMARY_BYTES + 100);
    storage
        .complete(completion(oversized.clone()))
        .await
        .unwrap();
    assert_eq!(summary(&storage, node(0, 1)).await, Some(oversized));
}

fn ranges(snapshot: &Snapshot) -> Vec<(u64, u64)> {
    match snapshot {
        Snapshot::Ready { nodes, .. } => nodes.iter().map(|n| (n.start, n.length)).collect(),
        Snapshot::Pending { .. } => panic!("expected a ready snapshot"),
    }
}

/// Eight short messages: every leaf and parent is built verbatim.
async fn eight_notes() -> InMemory {
    let storage = InMemory::default();
    for entry in 0..8 {
        storage
            .ingest(message(entry, format!("note {entry}")))
            .await
            .unwrap();
    }
    storage
}

#[tokio::test]
async fn a_sized_snapshot_merges_its_own_copy_and_leaves_the_live_view_alone() {
    let storage = eight_notes().await;
    let small = storage.prepare(Uuid::new_v4(), Some(40)).await.unwrap();
    assert_eq!(ranges(&small), [(0, 8)]);
    let full = storage.prepare(Uuid::new_v4(), None).await.unwrap();
    assert_eq!(ranges(&full).len(), 8);
}

#[tokio::test]
async fn a_waiting_sized_snapshot_is_frozen_at_its_own_size() {
    let storage = eight_notes().await;
    storage.ingest(message(8, "y".repeat(600))).await.unwrap();
    let id = Uuid::new_v4();
    assert!(matches!(
        storage.prepare(id, Some(60)).await.unwrap(),
        Snapshot::Pending { cutoff: 9, .. }
    ));
    let claim = storage.claim().await.unwrap().unwrap();
    storage
        .complete(Completion {
            range: claim.job.range,
            token: claim.token,
            text: "short".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        ranges(&storage.snapshot(id).await.unwrap()),
        [(0, 8), (8, 1)]
    );
}

#[tokio::test]
async fn http_rejects_a_size_too_small_for_any_view() {
    let app = crate::router(Arc::new(eight_notes().await));
    for (within, status) in [(5, 400), (200, 200)] {
        let response = app
            .clone()
            .oneshot(
                Request::put(format!("/v1/snapshots/{}?within={within}", Uuid::new_v4()))
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
}
