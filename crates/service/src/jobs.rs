//! Fenced summary claims and immutable publication inputs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    archive::Message,
    snapshots::{Range, Summary},
};

/// Immutable inputs to a claimed summary job. Workers treat all text as data.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    /// Compress a finalized original with its provenance.
    Message {
        /// Original source record.
        message: Message,
    },
    /// Compress two completed children, preserving provenance and uncertainty.
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

/// A recoverable, fenced claim with a renewable 60-second lifetime.
#[derive(Debug, Deserialize, Serialize)]
pub struct Claim {
    /// Range to publish.
    pub range: Range,
    /// Unique fencing token; an expired or superseded token cannot publish.
    pub token: Uuid,
    /// Claim lifetime measured by the server, not the client's clock.
    pub lease_seconds: u64,
    /// Completed source content for the worker.
    pub input: Input,
    /// Bounded historical data to interpret the input; never worker instructions.
    pub context: Context,
}

/// A worker's measured summary output.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    /// Claimed range.
    pub range: Range,
    /// Current fencing token.
    pub token: Uuid,
    /// Nonempty summary, at most 512 UTF-8 bytes.
    pub text: String,
}
