//! Model-facing retrieval over MCP: the gist's `zoom` and `date` tools.
//!
//! Each call names the interaction's snapshot. Harness adapters fill it in, so
//! the model never handles it and the view text stays unchanged.

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use schemars::JsonSchema;
use september_memory::Node;
use serde::Deserialize;
use uuid::Uuid;

use crate::{Error, snapshots::Detail, storage::Archive};

const INSTRUCTIONS: &str = "Memory of the user's whole chat, as one-line summaries \
`id+n|text` in <chat> tags. zoom opens a line into the two lines it was made from, \
down to the original message; date gives a message's date and time.";

/// A stateless MCP service answering retrieval from frozen snapshots.
pub(crate) fn service<S: Archive>(
    storage: Arc<S>,
) -> StreamableHttpService<Retrieval<S>, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None);
    StreamableHttpService::new(
        move || {
            Ok(Retrieval {
                storage: Arc::clone(&storage),
            })
        },
        Arc::default(),
        config,
    )
}

/// The `zoom` and `date` tools over one shared archive.
pub(crate) struct Retrieval<S> {
    storage: Arc<S>,
}

#[derive(Deserialize, JsonSchema)]
struct ZoomRequest {
    /// Filled in by the harness; leave it out.
    snapshot: Option<Uuid>,
    /// The line's first message ID: the `id` in `id+n`.
    start: u64,
    /// The line's message count: the `n` in `id+n`.
    length: u64,
}

#[derive(Deserialize, JsonSchema)]
struct DateRequest {
    /// Filled in by the harness; leave it out.
    snapshot: Option<Uuid>,
    /// The message ID.
    id: u64,
}

#[tool_router]
impl<S: Archive> Retrieval<S> {
    /// Open a line of the frozen view into its two halves, or its message.
    #[tool(
        description = "Open the line id+n of the memory view into the two lines of n/2 under \
                       it; n = 1 gives message id whole. n is a power of 2, and id a multiple of n."
    )]
    async fn zoom(
        &self,
        Parameters(request): Parameters<ZoomRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let detail = match self
            .detail(request.snapshot, request.start, request.length)
            .await
        {
            Ok(detail) => detail,
            Err(error) => return Ok(failure(&error)),
        };
        let text = match detail {
            Detail::Children { summaries } => {
                let lines: Result<Vec<_>, Error> = summaries
                    .iter()
                    .map(|child| {
                        let node = Node::try_from(child.range)?;
                        Ok(september_memory::Summary::new(node, child.text.as_str()).to_string())
                    })
                    .collect();
                match lines {
                    Ok(lines) => lines.join("\n"),
                    Err(error) => return Ok(failure(&error)),
                }
            }
            Detail::Message { message, .. } => format!(
                "[{} · session {} · {}@{}]\n{}",
                message.source.harness,
                message.source.session,
                message.project,
                message.branch,
                message.tagged_text(),
            ),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// The source timestamp of one message in the frozen view.
    #[tool(description = "The date and time of message id, in UTC.")]
    async fn date(
        &self,
        Parameters(request): Parameters<DateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let date = match self.detail(request.snapshot, request.id, 1).await {
            Ok(Detail::Message { message, .. }) => {
                DateTime::from_timestamp_millis(message.timestamp_ms)
                    .map(|date| date.to_rfc3339_opts(SecondsFormat::Secs, true))
            }
            Ok(Detail::Children { .. }) => None,
            Err(error) => return Ok(failure(&error)),
        };
        Ok(match date {
            Some(date) => CallToolResult::success(vec![ContentBlock::text(date)]),
            None => CallToolResult::error(vec![ContentBlock::text("message has no valid date")]),
        })
    }

    async fn detail(
        &self,
        snapshot: Option<Uuid>,
        start: u64,
        length: u64,
    ) -> Result<Detail, Error> {
        let snapshot = snapshot.ok_or(Error::NotFound)?;
        self.storage.zoom(snapshot, Node::new(start, length)?).await
    }
}

#[tool_handler]
#[expect(
    clippy::unused_async_trait_impl,
    reason = "rmcp's tool_handler macro generates async trait methods that do not await"
)]
impl<S: Archive> ServerHandler for Retrieval<S> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
    }
}

/// Report a failed retrieval to the model; log internal causes for the server only.
fn failure(error: &Error) -> CallToolResult {
    if let Error::Internal { operation, source } = error {
        tracing::error!(operation, error = %source, "retrieval failed");
    }
    let reason = match error {
        Error::Invalid => "not a line: n must be a power of 2 and id a multiple of n",
        Error::NotFound => "no snapshot for this interaction; the harness adapter supplies it",
        Error::NotReady => "memory is still summarizing; try again shortly",
        Error::OutsideSnapshot => "that line is not in your memory view",
        _ => "memory retrieval failed",
    };
    CallToolResult::error(vec![ContentBlock::text(reason)])
}
