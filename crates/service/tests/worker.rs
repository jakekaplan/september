//! Continuous worker behavior across Docket, storage, and frozen snapshots.

#![expect(
    clippy::unwrap_used,
    reason = "test failures should identify broken invariants"
)]

use std::{
    error::Error as StdError,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use docket::Docket;
use september::{
    Error,
    archive::{Kind, Message, Source},
    jobs::{Claim, Completion, Job, MAX_SUMMARY_BYTES},
    snapshots::{Detail, Snapshot},
    storage::{Archive, InMemory, Jobs},
    worker::Worker,
};
use september_memory::{Budget, Node};
use tokio::{
    sync::{Notify, Semaphore, oneshot, watch},
    time::{advance, pause, resume, sleep, timeout},
};
use uuid::Uuid;

/// A synthetic message too long to publish verbatim, so it becomes a job.
fn message(entry: usize) -> Message {
    Message {
        source: Source {
            harness: "worker-test".into(),
            session: "synthetic".into(),
            entry: entry.to_string(),
            part: 0,
        },
        project: "synthetic".into(),
        branch: "test".into(),
        timestamp_ms: 0,
        kind: Kind::User,
        call_id: None,
        text: format!("Synthetic message {entry}. ").repeat(40),
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

async fn ready(
    storage: &impl Archive,
    id: Uuid,
) -> Result<Snapshot, Box<dyn StdError + Send + Sync>> {
    Ok(timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = storage.snapshot(id).await?;
            if matches!(snapshot, Snapshot::Ready { .. }) {
                return Ok::<_, Error>(snapshot);
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await??)
}

// Observe the worker's claims and renewals so virtual-time checks need no polling.
struct Observed {
    inner: InMemory,
    token: watch::Sender<Option<Uuid>>,
    renewals: watch::Sender<usize>,
}

impl Default for Observed {
    fn default() -> Self {
        Self {
            inner: InMemory::default(),
            token: watch::channel(None).0,
            renewals: watch::channel(0).0,
        }
    }
}

impl Jobs for Observed {
    async fn claim(&self) -> Result<Option<Claim>, Error> {
        let claim = self.inner.claim().await?;
        if let Some(claim) = &claim {
            self.token.send_replace(Some(claim.token));
        }
        Ok(claim)
    }
    async fn renew(&self, node: Node, token: Uuid) -> Result<(), Error> {
        self.inner.renew(node, token).await?;
        self.renewals.send_modify(|count| *count += 1);
        Ok(())
    }
    async fn complete(&self, completion: Completion) -> Result<(), Error> {
        self.inner.complete(completion).await
    }
}

#[tokio::test]
async fn accepts_new_work_while_running_and_preserves_frozen_snapshots() {
    let storage = Arc::new(InMemory::new(Budget::new(80, 160).unwrap()));
    let (calls, mut called) = watch::channel(0);
    let gate = Arc::new(Semaphore::new(0));
    let worker = Worker::new(queue().await, Arc::clone(&storage), {
        let gate = Arc::clone(&gate);
        move |job: Job| {
            let gate = Arc::clone(&gate);
            let calls = calls.clone();
            async move {
                let _permit = gate.acquire_owned().await.unwrap();
                let summary = summarize(job).await;
                calls.send_modify(|count| *count += 1);
                summary
            }
        }
    });
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    for entry in 0..8 {
        storage.ingest(message(entry)).await.unwrap();
    }
    let id = Uuid::new_v4();
    assert!(matches!(
        storage.prepare(id, None).await.unwrap(),
        Snapshot::Pending { cutoff: 8, .. }
    ));
    gate.add_permits(8);
    timeout(
        Duration::from_secs(5),
        // Stand-in summaries are short, so every parent joins them verbatim.
        called.wait_for(|count| *count == 8),
    )
    .await
    .unwrap()
    .unwrap();
    let frozen = serde_json::to_value(ready(storage.as_ref(), id).await.unwrap()).unwrap();
    storage.ingest(message(8)).await.unwrap();
    let later = Uuid::new_v4();
    storage.prepare(later, None).await.unwrap();
    assert!(matches!(
        ready(storage.as_ref(), later).await.unwrap(),
        Snapshot::Ready { cutoff: 9, .. }
    ));
    assert_eq!(
        serde_json::to_value(storage.snapshot(id).await.unwrap()).unwrap(),
        frozen
    );
    let Detail::Message {
        message: original, ..
    } = storage.zoom(later, Node::new(0, 1).unwrap()).await.unwrap()
    else {
        panic!("expected original");
    };
    assert_eq!(original, message(0));
    stop.send(()).unwrap();
    timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn eight_jobs_drain_on_shutdown_without_claiming_more_work() {
    let storage = Arc::new(InMemory::default());
    for entry in 0..16 {
        storage.ingest(message(entry)).await.unwrap();
    }
    let (calls, mut called) = watch::channel(0);
    let gate = Arc::new(Semaphore::new(0));
    let worker = Worker::new(queue().await, Arc::clone(&storage), {
        let gate = Arc::clone(&gate);
        move |job: Job| {
            let gate = Arc::clone(&gate);
            let calls = calls.clone();
            async move {
                calls.send_modify(|count| *count += 1);
                let _permit = gate.acquire_owned().await.unwrap();
                summarize(job).await
            }
        }
    });
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    timeout(Duration::from_secs(5), called.wait_for(|count| *count == 8))
        .await
        .unwrap()
        .unwrap();
    assert!(storage.claim().await.unwrap().is_none());
    stop.send(()).unwrap();
    gate.add_permits(8);
    timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(*called.borrow(), 8);
    let next = storage.claim().await.unwrap().unwrap();
    assert_eq!(next.job.range.start, 8);
    assert_eq!(next.job.range.length, 1);
}

#[tokio::test]
async fn transient_failure_retries_and_completes() {
    let storage = Arc::new(InMemory::default());
    storage.ingest(message(0)).await.unwrap();
    let id = Uuid::new_v4();
    storage.prepare(id, None).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = Worker::new(queue().await, Arc::clone(&storage), {
        let calls = Arc::clone(&calls);
        move |job: Job| {
            let calls = Arc::clone(&calls);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(Error::NotReady);
                }
                summarize(job).await
            }
        }
    });
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    ready(storage.as_ref(), id).await.unwrap();
    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn renewal_keeps_a_slow_summary_valid_beyond_its_original_lease() {
    let storage = Arc::new(Observed::default());
    let mut renewed = storage.renewals.subscribe();
    storage.inner.ingest(message(0)).await.unwrap();
    let id = Uuid::new_v4();
    storage.inner.prepare(id, None).await.unwrap();
    let started = Arc::new(Notify::new());
    let gate = Arc::new(Semaphore::new(0));
    let worker = Worker::new(queue().await, Arc::clone(&storage), {
        let started = Arc::clone(&started);
        let gate = Arc::clone(&gate);
        move |job: Job| {
            let started = Arc::clone(&started);
            let gate = Arc::clone(&gate);
            async move {
                started.notify_one();
                let _permit = gate.acquire_owned().await.unwrap();
                summarize(job).await
            }
        }
    });
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    assert_eq!(*renewed.borrow_and_update(), 1);
    pause();
    for _ in 0..3 {
        advance(Duration::from_secs(20)).await;
        timeout(Duration::from_secs(1), renewed.changed())
            .await
            .unwrap()
            .unwrap();
        renewed.borrow_and_update();
    }
    advance(Duration::from_secs(5)).await;
    assert!(storage.claim().await.unwrap().is_none());
    resume();
    gate.add_permits(1);
    ready(&storage.inner, id).await.unwrap();
    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn rejected_results_retry_then_their_claim_lapses_for_another_worker() {
    let storage = Arc::new(InMemory::default());
    storage.ingest(message(0)).await.unwrap();
    let id = Uuid::new_v4();
    storage.prepare(id, None).await.unwrap();
    let (calls, mut called) = watch::channel(0);
    let worker = Worker::new(queue().await, Arc::clone(&storage), move |_| {
        calls.send_modify(|count| *count += 1);
        std::future::ready(Ok("x".repeat(MAX_SUMMARY_BYTES + 1)))
    });
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    timeout(
        Duration::from_secs(10),
        called.wait_for(|count| *count == 3),
    )
    .await
    .unwrap()
    .unwrap();
    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
    assert_eq!(*called.borrow(), 3);
    assert!(matches!(
        storage.snapshot(id).await.unwrap(),
        Snapshot::Pending { .. }
    ));
    assert!(storage.claim().await.unwrap().is_none());
    pause();
    advance(Duration::from_secs(60)).await;
    assert!(storage.claim().await.unwrap().is_some());
    resume();
}

#[tokio::test]
async fn shutdown_deadline_cancels_the_model_and_its_claim_lapses() {
    let storage = Arc::new(Observed::default());
    storage.inner.ingest(message(0)).await.unwrap();
    let started = Arc::new(Notify::new());
    let held = Arc::new(Semaphore::new(1));
    let worker = Worker::new(queue().await, Arc::clone(&storage), {
        let started = Arc::clone(&started);
        let held = Arc::clone(&held);
        move |_| {
            let started = Arc::clone(&started);
            let held = Arc::clone(&held);
            async move {
                let _permit = held.acquire_owned().await.unwrap();
                started.notify_one();
                std::future::pending::<Result<String, Error>>().await
            }
        }
    });
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let old = storage.token.borrow().unwrap();
    pause();
    stop.send(()).unwrap();
    timeout(Duration::from_secs(20), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(held.available_permits(), 1);
    assert!(storage.claim().await.unwrap().is_none());
    advance(Duration::from_secs(60)).await;
    let replacement = storage.claim().await.unwrap().unwrap();
    resume();
    assert_ne!(old, replacement.token);
    assert!(matches!(
        storage
            .complete(Completion {
                range: replacement.job.range,
                token: old,
                text: "late".into()
            })
            .await,
        Err(Error::ClaimLost)
    ));
    storage
        .complete(Completion {
            range: replacement.job.range,
            token: replacement.token,
            text: "replacement".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn aborting_the_worker_cancels_children_and_recovers_by_expiry() {
    let storage = Arc::new(InMemory::default());
    storage.ingest(message(0)).await.unwrap();
    let started = Arc::new(Notify::new());
    let held = Arc::new(Semaphore::new(1));
    let worker = Worker::new(queue().await, Arc::clone(&storage), {
        let started = Arc::clone(&started);
        let held = Arc::clone(&held);
        move |_| {
            let started = Arc::clone(&started);
            let held = Arc::clone(&held);
            async move {
                let _permit = held.acquire_owned().await.unwrap();
                started.notify_one();
                std::future::pending::<Result<String, Error>>().await
            }
        }
    });
    let running = tokio::spawn(worker.run(std::future::pending()));
    timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    let permit = timeout(Duration::from_secs(1), held.acquire())
        .await
        .unwrap()
        .unwrap();
    drop(permit);
    assert!(storage.claim().await.unwrap().is_none());
    pause();
    advance(Duration::from_secs(60)).await;
    assert!(storage.claim().await.unwrap().is_some());
    resume();
}
