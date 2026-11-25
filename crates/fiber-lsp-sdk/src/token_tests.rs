use crate::{ChannelKeyId, MemoryStore, RootKey, RootSigner};

use super::{HostedSession, HostedSessionState};

async fn session() -> (HostedSession<MemoryStore>, MemoryStore) {
    let store = MemoryStore::default();
    let root = RootSigner::create(RootKey::import([42; 32]).unwrap(), store.clone())
        .await
        .unwrap();
    (HostedSession::new(root), store)
}

#[tokio::test]
async fn expiry_detection_handles_boundaries_missing_metadata_and_overflow() {
    let (mut session, _) = session().await;
    assert!(session.is_token_near_expiry(100_000, 1_000));
    session.replace_token("old-token".into(), 110_000).unwrap();
    let before = session.state().clone();
    assert!(!session.is_token_near_expiry(108_999, 1_000));
    assert!(session.is_token_near_expiry(109_000, 1_000));
    assert!(!session.is_token_near_expiry(109_999, 0));
    assert!(session.is_token_near_expiry(110_000, 0));
    assert!(session.is_token_near_expiry(120_000, 0));
    assert!(session.is_token_near_expiry(u64::MAX - 10, 100));
    assert_eq!(*session.state(), before);
    session.state.token_expires_at_ms = None;
    assert!(session.is_token_near_expiry(100_000, 1_000));
}

#[tokio::test]
async fn token_replacement_preserves_channel_state_and_survives_caller_persistence() {
    let (mut session, store) = session().await;
    session.allocate_pending_channel().await.unwrap();
    let key_id = session.pending_channel_key_id().unwrap();
    let material = session
        .open_channel(key_id)
        .await
        .unwrap()
        .channel_open_material(false)
        .await
        .unwrap();
    session
        .state
        .bindings
        .insert([1; 32].into(), ChannelKeyId([2; 32].into()));
    session.state.opening_request = Some(vec![3, 4, 5]);
    session.replace_token("old-token".into(), 110_000).unwrap();
    let before = session.state().clone();
    session.replace_token("new-token".into(), 210_000).unwrap();
    assert_eq!(session.tenant_token(), Some("new-token"));
    assert_eq!(session.state().token_expires_at_ms, Some(210_000));
    assert_eq!(session.state().pending, before.pending);
    assert_eq!(session.state().bindings, before.bindings);
    assert_eq!(session.state().opening_request, before.opening_request);

    // Applications continue to own session storage and transport credential changes.
    let encoded = serde_json::to_string(session.state()).unwrap();
    assert!(!encoded.contains("old-token"));
    let restored: HostedSessionState = serde_json::from_str(&encoded).unwrap();
    let root = RootSigner::open(RootKey::import([42; 32]).unwrap(), store)
        .await
        .unwrap();
    let restored = HostedSession::new(root).with_state(restored);
    assert_eq!(restored.state(), session.state());
    assert!(!restored.is_token_near_expiry(200_000, 1_000));
    let restored_material = restored
        .open_channel(key_id)
        .await
        .unwrap()
        .channel_open_material(false)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(crate::json::open_material_to_rpc(&material)).unwrap(),
        serde_json::to_value(crate::json::open_material_to_rpc(&restored_material)).unwrap()
    );
}

#[tokio::test]
async fn invalid_replacement_keeps_previous_credentials() {
    let (mut session, _) = session().await;
    session.replace_token("old-token".into(), 110_000).unwrap();
    let before = session.state().clone();
    assert!(session.replace_token(String::new(), 210_000).is_err());
    assert_eq!(*session.state(), before);
    assert!(session.replace_token("new-token".into(), 0).is_err());
    assert_eq!(*session.state(), before);
}
