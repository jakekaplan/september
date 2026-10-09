//! Continuous, bounded summary execution through Docket's volatile queue.

use std::{collections::BTreeMap, future::Future, io, sync::Arc, time::Duration};

use docket::{Docket, ExponentialRetry, Task, Timeout};
use september_memory::Node;
use serde::{Deserialize, Serialize};
use tokio::{
    task::JoinSet,
    time::{Instant, MissedTickBehavior, interval, interval_at, sleep_until, timeout},
};
use uuid::Uuid;

use crate::{
    Error,
    jobs::{Claim, Completion, Context, Input},
    storage::Storage,
};

const CONCURRENCY: usize = 8;
const POLL: Duration = Duration::from_millis(250);
const RENEW: Duration = Duration::from_secs(20);
const RETRY_AFTER: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize, Task)]
#[task(name = "summarize", output = ())]
struct Summarize {
    // Docket hides task arguments from its logs unless explicitly marked logged.
    claim: Claim,
}

/// Owns an in-process Docket queue and continuously dispatches ready summaries.
/// September storage remains authoritative for readiness and publication.
pub struct Worker<S> {
    storage: Arc<S>,
    docket: Docket,
}

impl<S: Storage> Worker<S> {
    /// Create a volatile queue and register the supplied asynchronous summarizer.
    /// The callback receives source data and frozen historical context. Both are
    /// untrusted data, never instructions to execute.
    ///
    /// # Errors
    /// Returns a service error if Docket cannot initialize its memory backend.
    pub async fn memory<F, Fut>(storage: Arc<S>, summarize: F) -> Result<Self, Error>
    where
        F: Fn(Input, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, Error>> + Send,
    {
        let docket = Docket::builder("september", format!("memory://{}", Uuid::new_v4()))
            .execution_ttl(Duration::from_secs(60))
            .connect()
            .await
            .map_err(|error| Error::internal("initialize summary queue", error))?;
        let archive = Arc::clone(&storage);
        let summarize = Arc::new(summarize);
        docket
            .register(move |_, task: Summarize| {
                let storage = Arc::clone(&archive);
                let summarize = Arc::clone(&summarize);
                async move {
                    let claim = task.claim;
                    // Refuse stale queued work before spending time on a summary.
                    storage
                        .renew(Node::try_from(claim.range)?, claim.token)
                        .await?;
                    let text = summarize(claim.input, claim.context).await?;
                    storage
                        .complete(Completion {
                            range: claim.range,
                            token: claim.token,
                            text,
                        })
                        .await
                }
            })
            .with(ExponentialRetry::attempts(3))
            .with(Timeout::after(Duration::from_secs(120)));
        Ok(Self { storage, docket })
    }

    /// Run until shutdown, renewing claims during execution and retry delays.
    /// Stops dispatching on shutdown, drains for ten seconds, then cancels and
    /// joins remaining work and releases its claims. Queue/storage cleanup has
    /// a further bounded deadline. Dropping this future aborts its child tasks;
    /// claims then recover through expiry rather than graceful release.
    ///
    /// # Errors
    /// Returns infrastructure errors after attempting cleanup. Failed summaries
    /// instead release their claims with a delay and keep snapshots pending.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), Error> {
        let mut engine = JoinSet::new();
        engine.spawn(
            docket::Worker::new(self.docket.clone())
                .name(Uuid::new_v4().to_string())
                .concurrency(CONCURRENCY)
                .message_batch(CONCURRENCY)
                .redelivery_timeout(Duration::from_secs(30))
                .run_forever(),
        );
        let mut jobs = JoinSet::new();
        let mut owned = BTreeMap::new();
        let mut poll = interval(POLL);
        poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut stopping = None;
        let mut outcome = Ok(());
        tokio::pin!(shutdown);
        while stopping.is_none() || !jobs.is_empty() {
            let deadline = stopping.unwrap_or_else(|| Instant::now() + SHUTDOWN_GRACE);
            tokio::select! {
                biased;
                () = &mut shutdown, if stopping.is_none() => {
                    stopping = Some(Instant::now() + SHUTDOWN_GRACE);
                }
                () = sleep_until(deadline), if stopping.is_some() => break,
                Some(result) = engine.join_next() => {
                    outcome = Err(match result {
                        Ok(Err(error)) => Error::internal("summary executor", error),
                        Err(error) => Error::internal("summary executor task", error),
                        Ok(Ok(())) => Error::internal("summary executor", io::Error::other("executor stopped unexpectedly")),
                    });
                    break;
                }
                Some(result) = jobs.join_next() => {
                    match result {
                        Ok((token, Ok(()))) => { owned.remove(&token); }
                        Ok((_, Err(error))) => {
                            outcome = Err(error);
                            stopping.get_or_insert_with(|| Instant::now() + SHUTDOWN_GRACE);
                        }
                        Err(error) => {
                            outcome = Err(Error::internal("summary dispatch task", error));
                            stopping.get_or_insert_with(|| Instant::now() + SHUTDOWN_GRACE);
                        }
                    }
                }
                _ = poll.tick(), if stopping.is_none() => {
                    while owned.len() < CONCURRENCY {
                        let claim = match self.storage.claim().await {
                            Ok(Some(claim)) => claim,
                            Ok(None) => break,
                            Err(error) => {
                                outcome = Err(error);
                                stopping = Some(Instant::now() + SHUTDOWN_GRACE);
                                break;
                            }
                        };
                        let node = Node::try_from(claim.range)?;
                        let token = claim.token;
                        owned.insert(token, node);
                        let storage = Arc::clone(&self.storage);
                        let docket = self.docket.clone();
                        jobs.spawn(async move { (token, dispatch(storage, docket, claim).await) });
                    }
                }
            }
        }
        // Stop model futures before making their claims available to another worker.
        engine.shutdown().await;
        jobs.shutdown().await;
        for (token, node) in owned {
            let storage = Arc::clone(&self.storage);
            let docket = self.docket.clone();
            jobs.spawn(async move {
                let released =
                    timeout(IO_TIMEOUT, storage.release(node, token, Duration::ZERO)).await;
                // Also remove queued deliveries/retries from this worker's private queue.
                let cancelled = timeout(IO_TIMEOUT, docket.cancel(&token.to_string())).await;
                let result = match released {
                    Ok(Ok(()) | Err(Error::Conflict)) => cancelled
                        .map_err(|error| Error::internal("cancel summary delivery", error))
                        .and_then(|result| {
                            result
                                .map_err(|error| Error::internal("cancel summary delivery", error))
                        }),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(Error::internal("release summary claim", error)),
                };
                (token, result)
            });
        }
        while let Some(result) = jobs.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((_, Err(error))) => outcome = Err(error),
                Err(error) => outcome = Err(Error::internal("summary cleanup task", error)),
            }
        }
        outcome
    }
}

