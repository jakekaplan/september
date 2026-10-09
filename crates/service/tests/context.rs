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
use docket::Docket;
use september::{
    Error,
    archive::{Kind, Message, Source},
    jobs::{Claim, Completion, Job},
    router,
    snapshots::Snapshot,
    storage::{Archive, InMemory, Jobs},
    worker::Worker,
};
use september_memory::Budget;
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

/// A deterministic stand-in for a model. Its summaries are short enough that
/// every pair of them joins verbatim.
/// A private in-process Docket queue for one test.
async fn queue() -> Docket {
    Docket::connect("september", format!("memory://{}", Uuid::new_v4()))
        .await
        .unwrap()
}

async fn summarize(job: Job) -> Result<String, Error> {
    tokio::task::yield_now().await;
    Ok(format!(
        "summary of {}+{}",
        job.range.start, job.range.length
    ))
}

async fn publish(storage: &InMemory, claim: &Claim, text: &str) {
    storage
        .complete(Completion {
            range: claim.job.range,
            token: claim.token,
            text: text.into(),
        })
        .await
        .unwrap();
}

/// Let every live claim lapse, so its job becomes claimable again.
async fn expire_claims() {
    tokio::time::advance(Duration::from_secs(61)).await;
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
    assert_eq!(claim.job.context.cutoff, 1);
    assert!(
        claim
            .job
            .context
            .view
            .contains("Use the memory backend first.")
    );
    assert!(
        claim
            .job
            .context
            .view
            .contains("0+1|user: Use the memory backend first.")
    );
    assert!(!claim.job.context.view.contains("current input"));
    assert!(!claim.job.context.view.contains("future decision"));
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
    assert_eq!(later.job.range.start, 2);
    assert_eq!(later.job.context.cutoff, 0);
    assert_eq!(later.job.context.view, "<chat>\n</chat>");
    // Long enough that the pair cannot be joined verbatim.
    publish(&storage, &first, &format!("gap filled {}", "g".repeat(500))).await;
    let parent = storage.claim().await.unwrap().unwrap();
    assert_eq!(parent.job.range.length, 2);
    assert_eq!(parent.job.context.cutoff, 2);
    assert!(parent.job.context.view.contains("gap filled"));
    assert!(parent.job.context.view.contains("ready after gap"));
    publish(&storage, &parent, "past pair").await;
    expire_claims().await;
    let retried = storage.claim().await.unwrap().unwrap();
    assert_ne!(retried.token, later.token);
    assert_eq!(retried.job.context, later.job.context);
    expire_claims().await;
    let expired = storage.claim().await.unwrap().unwrap();
    assert_ne!(expired.token, retried.token);
    assert_eq!(expired.job.context, later.job.context);
}

#[tokio::test(start_paused = true)]
async fn main_merge_rederives_context_but_preserves_already_claimed_context() {
    let storage = InMemory::new(Budget::new(80, 300).unwrap());
    // Each fits verbatim, but not both together, so their parent is a job.
    let padding = "-".repeat(300);
    storage
        .ingest(message(0, &format!("first decision {padding}")))
        .await
        .unwrap();
    storage
        .ingest(message(1, &format!("second decision {padding}")))
        .await
        .unwrap();
    storage
        .ingest(message(2, &"input".repeat(150)))
        .await
        .unwrap();
    let parent = storage.claim().await.unwrap().unwrap();
    let leaf = storage.claim().await.unwrap().unwrap();
    assert!(leaf.job.context.view.contains("first decision"));
    publish(&storage, &parent, "merged decisions").await;
    expire_claims().await;
    let retried = storage.claim().await.unwrap().unwrap();
    assert_eq!(retried.job.context, leaf.job.context);
    publish(&storage, &retried, "third decision").await;
    storage
        .ingest(message(3, &"next".repeat(200)))
        .await
        .unwrap();
    let next = storage.claim().await.unwrap().unwrap();
    assert_eq!(next.job.context.cutoff, 3);
    assert!(next.job.context.view.contains("0+2|merged decisions"));
    assert!(!next.job.context.view.contains("first decision"));
}

#[tokio::test]
async fn smaller_view_batches_independently_and_counts_utf8_bytes() {
    let storage = InMemory::default();
    let text = "🙂".repeat(65);
    for entry in 0..160 {
        storage.ingest(message(entry, &text)).await.unwrap();
        while let Some(claim) = storage.claim().await.unwrap() {
            assert!(claim.job.context.view.len() <= 32_000);
            publish(&storage, &claim, &"é".repeat(200)).await;
        }
    }
    let Snapshot::Ready { view, .. } = storage.prepare(Uuid::new_v4(), None).await.unwrap() else {
        panic!("ready")
    };
    assert!(view.len() > 32_000);
    storage
        .ingest(message(160, &"probe".repeat(150)))
        .await
        .unwrap();
    let probe = storage.claim().await.unwrap().unwrap();
    assert_eq!(probe.job.context.cutoff, 160);
    assert!(probe.job.context.view.len() <= 32_000);
    assert!(probe.job.context.view.contains("é"));
    assert!(probe.job.context.view.contains("159+1|"));
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
        assert!(claim.job.range.length > 1);
        assert!(claim.job.context.view.len() <= 32_000);
        assert_eq!(
            claim.job.context.cutoff,
            claim.job.range.start + claim.job.range.length
        );
        publish(&storage, &claim, &"é".repeat(200)).await;
        completed += 1;
    }
    assert_eq!(completed, 397);
}

#[tokio::test(start_paused = true)]
async fn eight_unbuilt_predecessors_block_a_leaf_even_after_their_claims_expire() {
    let storage = InMemory::default();
    for entry in 0..9 {
        storage
            .ingest(message(entry, &"long input".repeat(100)))
            .await
            .unwrap();
    }
    for _ in 0..8 {
        storage.claim().await.unwrap().unwrap();
    }
    expire_claims().await;
    let mut first = None;
    for _ in 0..8 {
        let claim = storage.claim().await.unwrap().unwrap();
        assert!(claim.job.range.start < 8);
        if claim.job.range.start == 0 {
            first = Some(claim);
        }
    }
    assert!(storage.claim().await.unwrap().is_none());
    publish(&storage, &first.unwrap(), "first complete").await;
    let ninth = storage.claim().await.unwrap().unwrap();
    assert_eq!(ninth.job.range.start, 8);
    assert_eq!(ninth.job.context.cutoff, 1);
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
    let worker = Worker::new(queue().await, Arc::clone(&storage), move |job: Job| {
        let contexts = contexts.clone();
        async move {
            contexts.send(job.context.clone()).await.unwrap();
            summarize(job).await
        }
    });
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
