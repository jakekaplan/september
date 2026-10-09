use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use september_memory::Budget;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    Error,
    archive::{Message, Receipt},
    jobs::Completion,
    mcp,
    snapshots::{Detail, Range, Snapshot},
    storage::{Archive, Jobs},
};

/// Build the versioned HTTP interface around one shared storage backend,
/// serving clients through [`Archive`] and external workers through [`Jobs`].
///
/// Includes bounded bodies, concurrency, request deadlines, and an explicit
/// durability header. No authentication is implemented: serve on loopback only.
pub fn router<S: Archive + Jobs>(storage: Arc<S>) -> Router {
    let durability = if storage.is_durable() {
        "durable"
    } else {
        "volatile"
    };
    let permits = Arc::new(Semaphore::new(64));
    Router::new()
        .route(
            "/health",
            get(move || async move { Json(json!({ "storage": durability })) }),
        )
        .route("/v1/messages", post(ingest::<S>))
        .route("/v1/snapshots/{id}", put(prepare::<S>).get(snapshot::<S>))
        .route("/v1/snapshots/{id}/zoom", get(zoom::<S>))
        .route("/v1/jobs/claim", post(claim::<S>))
        .route("/v1/jobs/complete", post(complete::<S>))
        .nest_service("/mcp", mcp::service(Arc::clone(&storage)))
        .layer(DefaultBodyLimit::max(512 * 1024))
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let permits = permits.clone();
            async move {
                let mut response = bounded(request, next, permits).await;
                response.headers_mut().insert(
                    "x-september-durability",
                    HeaderValue::from_static(durability),
                );
                response
            }
        }))
        .with_state(storage)
}

async fn bounded(request: Request, next: Next, permits: Arc<Semaphore>) -> Response {
    // The unauthenticated local API is not a browser API. Reject browser-origin
    // requests and unrecognized hosts to prevent cross-origin writes/rebinding.
    let host = request.headers().get("host").and_then(|v| v.to_str().ok());
    let local_host = host.is_some_and(|host| {
        host == "localhost"
            || host
                .strip_prefix("localhost:")
                .is_some_and(|p| p.parse::<u16>().is_ok())
            || host
                .parse::<std::net::SocketAddr>()
                .is_ok_and(|a| a.ip().is_loopback())
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|a| a.is_loopback())
    });
    if request.headers().contains_key("origin") || !local_host {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(_permit) = permits.try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let response = tokio::time::timeout(Duration::from_secs(10), next.run(request))
        .await
        .unwrap_or_else(|_| StatusCode::REQUEST_TIMEOUT.into_response());
    tracing::debug!(status = response.status().as_u16(), "request completed");
    response
}

async fn ingest<S: Archive>(
    State(storage): State<Arc<S>>,
    Json(message): Json<Message>,
) -> Result<(StatusCode, Json<Receipt>), Error> {
    let receipt = storage.ingest(message).await?;
    let status = if receipt.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(receipt)))
}

/// An optional size limit for a harness that takes less context.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preparation {
    within: Option<usize>,
}

async fn prepare<S: Archive>(
    State(storage): State<Arc<S>>,
    Path(id): Path<Uuid>,
    Query(preparation): Query<Preparation>,
) -> Result<Response, Error> {
    let within = preparation
        .within
        .map(Budget::at_most)
        .transpose()
        .map_err(|_| Error::Invalid)?;
    Ok(snapshot_response(storage.prepare(id, within).await?))
}

async fn snapshot<S: Archive>(
    State(storage): State<Arc<S>>,
    Path(id): Path<Uuid>,
) -> Result<Response, Error> {
    Ok(snapshot_response(storage.snapshot(id).await?))
}

fn snapshot_response(snapshot: Snapshot) -> Response {
    let status = if matches!(snapshot, Snapshot::Pending { .. }) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    (status, Json(snapshot)).into_response()
}

async fn zoom<S: Archive>(
    State(storage): State<Arc<S>>,
    Path(id): Path<Uuid>,
    Query(range): Query<Range>,
) -> Result<Json<Detail>, Error> {
    Ok(Json(storage.zoom(id, range.try_into()?).await?))
}

async fn claim<S: Jobs>(State(storage): State<Arc<S>>) -> Result<Response, Error> {
    Ok(match storage.claim().await? {
        Some(claim) => Json(claim).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

async fn complete<S: Jobs>(
    State(storage): State<Arc<S>>,
    Json(completion): Json<Completion>,
) -> Result<StatusCode, Error> {
    storage.complete(completion).await?;
    Ok(StatusCode::NO_CONTENT)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::Invalid => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Conflict => (StatusCode::CONFLICT, "conflict"),
            Self::ClaimLost => (StatusCode::CONFLICT, "claim_lost"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::NotReady => (StatusCode::CONFLICT, "not_ready"),
            Self::OutsideSnapshot => (StatusCode::FORBIDDEN, "outside_snapshot"),
            Self::Capacity => (StatusCode::SERVICE_UNAVAILABLE, "capacity"),
            Self::Internal { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        if let Self::Internal { operation, source } = &self {
            tracing::error!(code, operation, error = %source, "service operation failed");
        }
        (status, Json(json!({ "error": code }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::{io, sync::Mutex};

    use axum::body::to_bytes;

    use super::*;

    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn logs_the_operation_and_cause_but_returns_only_the_public_error() {
        let log = Log(Arc::default());
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let error = Error::internal(
            "freeze pending snapshot",
            september_memory::Error::CutoffMismatch {
                expected: 4,
                actual: 2,
            },
        );
        let response = tracing::subscriber::with_default(subscriber, || error.into_response());
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": "internal"})
        );
        let bytes = log.0.lock().unwrap();
        let recorded = std::str::from_utf8(&bytes).unwrap();
        assert!(recorded.contains("freeze pending snapshot"), "{recorded}");
        assert!(
            recorded.contains("view cutoff is 2, expected 4"),
            "{recorded}"
        );
    }
}
