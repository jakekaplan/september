//! Finalized source records and ingestion receipts.

use serde::{Deserialize, Serialize};

use crate::{Error, jobs::SUMMARY_BYTES};

/// Stable adapter identity used to deduplicate uploads.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Harness name, such as pi.
    pub harness: String,
    /// Stable session identity, retained across reconnects.
    pub session: String,
    /// Stable transcript entry identity.
    pub entry: String,
    /// Adapter-assigned part number for split messages.
    pub part: u32,
}

/// Allowed finalized content. Reasoning is deliberately not a supported kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A user message.
    User,
    /// A final assistant response.
    Assistant,
    /// A serialized tool invocation.
    ToolCall,
    /// A tool result.
    ToolResult,
}

/// A finalized text message with its original provenance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    /// Stable upload identity for retry detection.
    pub source: Source,
    /// Project identity; does not confer global authority on the content.
    pub project: String,
    /// Harness branch identity, including abandoned branches.
    pub branch: String,
    /// Source timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: i64,
    /// The finalized message category.
    pub kind: Kind,
    /// Tool relationship identity; required for both tool categories.
    pub call_id: Option<String>,
    /// Retained original text, at most 65,536 UTF-8 bytes.
    pub text: String,
}

impl Message {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let labels = [
            &self.source.harness,
            &self.source.session,
            &self.source.entry,
            &self.project,
            &self.branch,
        ];
        if labels.into_iter().any(|v| !valid_label(v))
            || self.text.is_empty()
            || self.text.len() > 65_536
            || self.timestamp_ms < 0
            || self.call_id.as_ref().is_some_and(|v| !valid_label(v))
            || matches!(self.kind, Kind::ToolCall | Kind::ToolResult) != self.call_id.is_some()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    /// The text tagged with its kind, as `user: ...`: how a message reads in memory.
    /// Provenance and timestamps stay in the archive, returned by zoom.
    pub(crate) fn tagged_text(&self) -> String {
        format!("{}: {}", self.kind.as_str(), self.text)
    }

    /// A short message is its own summary, word for word, with no model call.
    pub(crate) fn verbatim_summary(&self) -> Option<String> {
        Some(self.tagged_text()).filter(|text| text.len() <= SUMMARY_BYTES)
    }
}

impl Kind {
    /// The wire name, also used to tag each item in memory.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }
}

fn valid_label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

/// An ingestion acknowledgment. A duplicate returns its original archive ID.
#[derive(Debug, Deserialize, Serialize)]
pub struct Receipt {
    /// Globally ordered ID within this archive instance.
    pub id: u64,
    /// Whether this request reused an identical source record.
    pub duplicate: bool,
}
