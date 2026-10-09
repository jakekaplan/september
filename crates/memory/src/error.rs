use std::error;
use std::fmt;

use crate::Node;

/// A violation of the memory tree, view, or snapshot contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// A range is empty, unaligned, not a power of two, or extends beyond `u64`.
    InvalidNode {
        /// The requested first message ID.
        start: u64,
        /// The requested number of messages.
        length: u64,
    },
    /// A parent range or rendered byte count cannot be represented.
    Overflow,
    /// The byte limits cannot represent the empty view or lack hysteresis.
    InvalidBudget {
        /// The requested batch target, in UTF-8 bytes.
        target: usize,
        /// The requested batch trigger, in UTF-8 bytes.
        trigger: usize,
    },
    /// Only a one-message node can be appended to a view.
    InvalidLeaf(Node),
    /// A cover has gaps, overlaps, or an inconsistent shrinking state.
    InvalidView,
    /// The view does not end at the requested interaction boundary.
    CutoffMismatch {
        /// The exclusive cutoff requested by the caller.
        expected: u64,
        /// The exclusive cutoff covered by the view.
        actual: u64,
    },
    /// A retrieval range is neither a frozen cover node nor its descendant.
    OutsideSnapshot(Node),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNode { start, length } => {
                write!(formatter, "invalid node range {start}+{length}")
            }
            Self::Overflow => formatter.write_str("memory range or rendered byte count overflowed"),
            Self::InvalidBudget { target, trigger } => {
                write!(
                    formatter,
                    "invalid byte budget: target {target}, trigger {trigger}"
                )
            }
            Self::InvalidLeaf(node) => write!(formatter, "cannot append non-leaf {node}"),
            Self::InvalidView => formatter.write_str("invalid view cover or shrinking state"),
            Self::CutoffMismatch { expected, actual } => {
                write!(formatter, "view cutoff is {actual}, expected {expected}")
            }
            Self::OutsideSnapshot(node) => {
                write!(formatter, "range {node} is outside the snapshot")
            }
        }
    }
}

impl error::Error for Error {}
