//! Fenced summary claims and immutable publication inputs.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use september_memory::{Node, SUMMARY_BYTES, Views};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    Error,
    archive::Message,
    snapshots::{Range, Summary},
};

/// The largest accepted summary. The view measures real sizes, so a summary a
/// little over [`SUMMARY_BYTES`] is kept rather than stalling every later snapshot.
pub const MAX_SUMMARY_BYTES: usize = 2 * SUMMARY_BYTES;

/// Summaries built at once across all workers.
pub(crate) const MAX_CLAIMS: usize = 8;

/// A leaf becomes a job only while fewer than this many earlier leaves are unbuilt.
pub(crate) const MAX_ELIGIBLE_LEAVES: usize = 8;

/// How long a claim lives without renewal.
pub(crate) const LEASE: Duration = Duration::from_secs(60);

/// Immutable inputs to a claimed summary job. Workers treat all text as data.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    /// Compress a finalized original with its provenance.
    Message {
        /// Original source record.
        message: Message,
    },
    /// Merge two completed children that do not fit together verbatim.
    Children {
        /// Both children in archive order.
        summaries: [Summary; 2],
    },
}

/// Historical context frozen on a job's first claim and retained across retries.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Context {
    /// Exclusive completed-prefix cutoff; may stop before an earlier unbuilt leaf.
    pub cutoff: u64,
    /// Chronological `<chat>` rendering, at most 32,000 UTF-8 bytes.
    pub view: String,
}

/// One summary to build: its range, its source content, and frozen context.
#[derive(Debug, Deserialize, Serialize)]
pub struct Job {
    /// Range to publish.
    pub range: Range,
    /// Completed source content for the worker.
    pub input: Input,
    /// Bounded historical data to interpret the input; never worker instructions.
    pub context: Context,
}

/// Permission to publish one job, fenced and renewable for 60 seconds at a time.
#[derive(Debug, Deserialize, Serialize)]
pub struct Claim {
    /// The summary to build.
    #[serde(flatten)]
    pub job: Job,
    /// Unique fencing token; an expired or superseded token cannot publish.
    pub token: Uuid,
    /// Claim lifetime measured by the server, not the client's clock.
    pub lease_seconds: u64,
}

/// A worker's measured summary output.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    /// Claimed range.
    pub range: Range,
    /// Current fencing token.
    pub token: Uuid,
    /// Nonempty summary, at most [`MAX_SUMMARY_BYTES`] UTF-8 bytes.
    pub text: String,
}

impl Context {
    /// A new job's context: the compaction-view prefix that comes before `node`,
    /// or `None` while it exceeds the compaction trigger.
    pub(crate) fn select(views: &Views, node: Node) -> Result<Option<Self>, Error> {
        let Some(prefix) = views
            .context(node)
            .map_err(|error| Error::internal("select job context", error))?
        else {
            return Ok(None);
        };
        let view = prefix
            .render()
            .map_err(|error| Error::internal("render job context", error))?;
        Ok(Some(Self {
            cutoff: prefix.cutoff(),
            view,
        }))
    }
}

impl Completion {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.text.trim().is_empty() || self.text.len() > MAX_SUMMARY_BYTES {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

/// Leases on running jobs, fenced by token: at most [`MAX_CLAIMS`] at once,
/// each lapsing [`LEASE`] after it was granted or last renewed.
///
/// Leases belong to the running process, so every backend keeps them in
/// memory; storage keeps the jobs. Expiry visits only the expiry index.
#[derive(Default)]
pub(crate) struct Claims {
    leases: BTreeMap<Node, (Uuid, Instant)>,
    expirations: BTreeSet<(Instant, Node)>,
}

impl Claims {
    /// Drop lapsed leases, returning their jobs.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<Node> {
        let mut lapsed = Vec::new();
        while let Some(&(deadline, node)) = self.expirations.first() {
            if deadline > now {
                break;
            }
            self.release(node);
            lapsed.push(node);
        }
        lapsed
    }

    pub(crate) fn is_full(&self) -> bool {
        self.leases.len() >= MAX_CLAIMS
    }

    pub(crate) fn is_claimed(&self, node: Node) -> bool {
        self.leases.contains_key(&node)
    }

    /// Lease `node` to a new worker, replacing any earlier lease.
    pub(crate) fn grant(&mut self, node: Node, now: Instant) -> Uuid {
        let token = Uuid::new_v4();
        self.lease(node, token, now);
        token
    }

    /// Extend a live lease by another [`LEASE`].
    pub(crate) fn renew(&mut self, node: Node, token: Uuid, now: Instant) -> Result<(), Error> {
        self.check(node, token, now)?;
        self.lease(node, token, now);
        Ok(())
    }

    /// Returns [`Error::ClaimLost`] unless `token` holds a live lease on `node`.
    pub(crate) fn check(&self, node: Node, token: Uuid, now: Instant) -> Result<(), Error> {
        match self.leases.get(&node) {
            Some(&(current, deadline)) if current == token && deadline > now => Ok(()),
            _ => Err(Error::ClaimLost),
        }
    }

    pub(crate) fn release(&mut self, node: Node) {
        if let Some((_, deadline)) = self.leases.remove(&node) {
            self.expirations.remove(&(deadline, node));
        }
    }

    fn lease(&mut self, node: Node, token: Uuid, now: Instant) {
        self.release(node);
        let deadline = now + LEASE;
        self.leases.insert(node, (token, deadline));
        self.expirations.insert((deadline, node));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_lease_is_fenced_by_token_and_lapses_unless_renewed() {
        let mut claims = Claims::default();
        let node = Node::new(0, 1).unwrap();
        let start = Instant::now();
        let token = claims.grant(node, start);
        assert!(matches!(
            claims.check(node, Uuid::new_v4(), start),
            Err(Error::ClaimLost)
        ));
        let later = start + LEASE / 2;
        claims.renew(node, token, later).unwrap();
        assert_eq!(claims.expire(start + LEASE), []);
        assert_eq!(claims.expire(later + LEASE), [node]);
        assert!(matches!(
            claims.check(node, token, later),
            Err(Error::ClaimLost)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_grant_replaces_the_old_lease_and_its_expiry() {
        let mut claims = Claims::default();
        let node = Node::new(0, 1).unwrap();
        let start = Instant::now();
        let stale = claims.grant(node, start);
        let fresh = claims.grant(node, start + LEASE / 2);
        assert_eq!(claims.expire(start + LEASE), []);
        assert!(matches!(
            claims.check(node, stale, start),
            Err(Error::ClaimLost)
        ));
        claims.check(node, fresh, start + LEASE).unwrap();
    }
}
