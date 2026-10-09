//! In-memory Docket experiment. Only synthetic messages and fake summaries.

use std::{error::Error, process::ExitCode, sync::Arc, time::Duration};

use september::{
    archive::{Kind, Message, Source},
    snapshots::Snapshot,
    storage::{InMemory, Storage},
    worker::{Worker, fake},
};
use september_memory::Budget;
use tokio::{
    sync::oneshot,
    time::{sleep, timeout},
};
use uuid::Uuid;

pub(crate) fn message(entry: usize) -> Message {
    Message {
        source: Source {
            harness: "docket-experiment".into(),
            session: "synthetic".into(),
            entry: entry.to_string(),
            part: 0,
        },
        project: "synthetic".into(),
        branch: "experiment".into(),
        timestamp_ms: 0,
        kind: Kind::User,
        call_id: None,
        text: format!("Synthetic message {entry}. ").repeat(40),
    }
}

#[tokio::main]
pub(crate) async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_test_writer()
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "Docket experiment failed");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let storage = Arc::new(InMemory::new(Budget::new(80, 160)?));
    for entry in 0..8 {
        storage.ingest(message(entry)).await?;
    }
    let id = Uuid::new_v4();
    if !matches!(
        storage.prepare(id).await?,
        Snapshot::Pending { cutoff: 8, .. }
    ) {
        return Err("synthetic snapshot should begin pending".into());
    }
    let worker = Worker::memory(Arc::clone(&storage), fake).await?;
    let (stop, stopped) = oneshot::channel();
    let running = tokio::spawn(worker.run(async {
        let _ = stopped.await;
    }));
    let completed = timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = storage.prepare(Uuid::new_v4()).await?;
            if let Snapshot::Ready { nodes, .. } = snapshot
                && nodes.len() == 1
                && nodes[0].length == 8
            {
                return Ok::<(), september::Error>(());
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await;
    let _ = stop.send(());
    running.await??;
    completed??;
    if !matches!(
        storage.snapshot(id).await?,
        Snapshot::Ready { cutoff: 8, .. }
    ) {
        return Err("synthetic snapshot did not become ready".into());
    }
    tracing::info!(
        "continuous worker passed: eight synthetic messages and all parents summarized; snapshot ready"
    );
    Ok(())
}
