use std::{error, fmt};

/// Service failures. Internal sources are retained for diagnostics, not HTTP output.
#[derive(Debug)]
pub enum Error {
    /// The request violates a size, identity, or range constraint.
    Invalid,
    /// A source identity or claim conflicts with existing state.
    Conflict,
    /// The requested snapshot does not exist in this archive instance.
    NotFound,
    /// Required summaries have not completed.
    NotReady,
    /// The requested range is not within the frozen cover.
    OutsideSnapshot,
    /// The bounded backend is full.
    Capacity,
    /// An internal invariant or storage operation failed.
    Internal {
        /// Operation that failed, for structured server diagnostics.
        operation: &'static str,
        /// Original typed cause. Must exclude conversation bodies and credentials.
        source: Box<dyn error::Error + Send + Sync>,
    },
}

impl Error {
    pub(crate) fn internal(
        operation: &'static str,
        source: impl error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Internal {
            operation,
            source: Box::new(source),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid request",
            Self::Conflict => "identity or claim conflict",
            Self::NotFound => "snapshot not found",
            Self::NotReady => "snapshot is waiting for summaries",
            Self::OutsideSnapshot => "range is outside the frozen cover",
            Self::Capacity => "storage capacity reached",
            Self::Internal { .. } => "internal service failure",
        })
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Internal { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<september_memory::Error> for Error {
    fn from(error: september_memory::Error) -> Self {
        match error {
            september_memory::Error::InvalidNode { .. } => Self::Invalid,
            september_memory::Error::OutsideSnapshot(_) => Self::OutsideSnapshot,
            source => Self::internal("memory operation", source),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Invariant {
    MissingMessage(u64),
    MissingSummary(september_memory::Node),
}

impl fmt::Display for Invariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMessage(id) => write!(f, "missing archived message {id}"),
            Self::MissingSummary(node) => write!(f, "missing completed summary {node}"),
        }
    }
}

impl error::Error for Invariant {}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn internal_failures_preserve_the_typed_source_and_operation() {
        let cause = september_memory::Error::CutoffMismatch {
            expected: 4,
            actual: 2,
        };
        let error = Error::internal("freeze pending snapshot", cause.clone());
        assert!(matches!(
            &error,
            Error::Internal {
                operation: "freeze pending snapshot",
                ..
            }
        ));
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<september_memory::Error>()),
            Some(&cause),
        );
        assert_eq!(error.to_string(), "internal service failure");
    }

    #[test]
    fn core_errors_keep_their_cause_when_converted() {
        let error = Error::from(september_memory::Error::InvalidView);
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<september_memory::Error>()),
            Some(&september_memory::Error::InvalidView),
        );
        assert!(matches!(
            Error::from(september_memory::Error::InvalidNode {
                start: 0,
                length: 3
            }),
            Error::Invalid
        ));
    }
}
