use std::{future::Future, io, time::Duration};

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::watch,
    task::{JoinError, JoinSet},
    time::timeout,
};

const MAX_CONNECTIONS: usize = 64;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Serve HTTP/1 with bounded connections, header reads, and shutdown draining.
///
/// Stops accepting when `shutdown` resolves, allows active connections ten seconds
/// to drain, then cancels and joins the remainder. The caller owns bind policy.
///
/// # Errors
/// Returns listener errors after draining already accepted connections.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    let (stop, stopped) = watch::channel(());
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break Ok(()),
            Some(result) = connections.join_next() => report_exit(result),
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                let app = app.clone();
                let stopped = stopped.clone();
                connections.spawn(async move {
                    if let Err(error) = connection(stream, app, stopped).await {
                        tracing::debug!(%error, "HTTP connection ended");
                    }
                });
            }
        }
    };
    drop(listener);
    drop(stop);
    drain(&mut connections).await;
    result
}

async fn connection(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    app: Router,
    mut stopped: watch::Receiver<()>,
) -> Result<(), hyper::Error> {
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT);
    let connection = builder.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app));
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => result,
        _ = stopped.changed() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    }
}

async fn drain(connections: &mut JoinSet<()>) {
    if timeout(SHUTDOWN_GRACE, async {
        while let Some(result) = connections.join_next().await {
            report_exit(result);
        }
    })
    .await
    .is_err()
    {
        tracing::warn!(
            remaining = connections.len(),
            "shutdown deadline reached; closing connections"
        );
        connections.shutdown().await;
    }
}

fn report_exit(result: Result<(), JoinError>) {
    if let Err(error) = result {
        tracing::error!(%error, "connection task failed");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::routing::get;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, duplex},
        sync::{Notify, oneshot},
        time::{advance, sleep},
    };

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unfinished_headers_expire_before_reaching_the_handler() {
        let (mut client, server) = duplex(1024);
        let (_stop, stopped) = watch::channel(());
        let task = tokio::spawn(connection(server, Router::new(), stopped));
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        advance(HEADER_TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
        assert!(!task.is_finished());
        advance(Duration::from_secs(1)).await;
        assert!(task.await.unwrap().unwrap_err().is_timeout());
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_shutdown_finishes_an_active_response() {
        let (mut client, server) = duplex(1024);
        let (stop, stopped) = watch::channel(());
        let started = Arc::new(Notify::new());
        let entered = started.clone();
        let app = Router::new().route(
            "/",
            get(move || {
                let entered = entered.clone();
                async move {
                    entered.notify_one();
                    sleep(Duration::from_secs(1)).await;
                    "finished"
                }
            }),
        );
        let task = tokio::spawn(connection(server, app, stopped));
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        started.notified().await;
        drop(stop);
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.ends_with("finished"));
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn drain_cancels_and_joins_tasks_at_the_deadline() {
        let (held, dropped) = oneshot::channel::<()>();
        let mut connections = JoinSet::new();
        connections.spawn(async move {
            std::future::pending::<()>().await;
            drop(held);
        });
        let start = tokio::time::Instant::now();
        drain(&mut connections).await;
        assert_eq!(start.elapsed(), SHUTDOWN_GRACE);
        assert!(connections.is_empty());
        assert!(dropped.await.is_err());
    }
}
