use crate::archive::Kind;

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

async fn with_job() -> Result<InMemory, Error> {
    let storage = InMemory::default();
    storage.ingest(message(0, "x".repeat(600))).await?;
    Ok(storage)
}

fn node(start: u64, length: u64) -> Node {
    Node::new(start, length).unwrap()
}

async fn summary(storage: &InMemory, node: Node) -> Option<String> {
    let state = storage.state.lock().await;
    state
        .summaries
        .get(&node)
        .map(|completed| completed.text.to_string())
}

#[tokio::test(start_paused = true)]
async fn renewal_moves_expiry_and_never_revives_a_replaced_claim() {
    let storage = with_job().await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let node = Node::try_from(claim.job.range).unwrap();
    tokio::time::advance(Duration::from_secs(40)).await;
    storage.renew(node, claim.token).await.unwrap();
    tokio::time::advance(Duration::from_secs(21)).await;
    assert!(storage.claim().await.unwrap().is_none());
    tokio::time::advance(Duration::from_secs(39)).await;
    assert!(matches!(
        storage.renew(node, claim.token).await,
        Err(Error::Conflict)
    ));
    let replacement = storage.claim().await.unwrap().unwrap();
    assert_ne!(replacement.token, claim.token);
    assert!(matches!(
        storage.renew(node, claim.token).await,
        Err(Error::Conflict)
    ));
    storage
        .complete(Completion {
            range: replacement.job.range,
            token: replacement.token,
            text: "summary".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        storage.renew(node, replacement.token).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn short_messages_and_pairs_publish_verbatim_without_jobs() {
    let storage = InMemory::default();
    for (entry, text) in (0..).zip(["first", "second", "third", "fourth"]) {
        storage.ingest(message(entry, text.into())).await.unwrap();
    }
    assert!(storage.claim().await.unwrap().is_none());
    assert_eq!(summary(&storage, node(0, 1)).await.unwrap(), "user: first");
    assert_eq!(
        summary(&storage, node(0, 2)).await.unwrap(),
        "user: first\nuser: second"
    );
    assert_eq!(
        summary(&storage, node(0, 4)).await.unwrap(),
        "user: first\nuser: second\nuser: third\nuser: fourth"
    );
}

#[tokio::test]
async fn the_first_parent_too_long_to_join_becomes_the_only_job() {
    let storage = InMemory::default();
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
    assert_eq!(summary(&storage, node(0, 2)).await, None);
}

#[tokio::test]
async fn completions_may_exceed_the_target_but_not_the_ceiling() {
    let storage = with_job().await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let completion = |text: String| Completion {
        range: claim.job.range,
        token: claim.token,
        text,
    };
    assert!(matches!(
        storage
            .complete(completion("z".repeat(MAX_SUMMARY_BYTES + 1)))
            .await,
        Err(Error::Invalid)
    ));
    let oversized = "z".repeat(SUMMARY_BYTES + 100);
    storage
        .complete(completion(oversized.clone()))
        .await
        .unwrap();
    assert_eq!(summary(&storage, node(0, 1)).await, Some(oversized));
}
