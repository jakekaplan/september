//! Continuous worker behavior across Docket, storage, and frozen snapshots.

use std::{
    error::Error as StdError,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use september::{
    Error,
    archive::{Message, Receipt},
    jobs::{Claim, Completion},
    snapshots::{Detail, Snapshot},
    storage::{InMemory, Storage},
    worker::{Worker, fake},
};
use september_memory::{Budget, Node};
use tokio::{
    sync::{Notify, Semaphore, oneshot, watch},
    time::{advance, pause, resume, sleep, timeout},
};
use uuid::Uuid;

#[path = "../examples/docket.rs"]
mod example;

async fn ready(
    storage: &impl Storage,
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

// Observe successful claim transitions so virtual-time checks need no polling guesses.
struct Observed {
    inner: InMemory,
    token: watch::Sender<Option<Uuid>>,
    renewals: watch::Sender<usize>,
    releases: watch::Sender<Option<Duration>>,
}

impl Default for Observed {
    fn default() -> Self {
        Self {
            inner: InMemory::default(),
            token: watch::channel(None).0,
            renewals: watch::channel(0).0,
            releases: watch::channel(None).0,
        }
    }
}

impl Storage for Observed {
    fn is_durable(&self) -> bool {
        false
    }
    async fn ingest(&self, message: Message) -> Result<Receipt, Error> {
        self.inner.ingest(message).await
    }
    async fn prepare(&self, id: Uuid) -> Result<Snapshot, Error> {
        self.inner.prepare(id).await
    }
    async fn snapshot(&self, id: Uuid) -> Result<Snapshot, Error> {
        self.inner.snapshot(id).await
    }
    async fn zoom(&self, id: Uuid, node: Node) -> Result<Detail, Error> {
        self.inner.zoom(id, node).await
    }
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
    async fn release(&self, node: Node, token: Uuid, delay: Duration) -> Result<(), Error> {
        self.inner.release(node, token, delay).await?;
        self.releases.send_replace(Some(delay));
        Ok(())
    }
    async fn complete(&self, completion: Completion) -> Result<(), Error> {
        self.inner.complete(completion).await
    }
}

#[test]
fn runnable_demo_uses_the_continuous_worker() {
    assert_eq!(example::main(), ExitCode::SUCCESS);
}

#[tokio::test]
async fn accepts_new_work_while_running_and_preserves_frozen_snapshots() {
    let storage = Arc::new(InMemory::new(Budget::new(80, 160).unwrap()));
    let (calls, mut called) = watch::channel(0);
    let gate = Arc::new(Semaphore::new(0));
    let worker = Worker::memory(Arc::clone(&storage), {
        let gate = Arc::clone(&gate);
        move |input, context| {
            let gate = Arc::clone(&gate);
            let calls = calls.clone();
            async move {
                let _permit = gate.acquire_owned().await.unwrap();
                let summary = fake(input, context).await;
                calls.send_modify(|count| *count += 1);
                summary
            }
        }
    })
    .await
    .unwrap();
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    for entry in 0..8 {
        storage.ingest(example::message(entry)).await.unwrap();
    }
    let id = Uuid::new_v4();
    assert!(matches!(
        storage.prepare(id).await.unwrap(),
        Snapshot::Pending { cutoff: 8, .. }
    ));
    gate.add_permits(8);
    timeout(
        Duration::from_secs(5),
        called.wait_for(|count| *count == 15),
    )
    .await
    .unwrap()
    .unwrap();
    let frozen = serde_json::to_value(ready(storage.as_ref(), id).await.unwrap()).unwrap();
    storage.ingest(example::message(8)).await.unwrap();
    let later = Uuid::new_v4();
    storage.prepare(later).await.unwrap();
    assert!(matches!(
        ready(storage.as_ref(), later).await.unwrap(),
        Snapshot::Ready { cutoff: 9, .. }
    ));
    assert_eq!(
        serde_json::to_value(storage.snapshot(id).await.unwrap()).unwrap(),
        frozen
    );
    let Detail::Message { message, .. } =
        storage.zoom(later, Node::new(0, 1).unwrap()).await.unwrap()
    else {
        panic!("expected original");
    };
    assert_eq!(message, example::message(0));
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
        storage.ingest(example::message(entry)).await.unwrap();
    }
    let (calls, mut called) = watch::channel(0);
    let gate = Arc::new(Semaphore::new(0));
    let worker = Worker::memory(Arc::clone(&storage), {
        let gate = Arc::clone(&gate);
        move |input, context| {
            let gate = Arc::clone(&gate);
            let calls = calls.clone();
            async move {
                calls.send_modify(|count| *count += 1);
                let _permit = gate.acquire_owned().await.unwrap();
                fake(input, context).await
            }
        }
    })
    .await
    .unwrap();
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
    assert_eq!(next.range.start, 8);
    assert_eq!(next.range.length, 1);
}

#[tokio::test]
async fn transient_failure_retries_and_completes() {
    let storage = Arc::new(InMemory::default());
    storage.ingest(example::message(0)).await.unwrap();
    let id = Uuid::new_v4();
    storage.prepare(id).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = Worker::memory(Arc::clone(&storage), {
        let calls = Arc::clone(&calls);
        move |input, context| {
            let calls = Arc::clone(&calls);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(Error::NotReady);
                }
                fake(input, context).await
            }
        }
    })
    .await
    .unwrap();
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
    storage.ingest(example::message(0)).await.unwrap();
    let id = Uuid::new_v4();
    storage.prepare(id).await.unwrap();
    let started = Arc::new(Notify::new());
    let gate = Arc::new(Semaphore::new(0));
    let worker = Worker::memory(Arc::clone(&storage), {
        let started = Arc::clone(&started);
        let gate = Arc::clone(&gate);
        move |input, context| {
            let started = Arc::clone(&started);
            let gate = Arc::clone(&gate);
            async move {
                started.notify_one();
                let _permit = gate.acquire_owned().await.unwrap();
                fake(input, context).await
            }
        }
    })
    .await
    .unwrap();
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
    ready(storage.as_ref(), id).await.unwrap();
    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn oversized_results_back_off_and_leave_the_snapshot_pending() {
    let storage = Arc::new(Observed::default());
    let mut released = storage.releases.subscribe();
    storage.ingest(example::message(0)).await.unwrap();
    let id = Uuid::new_v4();
    storage.prepare(id).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = Worker::memory(Arc::clone(&storage), {
        let calls = Arc::clone(&calls);
        move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok("🙂".repeat(129)))
        }
    })
    .await
    .unwrap();
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    timeout(Duration::from_secs(10), released.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*released.borrow(), Some(Duration::from_secs(30)));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(matches!(
        storage.snapshot(id).await.unwrap(),
        Snapshot::Pending { .. }
    ));
    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
    pause();
    advance(Duration::from_secs(29)).await;
    assert!(storage.claim().await.unwrap().is_none());
    advance(Duration::from_secs(1)).await;
    resume();
    let worker = Worker::memory(Arc::clone(&storage), fake).await.unwrap();
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    ready(storage.as_ref(), id).await.unwrap();
    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_deadline_cancels_the_model_and_releases_its_claim() {
    let storage = Arc::new(Observed::default());
    storage.ingest(example::message(0)).await.unwrap();
    let started = Arc::new(Notify::new());
    let held = Arc::new(Semaphore::new(1));
    let worker = Worker::memory(Arc::clone(&storage), {
        let started = Arc::clone(&started);
        let held = Arc::clone(&held);
        move |_, _| {
            let started = Arc::clone(&started);
            let held = Arc::clone(&held);
            async move {
                let _permit = held.acquire_owned().await.unwrap();
                started.notify_one();
                std::future::pending::<Result<String, Error>>().await
            }
        }
    })
    .await
    .unwrap();
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
    resume();
    assert_eq!(held.available_permits(), 1);
    let replacement = storage.claim().await.unwrap().unwrap();
    assert_ne!(old, replacement.token);
    assert!(matches!(
        storage
            .complete(Completion {
                range: replacement.range,
                token: old,
                text: "late".into()
            })
            .await,
        Err(Error::Conflict)
    ));
    storage
        .complete(Completion {
            range: replacement.range,
            token: replacement.token,
            text: "replacement".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn aborting_the_worker_cancels_children_and_recovers_by_expiry() {
    let storage = Arc::new(InMemory::default());
    storage.ingest(example::message(0)).await.unwrap();
    let started = Arc::new(Notify::new());
    let held = Arc::new(Semaphore::new(1));
    let worker = Worker::memory(Arc::clone(&storage), {
        let started = Arc::clone(&started);
        let held = Arc::clone(&held);
        move |_, _| {
            let started = Arc::clone(&started);
            let held = Arc::clone(&held);
            async move {
                let _permit = held.acquire_owned().await.unwrap();
                started.notify_one();
                std::future::pending::<Result<String, Error>>().await
            }
        }
    })
    .await
    .unwrap();
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
