use tempfile::TempDir;

use september_memory::SUMMARY_BYTES;

use crate::{
    archive::{Kind, Source},
    jobs::{Claim, Completion, Context, Input, MAX_SUMMARY_BYTES},
    storage::Jobs,
};

use super::*;

fn message(entry: u64, text: String) -> Message {
    Message {
        source: Source {
            harness: "test".into(),
            session: "test".into(),
            entry: entry.to_string(),
            part: 0,
        },
        project: "test".into(),
        branch: "test".into(),
        timestamp_ms: 0,
        kind: Kind::User,
        call_id: None,
        text,
    }
}

/// A fresh archive file in a nested folder that `open` has to create.
fn folder() -> (TempDir, std::path::PathBuf) {
    let folder = TempDir::new().unwrap();
    let path = folder.path().join("data/september.sqlite3");
    (folder, path)
}

fn within(bytes: usize) -> Budget {
    Budget::at_most(bytes).unwrap()
}

fn open(path: &Path) -> Sqlite {
    Sqlite::open(path, Budget::CHAT).unwrap()
}

async fn finish(storage: &Sqlite, claim: &Claim, text: &str) -> Result<(), Error> {
    storage
        .complete(Completion {
            range: claim.job.range,
            token: claim.token,
            text: text.into(),
        })
        .await
}

fn node(start: u64, length: u64) -> Node {
    Node::new(start, length).unwrap()
}

async fn text(storage: &Sqlite, node: Node) -> Option<String> {
    let state = storage.state.lock().await;
    summary_text(&state.db, node)
        .unwrap()
        .map(|text| text.to_string())
}

fn view(snapshot: &Snapshot) -> &str {
    match snapshot {
        Snapshot::Ready { view, .. } => view,
        Snapshot::Pending { .. } => panic!("expected a ready snapshot"),
    }
}

#[tokio::test]
async fn retried_messages_keep_their_id_and_changed_ones_conflict() {
    let (_folder, path) = folder();
    let storage = open(&path);
    let first = storage.ingest(message(0, "alpha".into())).await.unwrap();
    let again = storage.ingest(message(0, "alpha".into())).await.unwrap();
    assert_eq!((first.id, first.duplicate), (0, false));
    assert_eq!((again.id, again.duplicate), (0, true));
    assert!(matches!(
        storage.ingest(message(0, "changed".into())).await,
        Err(Error::Conflict)
    ));
    let next = storage.ingest(message(1, "beta".into())).await.unwrap();
    assert_eq!((next.id, next.duplicate), (1, false));
}

#[tokio::test]
async fn a_waiting_snapshot_becomes_ready_when_its_summaries_complete() {
    let (_folder, path) = folder();
    let storage = open(&path);
    storage.ingest(message(0, "x".repeat(600))).await.unwrap();
    let id = Uuid::new_v4();
    assert!(matches!(
        storage.prepare(id, None).await.unwrap(),
        Snapshot::Pending { cutoff: 1, .. }
    ));
    assert!(matches!(
        storage.zoom(id, Node::new(0, 1).unwrap()).await,
        Err(Error::NotReady)
    ));
    let claim = storage.claim().await.unwrap().unwrap();
    finish(&storage, &claim, "a long message").await.unwrap();
    let ready = storage.snapshot(id).await.unwrap();
    assert_eq!(view(&ready), "<chat>\n0+1|a long message\n</chat>");
    assert!(matches!(
        storage.zoom(id, Node::new(0, 1).unwrap()).await.unwrap(),
        Detail::Message { id: 0, .. }
    ));
}

#[tokio::test]
async fn a_sized_snapshot_merges_its_own_copy_through_built_parents() {
    let (_folder, path) = folder();
    let storage = open(&path);
    for entry in 0..8 {
        let note = message(entry, format!("note {entry}"));
        storage.ingest(note).await.unwrap();
    }
    let small = storage
        .prepare(Uuid::new_v4(), Some(within(80)))
        .await
        .unwrap();
    assert!(view(&small).starts_with("<chat>\n0+8|user: note 0 user: note 1"));
    let full = storage.prepare(Uuid::new_v4(), None).await.unwrap();
    assert_eq!(view(&full).lines().count(), 10);
}

#[tokio::test]
async fn only_the_live_claim_publishes_and_an_identical_retry_succeeds() {
    let (_folder, path) = folder();
    let storage = open(&path);
    storage.ingest(message(0, "x".repeat(600))).await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    assert!(storage.claim().await.unwrap().is_none());
    let stranger = Completion {
        range: claim.job.range,
        token: Uuid::new_v4(),
        text: "stolen".into(),
    };
    assert!(matches!(
        storage.complete(stranger).await,
        Err(Error::ClaimLost)
    ));
    let node = Node::try_from(claim.job.range).unwrap();
    storage.renew(node, claim.token).await.unwrap();
    finish(&storage, &claim, "summary").await.unwrap();
    finish(&storage, &claim, "summary").await.unwrap();
    assert!(matches!(
        finish(&storage, &claim, "another").await,
        Err(Error::ClaimLost)
    ));
    assert!(matches!(
        storage.renew(node, claim.token).await,
        Err(Error::ClaimLost)
    ));
}

