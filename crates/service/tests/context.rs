//! Historical worker context through storage, HTTP, and the continuous worker.

#![expect(
    clippy::unwrap_used,
    reason = "test failures should identify broken invariants"
)]

use std::{sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use september::{
    archive::{Kind, Message, Source},
    jobs::{Claim, Completion},
    router,
    snapshots::Snapshot,
    storage::{InMemory, Storage},
    worker::{Worker, fake},
};
use september_memory::{Budget, Node};
use tokio::sync::{mpsc, oneshot};
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

async fn publish(storage: &InMemory, claim: &Claim, text: &str) {
    storage
        .complete(Completion {
            range: claim.range,
            token: claim.token,
            text: text.into(),
        })
        .await
        .unwrap();
}

async fn release(storage: &InMemory, claim: &Claim) {
    storage
        .release(
            Node::try_from(claim.range).unwrap(),
            claim.token,
            Duration::ZERO,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn http_claim_has_prior_context_without_its_input_or_future() {
    let storage = Arc::new(InMemory::default());
    storage
        .ingest(message(0, "Use the memory backend first."))
        .await
        .unwrap();
    storage
        .ingest(message(1, &"current input".repeat(100)))
        .await
        .unwrap();
    storage.ingest(message(2, "future decision")).await.unwrap();
    let response = router(storage)
        .oneshot(
            Request::post("/v1/jobs/claim")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let claim: Claim =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(claim.context.cutoff, 1);
    assert!(claim.context.view.contains("Use the memory backend first."));
    assert!(claim.context.view.contains("september"));
    assert!(!claim.context.view.contains("current input"));
    assert!(!claim.context.view.contains("future decision"));
}

#[tokio::test(start_paused = true)]
async fn first_unbuilt_leaf_bounds_context_and_retries_keep_it_after_the_gap_fills() {
    let storage = InMemory::default();
    storage
        .ingest(message(0, &"old gap".repeat(100)))
        .await
        .unwrap();
    storage.ingest(message(1, "ready after gap")).await.unwrap();
    storage
        .ingest(message(2, &"later input".repeat(100)))
        .await
        .unwrap();
    let first = storage.claim().await.unwrap().unwrap();
    let later = storage.claim().await.unwrap().unwrap();
    assert_eq!(later.range.start, 2);
    assert_eq!(later.context.cutoff, 0);
    assert_eq!(later.context.view, "<chat>\n</chat>");
    publish(&storage, &first, "gap filled").await;
    let parent = storage.claim().await.unwrap().unwrap();
    assert_eq!(parent.range.length, 2);
    assert_eq!(parent.context.cutoff, 2);
    assert!(parent.context.view.contains("gap filled"));
    assert!(parent.context.view.contains("ready after gap"));
    publish(&storage, &parent, "past pair").await;
    release(&storage, &later).await;
    let retried = storage.claim().await.unwrap().unwrap();
    assert_ne!(retried.token, later.token);
    assert_eq!(retried.context, later.context);
    tokio::time::advance(Duration::from_secs(61)).await;
    let expired = storage.claim().await.unwrap().unwrap();
    assert_ne!(expired.token, retried.token);
    assert_eq!(expired.context, later.context);
}

#[tokio::test]
async fn main_merge_rederives_context_but_preserves_already_claimed_context() {
    let storage = InMemory::new(Budget::new(80, 300).unwrap());
    storage.ingest(message(0, "first decision")).await.unwrap();
    storage.ingest(message(1, "second decision")).await.unwrap();
    storage
        .ingest(message(2, &"input".repeat(150)))
        .await
        .unwrap();
    let parent = storage.claim().await.unwrap().unwrap();
    let leaf = storage.claim().await.unwrap().unwrap();
    assert!(leaf.context.view.contains("first decision"));
    publish(&storage, &parent, "merged decisions").await;
    release(&storage, &leaf).await;
    let retried = storage.claim().await.unwrap().unwrap();
    assert_eq!(retried.context, leaf.context);
    publish(&storage, &retried, "third decision").await;
    storage
        .ingest(message(3, &"next".repeat(200)))
        .await
        .unwrap();
    let next = storage.claim().await.unwrap().unwrap();
    assert_eq!(next.context.cutoff, 3);
    assert!(next.context.view.contains("0+2|merged decisions"));
    assert!(!next.context.view.contains("first decision"));
}

#[tokio::test]
async fn smaller_view_batches_independently_and_counts_utf8_bytes() {
    let storage = InMemory::default();
    let text = "🙂".repeat(65);
    for entry in 0..96 {
        storage.ingest(message(entry, &text)).await.unwrap();
        while let Some(claim) = storage.claim().await.unwrap() {
            assert!(claim.context.view.len() <= 32_000);
            publish(&storage, &claim, &"é".repeat(200)).await;
        }
    }
    let Snapshot::Ready { view, .. } = storage.prepare(Uuid::new_v4()).await.unwrap() else {
        panic!("ready")
    };
    assert!(view.len() > 32_000);
    storage
        .ingest(message(96, &"probe".repeat(150)))
        .await
        .unwrap();
    let probe = storage.claim().await.unwrap().unwrap();
    assert_eq!(probe.context.cutoff, 96);
    assert!(probe.context.view.len() <= 32_000);
    assert!(probe.context.view.contains("é"));
    assert!(probe.context.view.contains("95+1|"));
}

#[tokio::test]
async fn oversized_context_defers_jobs_while_ready_parents_make_progress() {
    let storage = InMemory::default();
    for entry in 0..400 {
        storage
            .ingest(message(entry, &"🙂".repeat(65)))
            .await
            .unwrap();
    }
    let mut completed = 0;
    while let Some(claim) = storage.claim().await.unwrap() {
        assert!(claim.range.length > 1);
        assert!(claim.context.view.len() <= 32_000);
        assert_eq!(claim.context.cutoff, claim.range.start + claim.range.length);
        publish(&storage, &claim, &"é".repeat(200)).await;
        completed += 1;
    }
    assert_eq!(completed, 397);
}

#[tokio::test(start_paused = true)]
async fn eight_unbuilt_predecessors_block_a_leaf_even_while_claims_back_off() {
    let storage = InMemory::default();
    for entry in 0..9 {
        storage
            .ingest(message(entry, &"long input".repeat(100)))
            .await
            .unwrap();
    }
    let first = storage.claim().await.unwrap().unwrap();
    for _ in 1..8 {
        let claim = storage.claim().await.unwrap().unwrap();
        storage
            .release(
                Node::try_from(claim.range).unwrap(),
                claim.token,
                Duration::from_secs(30),
            )
            .await
            .unwrap();
    }
    assert!(storage.claim().await.unwrap().is_none());
    publish(&storage, &first, "first complete").await;
    let ninth = storage.claim().await.unwrap().unwrap();
    assert_eq!(ninth.range.start, 8);
    assert_eq!(ninth.context.cutoff, 1);
}

#[tokio::test]
async fn continuous_worker_receives_the_claims_historical_context() {
    let storage = Arc::new(InMemory::default());
    storage
        .ingest(message(0, "keep the earlier decision"))
        .await
        .unwrap();
    storage
        .ingest(message(1, &"input".repeat(150)))
        .await
        .unwrap();
    let (contexts, mut received) = mpsc::channel(8);
    let worker = Worker::memory(Arc::clone(&storage), move |input, context| {
        let contexts = contexts.clone();
        async move {
            contexts.send(context.clone()).await.unwrap();
            fake(input, context).await
        }
    })
    .await
    .unwrap();
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    let context = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.cutoff, 1);
    assert!(context.view.contains("keep the earlier decision"));
    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
}
