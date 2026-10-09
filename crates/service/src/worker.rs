//! Continuous, bounded summary execution through Docket.
//!
//! September storage stays authoritative for readiness, claims, and publication.
//! Docket runs the background work: a perpetual dispatch task claims ready jobs
//! and adds one summary task per claim, with Docket's retries and timeouts.

use std::{future::Future, sync::Arc, time::Duration};

use docket::{Docket, ExponentialRetry, Perpetual, Task, Timeout};
use september_memory::Node;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::oneshot,
    time::{Instant, MissedTickBehavior, interval_at, timeout},
};
use uuid::Uuid;

use crate::{
    Error,
    jobs::{Claim, Completion, Job, MAX_CLAIMS},
    storage::Storage,
};

const DISPATCH_EVERY: Duration = Duration::from_millis(250);
const RENEW_EVERY: Duration = Duration::from_secs(20);
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Claims ready jobs and adds a [`Summarize`] task for each.
#[derive(Default, Deserialize, Serialize, Task)]
#[task(name = "dispatch_summaries", output = ())]
struct Dispatch {}

/// Builds one claimed summary.
#[derive(Deserialize, Serialize, Task)]
#[task(name = "summarize", output = ())]
struct Summarize {
    // Docket hides task arguments from its logs unless explicitly marked logged.
    claim: Claim,
}

/// A Docket queue registered to dispatch and build summaries.
pub struct Worker {
    docket: Docket,
}

impl Worker {
    /// Create a volatile Docket queue and register the supplied summarizer.
    /// The callback receives a job's source data and frozen historical context.
    /// Both are untrusted data, never instructions to execute.
    ///
    /// # Errors
    /// Returns a service error if Docket cannot initialize its memory backend.
    pub async fn memory<S, F, Fut>(storage: Arc<S>, summarize: F) -> Result<Self, Error>
    where
        S: Storage,
        F: Fn(Job) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, Error>> + Send,
    {
        let docket = Docket::connect("september", &format!("memory://{}", Uuid::new_v4()))
            .await
            .map_err(|error| Error::internal("initialize summary queue", error))?;
        let claims = Arc::clone(&storage);
        docket
            .register(move |run: docket::Context, _: Dispatch| {
                let storage = Arc::clone(&claims);
                let docket = run.docket().clone();
                async move { dispatch(&*storage, &docket).await }
            })
            .with(Perpetual::every(DISPATCH_EVERY).automatic());
        let summarize = Arc::new(summarize);
        docket
            .register(move |_, task: Summarize| {
                let storage = Arc::clone(&storage);
                let summarize = Arc::clone(&summarize);
                async move { build(&*storage, &*summarize, task.claim).await }
            })
            .with(ExponentialRetry::attempts(3))
            .with(Timeout::after(SUMMARY_TIMEOUT));
        Ok(Self { docket })
    }

    /// Run until shutdown, then let running summaries finish for ten seconds.
    /// Summaries still running at that deadline are cancelled; their claims
    /// lapse and become claimable again.
    ///
    /// # Errors
    /// Returns Docket infrastructure errors. A failed summary is retried by
    /// Docket, then by a later claim; its snapshots stay pending meanwhile.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), Error> {
        let (stop, stopped) = oneshot::channel::<()>();
        let worker = docket::Worker::new(self.docket)
            .concurrency(MAX_CLAIMS + 1)
            .run_until(async {
                let _ = stopped.await;
            });
        tokio::pin!(worker, shutdown);
        let result = tokio::select! {
            result = &mut worker => result,
            () = &mut shutdown => {
                let _ = stop.send(());
                timeout(SHUTDOWN_GRACE, &mut worker).await.unwrap_or_else(|_| {
                    tracing::warn!("summary drain deadline reached; cancelling running summaries");
                    Ok(())
                })
            }
        };
        result.map_err(|error| Error::internal("run summary worker", error))
    }
}

/// Add a summary task for every job storage lets this worker claim. Storage
/// bounds active claims, so this ends; a claim that fails to enqueue lapses.
async fn dispatch(storage: &impl Storage, docket: &Docket) -> Result<(), Error> {
    while let Some(claim) = storage.claim().await? {
        let key = claim.token.to_string();
        docket
            .add(Summarize { claim })
            .key(&key)
            .await
            .map_err(|error| Error::internal("enqueue summary", error))?;
    }
    Ok(())
}

/// Summarize a claimed job, renewing its claim until the summary is published.
/// A replaced claim means another worker owns the job, so this one stops.
async fn build<F, Fut>(storage: &impl Storage, summarize: &F, claim: Claim) -> Result<(), Error>
where
    F: Fn(Job) -> Fut,
    Fut: Future<Output = Result<String, Error>>,
{
    let Claim { job, token, .. } = claim;
    let range = job.range;
    let node = Node::try_from(range)?;
    // Refuse a stale delivery before spending time on a summary.
    if replaced(storage.renew(node, token).await)? {
        return Ok(());
    }
    let summary = summarize(job);
    tokio::pin!(summary);
    let mut renewal = interval_at(Instant::now() + RENEW_EVERY, RENEW_EVERY);
    renewal.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let text = loop {
        tokio::select! {
            biased;
            text = &mut summary => break text?,
            _ = renewal.tick() => {
                if replaced(storage.renew(node, token).await)? {
                    return Ok(());
                }
            }
        }
    };
    let completion = Completion { range, token, text };
    replaced(storage.complete(completion).await)?;
    Ok(())
}

fn replaced(result: Result<(), Error>) -> Result<bool, Error> {
    match result {
        Ok(()) => Ok(false),
        Err(Error::Conflict) => Ok(true),
        Err(error) => Err(error),
    }
}
