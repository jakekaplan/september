//! Frozen interaction views and authorized retrieval results.

use std::sync::Arc;

use september_memory::{Budget, Node, Snapshot as Frozen, View};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, archive::Message};

/// A wire-format range, validated when converted to a [`Node`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    /// First archive message ID.
    pub start: u64,
    /// Aligned power-of-two message count.
    pub length: u64,
}

impl TryFrom<Range> for Node {
    type Error = Error;

    fn try_from(value: Range) -> Result<Self, Self::Error> {
        Ok(Self::new(value.start, value.length)?)
    }
}

impl From<Node> for Range {
    fn from(node: Node) -> Self {
        Self {
            start: node.start(),
            length: node.length(),
        }
    }
}

/// Immutable summary content returned by zoom or supplied to a summary worker.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Summary {
    /// The covered archive range.
    pub range: Range,
    /// Completed text.
    pub text: String,
}

/// A snapshot remains pending until its exact frozen prefix has summaries.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Snapshot {
    /// No view or partial fallback is exposed yet.
    Pending {
        /// Opaque identity used for polling and zoom.
        id: Uuid,
        /// Exclusive cutoff, fixed at creation.
        cutoff: u64,
    },
    /// An immutable interaction view.
    Ready {
        /// Opaque identity used for polling and zoom.
        id: Uuid,
        /// Exclusive cutoff, fixed at creation.
        cutoff: u64,
        /// Frozen node cover for navigation.
        nodes: Vec<Range>,
        /// Exact model-visible memory text.
        view: String,
    },
}

impl Snapshot {
    /// An interaction saved at `cutoff`: pending until its cover is frozen.
    pub(crate) fn new(id: Uuid, cutoff: u64, frozen: Option<&Frozen>) -> Self {
        match frozen {
            None => Self::Pending { id, cutoff },
            Some(frozen) => Self::Ready {
                id,
                cutoff,
                nodes: frozen.nodes().iter().copied().map(Into::into).collect(),
                view: frozen.render().to_owned(),
            },
        }
    }
}

/// Freeze `view` for an interaction saved at `cutoff`, merged further through
/// built parents when it asked for a view `within` a smaller size.
pub(crate) fn freeze(
    view: &View,
    cutoff: u64,
    within: Option<Budget>,
    built: impl Fn(Node) -> Option<Arc<str>>,
) -> Result<Frozen, Error> {
    match within {
        None => view.freeze(cutoff),
        Some(budget) => view.freeze_within(cutoff, budget, built),
    }
    .map_err(|error| Error::internal("freeze snapshot", error))
}

/// A snapshot-authorized retrieval result.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Detail {
    /// Open a parent into its two immutable child summaries.
    Children {
        /// Left and right child, in chronological order.
        summaries: [Summary; 2],
    },
    /// Open a leaf into its archived original and provenance.
    Message {
        /// Original archive message ID.
        id: u64,
        /// Original message, including source timestamp and tool identity.
        message: Message,
    },
}
