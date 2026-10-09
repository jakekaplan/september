//! Local September server. In-memory mode intentionally loses data at shutdown.

mod settings;

use std::{error::Error, process::ExitCode, sync::Arc};

use axum::{
    extract::Request,
    http::HeaderValue,
    middleware::{self, Next},
};
use september::{
    router, serve,
    storage::InMemory,
    summarizer::Summarizer,
    worker::{self, Worker},
};
use tokio::{net::TcpListener, signal, sync::watch, task::JoinSet};
use tracing::Level;
use tracing_subscriber::{filter::Targets, fmt, prelude::*};

#[tokio::main]
async fn main() -> ExitCode {
    // Docket logs every task run, including the dispatcher's polls; keep its failures.
    let levels = Targets::new()
        .with_default(Level::INFO)
        .with_target("docket", Level::WARN);
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .with(levels)
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "server stopped with an error");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let settings = settings::Settings::load()?;
    let storage = Arc::new(InMemory::default());
    let worker = match settings.summarizer {
        settings::Summarizer::None => None,
        settings::Summarizer::Fake => {
            Some(Worker::memory(Arc::clone(&storage), worker::fake).await?)
        }
        settings::Summarizer::Model(provider) => {
            let api_key = settings::api_key(provider)?;
            let model = settings.model.ok_or("model setting missing")?;
            let summarizer = Summarizer::new(provider, model, api_key)?;
            Some(
                Worker::memory(Arc::clone(&storage), move |job| {
                    let summarizer = summarizer.clone();
                    async move { summarizer.summarize(&job).await }
                })
                .await?,
            )
        }
    };
    let mut app = router(storage);
    if settings.summarizer == settings::Summarizer::Fake {
        tracing::warn!("fake summarizer enabled; use synthetic messages only");
        app = app.layer(middleware::from_fn(
            |request: Request, next: Next| async move {
                let mut response = next.run(request).await;
                response
                    .headers_mut()
                    .insert("x-september-summarizer", HeaderValue::from_static("fake"));
                response
            },
        ));
    }
    let listener = TcpListener::bind(settings.bind).await?;
    let address = listener.local_addr()?;
    let shutdown = shutdown_signal()?;
    tracing::info!(%address, storage = "memory", durability = "volatile", "September listening; restart clears all data");
    let (stop, mut stopped) = watch::channel(());
    let mut tasks: JoinSet<Result<(), Box<dyn Error + Send + Sync>>> = JoinSet::new();
    tasks.spawn(async move {
        serve(listener, app, async move {
            let _ = stopped.changed().await;
        })
        .await?;
        Ok(())
    });
    if let Some(worker) = worker {
        let mut stopped = stop.subscribe();
        tasks.spawn(async move {
            worker
                .run(async move {
                    let _ = stopped.changed().await;
                })
                .await?;
            Ok(())
        });
    }
    let mut outcome = tokio::select! {
        () = shutdown => Ok(()),
        Some(result) = tasks.join_next() => result.unwrap_or_else(|error| Err(error.into())),
    };
    drop(stop);
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result.unwrap_or_else(|error| Err(error.into())) {
            outcome = Err(error);
        }
    }
    outcome
}

fn shutdown_signal() -> Result<impl Future<Output = ()>, std::io::Error> {
    #[cfg(unix)]
    let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    Ok(async move {
        #[cfg(unix)]
        tokio::select! {
            result = signal::ctrl_c() => {
                if let Err(error) = result { tracing::error!(%error, "shutdown signal failed"); }
            }
            _ = terminate.recv() => {}
        }
        #[cfg(not(unix))]
        if let Err(error) = signal::ctrl_c().await {
            tracing::error!(%error, "shutdown signal failed");
        }
        tracing::info!("shutting down");
    })
}
