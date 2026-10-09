//! Fenced summary claims and immutable publication inputs.

use std::time::Duration;

use september_memory::SUMMARY_BYTES;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
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