#[tokio::test]
async fn a_restart_keeps_the_archive_views_snapshots_and_job_contexts() {
    let (_folder, path) = folder();
    let storage = open(&path);
    storage.ingest(message(0, "alpha".into())).await.unwrap();
    storage.ingest(message(1, "x".repeat(600))).await.unwrap();
    storage.ingest(message(2, "omega".into())).await.unwrap();
    let waiting = Uuid::new_v4();
    storage.prepare(waiting, Some(within(1_000))).await.unwrap();
    let before = storage.claim().await.unwrap().unwrap();
    let context = Context {
        cutoff: 1,
        view: "<chat>\n0+1|user: alpha\n</chat>".into(),
    };
    assert_eq!(before.job.context, context);
    drop(storage);

    let storage = open(&path);
    assert!(matches!(
        storage.snapshot(waiting).await.unwrap(),
        Snapshot::Pending { cutoff: 3, .. }
    ));
    // The claim died with the process; the job and its frozen context did not.
    let after = storage.claim().await.unwrap().unwrap();
    assert_eq!(
        (after.job.range, &after.job.context),
        (before.job.range, &context)
    );
    assert!(matches!(
        finish(&storage, &before, "late").await,
        Err(Error::ClaimLost)
    ));
    finish(&storage, &after, "a long message").await.unwrap();
    let expected = "<chat>\n0+1|user: alpha\n1+1|a long message\n2+1|user: omega\n</chat>";
    assert_eq!(view(&storage.snapshot(waiting).await.unwrap()), expected);
    let current = Uuid::new_v4();
    assert_eq!(
        view(&storage.prepare(current, None).await.unwrap()),
        expected
    );
    drop(storage);

    let storage = open(&path);
    assert_eq!(view(&storage.snapshot(current).await.unwrap()), expected);
    assert_eq!(
        view(&storage.prepare(Uuid::new_v4(), None).await.unwrap()),
        expected
    );
    let again = storage.ingest(message(0, "alpha".into())).await.unwrap();
    assert_eq!((again.id, again.duplicate), (0, true));
    assert_eq!(
        storage.ingest(message(3, "next".into())).await.unwrap().id,
        3
    );
    assert!(matches!(
        storage
            .zoom(current, Node::new(1, 1).unwrap())
            .await
            .unwrap(),
        Detail::Message { id: 1, .. }
    ));
}

#[tokio::test]
async fn a_second_server_cannot_open_a_held_archive() {
    let (_folder, path) = folder();
    let storage = open(&path);
    let Err(OpenError::Database(cause)) = Sqlite::open(&path, Budget::CHAT) else {
        panic!("the archive is held");
    };
    assert_eq!(
        cause.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    drop(storage);
    Sqlite::open(&path, Budget::CHAT).unwrap();
}

#[tokio::test]
async fn short_messages_and_pairs_publish_verbatim_without_jobs() {
    let storage = Sqlite::in_memory(Budget::CHAT).unwrap();
    for (entry, note) in (0..).zip(["first", "second", "third", "fourth"]) {
        storage.ingest(message(entry, note.into())).await.unwrap();
    }
    assert!(storage.claim().await.unwrap().is_none());
    assert_eq!(text(&storage, node(0, 1)).await.unwrap(), "user: first");
    assert_eq!(
        text(&storage, node(0, 4)).await.unwrap(),
        "user: first\nuser: second\nuser: third\nuser: fourth"
    );
}

#[tokio::test]
async fn the_first_parent_too_long_to_join_becomes_the_only_job() {
    let storage = Sqlite::in_memory(Budget::CHAT).unwrap();
    for entry in 0..2 {
        storage
            .ingest(message(entry, "y".repeat(300)))
            .await
            .unwrap();
    }
    let claim = storage.claim().await.unwrap().unwrap();
    assert_eq!(Node::try_from(claim.job.range).unwrap(), node(0, 2));
    assert!(matches!(claim.job.input, Input::Children { .. }));
    assert!(storage.claim().await.unwrap().is_none());
    assert_eq!(text(&storage, node(0, 2)).await, None);
}

#[tokio::test]
async fn completions_may_exceed_the_target_but_not_the_ceiling() {
    let storage = Sqlite::in_memory(Budget::CHAT).unwrap();
    storage.ingest(message(0, "x".repeat(600))).await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let ceiling = "z".repeat(MAX_SUMMARY_BYTES + 1);
    assert!(matches!(
        finish(&storage, &claim, &ceiling).await,
        Err(Error::Invalid)
    ));
    let oversized = "z".repeat(SUMMARY_BYTES + 100);
    finish(&storage, &claim, &oversized).await.unwrap();
    assert_eq!(text(&storage, node(0, 1)).await, Some(oversized));
}

#[tokio::test]
async fn a_waiting_sized_snapshot_is_frozen_at_its_own_size() {
    let storage = Sqlite::in_memory(Budget::CHAT).unwrap();
    for entry in 0..8 {
        let note = message(entry, format!("note {entry}"));
        storage.ingest(note).await.unwrap();
    }
    storage.ingest(message(8, "y".repeat(600))).await.unwrap();
    let id = Uuid::new_v4();
    assert!(matches!(
        storage.prepare(id, Some(within(60))).await.unwrap(),
        Snapshot::Pending { cutoff: 9, .. }
    ));
    let claim = storage.claim().await.unwrap().unwrap();
    finish(&storage, &claim, "short").await.unwrap();
    let Snapshot::Ready { nodes, .. } = storage.snapshot(id).await.unwrap() else {
        panic!("the waiting snapshot is ready");
    };
    let ranges: Vec<_> = nodes
        .iter()
        .map(|range| (range.start, range.length))
        .collect();
    assert_eq!(ranges, [(0, 8), (8, 1)]);
}
