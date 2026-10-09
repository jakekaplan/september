use crate::archive::Kind;

use super::*;

async fn with_job() -> Result<InMemory, Error> {
    let storage = InMemory::default();
    storage
        .ingest(Message {
            source: Source {
                harness: "test".into(),
                session: "test".into(),
                entry: "0".into(),
                part: 0,
            },
            project: "test".into(),
            branch: "test".into(),
            timestamp_ms: 0,
            kind: Kind::User,
            call_id: None,
            text: "x".repeat(600),
        })
        .await?;
    Ok(storage)
}

#[tokio::test(start_paused = true)]
async fn renewal_moves_expiry_and_never_revives_a_replaced_claim() {
    let storage = with_job().await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let node = Node::try_from(claim.range).unwrap();
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
    assert!(matches!(
        storage.release(node, claim.token, Duration::ZERO).await,
        Err(Error::Conflict)
    ));
    storage
        .complete(Completion {
            range: replacement.range,
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

#[tokio::test(start_paused = true)]
async fn delayed_release_is_fenced_and_available_only_after_backoff() {
    let storage = with_job().await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let node = Node::try_from(claim.range).unwrap();
    storage
        .release(node, claim.token, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(matches!(
        storage.renew(node, claim.token).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        storage
            .complete(Completion {
                range: claim.range,
                token: claim.token,
                text: "late".into()
            })
            .await,
        Err(Error::Conflict)
    ));
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(storage.claim().await.unwrap().is_none());
    tokio::time::advance(Duration::from_secs(1)).await;
    let replacement = storage.claim().await.unwrap().unwrap();
    assert_ne!(claim.token, replacement.token);
    storage
        .release(node, replacement.token, Duration::ZERO)
        .await
        .unwrap();
    assert!(storage.claim().await.unwrap().is_some());
    assert!(storage.claim().await.unwrap().is_none());
}

#[tokio::test]
async fn invalid_release_delay_does_not_change_the_claim() {
    let storage = with_job().await.unwrap();
    let claim = storage.claim().await.unwrap().unwrap();
    let node = Node::try_from(claim.range).unwrap();
    assert!(matches!(
        storage.release(node, claim.token, Duration::MAX).await,
        Err(Error::Invalid)
    ));
    storage
        .complete(Completion {
            range: claim.range,
            token: claim.token,
            text: "summary".into(),
        })
        .await
        .unwrap();
}