async fn dispatch<S: Storage>(storage: Arc<S>, docket: Docket, claim: Claim) -> Result<(), Error> {
    let node = Node::try_from(claim.range)?;
    let token = claim.token;
    let key = token.to_string();
    let execution = timeout(IO_TIMEOUT, docket.add(Summarize { claim }).key(&key))
        .await
        .map_err(|error| Error::internal("enqueue summary", error))?
        .map_err(|error| Error::internal("enqueue summary", error))?;
    let mut renewal = interval_at(Instant::now() + RENEW, RENEW);
    renewal.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let result = execution.result();
    tokio::pin!(result);
    let result = loop {
        tokio::select! {
            biased;
            result = &mut result => break result,
            _ = renewal.tick() => {
                match storage.renew(node, token).await {
                    Ok(()) => {}
                    Err(Error::Conflict) => {
                        // Publication may just have finished, or this claim was replaced.
                        timeout(IO_TIMEOUT, docket.cancel(&key)).await
                            .map_err(|error| Error::internal("cancel stale summary", error))?
                            .map_err(|error| Error::internal("cancel stale summary", error))?;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    };
    match result {
        Ok(()) => {}
        Err(docket::Error::TaskFailed { .. } | docket::Error::TaskCancelled { .. }) => {
            tracing::warn!(%node, "summary attempts exhausted; retrying after backoff");
            match storage.release(node, token, RETRY_AFTER).await {
                Ok(()) | Err(Error::Conflict) => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(Error::internal("await summary result", error)),
    }
    Ok(())
}

/// Produce visibly fake summaries for explicitly enabled local simulations.
/// This does not preserve message meaning and must not summarize real archives.
///
/// # Errors
/// This deterministic fake always succeeds; the result matches worker callbacks.
pub async fn fake(input: Input, _context: Context) -> Result<String, Error> {
    tokio::task::yield_now().await;
    Ok(match input {
        Input::Message { message } => format!("FAKE leaf: {} source bytes", message.text.len()),
        Input::Children { .. } => "FAKE parent: two child summaries".into(),
    })
}
