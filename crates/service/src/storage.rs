//! Atomic storage contracts and backend implementations.

use std::future::Future;

use september_memory::Node;
use uuid::Uuid;

use crate::{
    Error,
    archive::{Message, Receipt},
    jobs::{Claim, Completion},
    snapshots::{Detail, Snapshot},
};

mod memory;
mod sqlite;
pub use memory::InMemory;
pub use sqlite::Sqlite;

/// Messages, interaction snapshots, and zoom: what clients use.
///
/// Operations are atomic across messages, summaries, views, and snapshots.
/// Backends must preserve immutable publications, source deduplication, ordered
/// IDs, and frozen cutoffs. Durable backends must commit before acknowledging;
/// volatile backends must advertise that limitation explicitly. Errors leave
/// logical state unchanged.
pub trait Archive: Send + Sync + 'static {
    /// Whether acknowledged state survives process restarts.
    fn is_durable(&self) -> bool;

    /// Save a finalized message and its summary work, or return its prior ID.
    fn ingest(&self, message: Message) -> impl Future<Output = Result<Receipt, Error>> + Send;

    /// Freeze the current archive cutoff, returning an existing snapshot on retry.
    /// The caller generates one UUID per interaction before its first request.
    ///
    /// `within` asks for a view of at most that many bytes, for a harness that
    /// takes less context: the frozen copy is merged further through built
    /// parents, and can stay larger while they are unbuilt.
    fn prepare(
        &self,
        id: Uuid,
        within: Option<usize>,
    ) -> impl Future<Output = Result<Snapshot, Error>> + Send;

    /// Read the original cutoff's readiness and fixed view.
    fn snapshot(&self, id: Uuid) -> impl Future<Output = Result<Snapshot, Error>> + Send;

    /// Retrieve only content permitted by a ready snapshot's frozen cover.
    fn zoom(&self, id: Uuid, node: Node) -> impl Future<Output = Result<Detail, Error>> + Send;
}

/// Summary jobs: what workers use.
///
/// Claims are fenced by token and expire unless renewed; no lock may span
/// external model work. Publication shares the archive's atomicity.
pub trait Jobs: Send + Sync + 'static {
    /// Claim one ready job, recovering expired claims; at most eight are active.
    /// Leaves require fewer than eight earlier unbuilt leaves. Freeze historical
    /// context on first claim and retain it across expiry and retries.
    /// Defer jobs whose context exceeds 32,000 bytes while ready parents progress.
    fn claim(&self) -> impl Future<Output = Result<Option<Claim>, Error>> + Send;

    /// Extend a live claim by another lease. A claim that is not renewed lapses,
    /// and its job becomes claimable again.
    ///
    /// Returns [`Error::ClaimLost`] for an expired or replaced token.
    fn renew(&self, node: Node, token: Uuid) -> impl Future<Output = Result<(), Error>> + Send;

    /// Publish once under a live claim, atomically with every parent whose two
    /// children now fit together verbatim; enqueue the first parent that does not.
    /// Retrying an identical successful completion is allowed.
    ///
    /// Returns [`Error::ClaimLost`] if the claim expired or was replaced.
    fn complete(&self, completion: Completion) -> impl Future<Output = Result<(), Error>> + Send;
}
