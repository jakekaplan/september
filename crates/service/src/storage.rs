//! Atomic storage contracts and backend implementations.

use september_memory::Node;
use std::{future::Future, time::Duration};
use uuid::Uuid;

use crate::{
    Error,
    archive::{Message, Receipt},
    jobs::{Claim, Completion},
    snapshots::{Detail, Snapshot},
};

mod memory;
pub use memory::InMemory;

/// Operations are atomic across messages, jobs, views, and snapshots.
///
/// Backends must preserve immutable publications, source deduplication, ordered
/// IDs, claim fencing, and frozen cutoffs. Durable backends must commit before
/// acknowledging; volatile backends must advertise that limitation explicitly.
/// Errors leave logical state unchanged. No lock may span external model work.
pub trait Storage: Send + Sync + 'static {
    /// Whether acknowledged state survives process restarts.
    fn is_durable(&self) -> bool;

    /// Save a finalized message and its summary work, or return its prior ID.
    fn ingest(&self, message: Message) -> impl Future<Output = Result<Receipt, Error>> + Send;

    /// Freeze the current archive cutoff, returning an existing snapshot on retry.
    /// The caller generates one UUID per interaction before its first request.
    fn prepare(&self, id: Uuid) -> impl Future<Output = Result<Snapshot, Error>> + Send;

    /// Read the original cutoff's readiness and fixed view.
    fn snapshot(&self, id: Uuid) -> impl Future<Output = Result<Snapshot, Error>> + Send;

    /// Retrieve only content permitted by a ready snapshot's frozen cover.
    fn zoom(&self, id: Uuid, node: Node) -> impl Future<Output = Result<Detail, Error>> + Send;

    /// Claim one ready job, recovering expired claims; at most eight are active.
    /// Leaves require fewer than eight earlier unbuilt leaves. Freeze historical
    /// context on first claim and retain it across release, expiry, and retries.
    /// Defer jobs whose context exceeds 32,000 bytes while ready parents progress.
    fn claim(&self) -> impl Future<Output = Result<Option<Claim>, Error>> + Send;

    /// Extend a live claim by another 60 seconds. Reject expired or replaced tokens.
    fn renew(&self, node: Node, token: Uuid) -> impl Future<Output = Result<(), Error>> + Send;

    /// Release a live claim, making it available after `retry_after`.
    /// Shutdown uses zero delay; exhausted attempts use a backoff.
    fn release(
        &self,
        node: Node,
        token: Uuid,
        retry_after: Duration,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Publish once under a live claim and enqueue newly ready parents atomically.
    /// Retrying an identical successful completion is allowed.
    fn complete(&self, completion: Completion) -> impl Future<Output = Result<(), Error>> + Send;
}
