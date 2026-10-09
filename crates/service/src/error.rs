use std::error;

/// Service failures. Internal sources are retained for diagnostics, not HTTP output.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request violates a size, identity, or range constraint.
    #[error("invalid request")]
    Invalid,
    /// A source identity was reused with different content.
    #[error("source identity conflict")]
    Conflict,
    /// A summary claim expired or passed to another worker.
    #[error("summary claim lost")]
    ClaimLost,
    /// The requested snapshot does not exist in this archive instance.
    #[error("snapshot not found")]
    NotFound,
    /// Required summaries have not completed.
    #[error("snapshot is waiting for summaries")]
    NotReady,
    /// The requested range is not within the frozen cover.
    #[error("range is outside the frozen cover")]
    OutsideSnapshot,
    /// The bounded backend is full.
    #[error("storage capacity reached")]
    Capacity,
    /// An internal invariant or storage operation failed.
    #[error("internal service failure")]
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

impl From<september_memory::Error> for Error {
    fn from(error: september_memory::Error) -> Self {
        match error {
            september_memory::Error::InvalidNode { .. } => Self::Invalid,
            september_memory::Error::OutsideSnapshot(_) => Self::OutsideSnapshot,
            source => Self::internal("memory operation", source),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Invariant {
    #[error("missing archived message {0}")]
    MissingMessage(u64),
    #[error("missing completed summary {0}")]
    MissingSummary(september_memory::Node),
    #[error("unknown database schema version {0}")]
    UnknownSchema(i64),
}

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
