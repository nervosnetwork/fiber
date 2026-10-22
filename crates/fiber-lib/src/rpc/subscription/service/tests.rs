use std::time::Duration;

use fiber_json_types::{SubscriptionEvent, SubscriptionOptions, SubscriptionTopic};
use fiber_types::{Hash256, NodeId};
use futures::{StreamExt, TryStreamExt};

use crate::{
    fiber::channel::{ChannelActorState, ChannelActorStateStore},
    signing::{
        notifications::{SigningChange, SigningScope, SIGNING_EVENT_BUFFER},
        SigningEvents,
    },
    store::{open_store, sample::StoreSample, Store},
};

use super::{Source, SubscriptionService};

fn source(store: &Store, events: &SigningEvents, node: NodeId) -> Source<Store> {
    Source {
        events: events.clone(),
        tenant: None,
        channels: store.clone(),
        #[cfg(feature = "watchtower")]
        watchtower: store.clone(),
        node,
    }
}

#[tokio::test]
async fn live_events_filter_deduplicate_and_remove_without_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let state = ChannelActorState::samples(42).remove(0);
    let id = state.get_id();
    store.insert_channel_actor_state(state.clone());
    let events = SigningEvents::default();
    let service = SubscriptionService::new(true, false);
    let mut all = service
        .subscribe(
            source(&store, &events, NodeId::local()),
            SubscriptionTopic::ChannelSigning,
            SubscriptionOptions::default(),
        )
        .unwrap();
    let mut filtered = service
        .subscribe(
            source(&store, &events, NodeId::local()),
            SubscriptionTopic::ChannelSigning,
            SubscriptionOptions {
                channel_ids: Some(vec![id.into()]),
            },
        )
        .unwrap();
    // Raw persistence deliberately has no subscription side effects.
    let mut unrelated = state.clone();
    unrelated.id = Hash256::from([9; 32]);
    store.insert_channel_actor_state(unrelated.clone());
    assert!(tokio::time::timeout(Duration::from_millis(20), all.next())
        .await
        .is_err());
    // Both the initial source and the last DB handle are gone. Only public
    // business events can drive these streams now (including dedup and filters).
    drop(store);
    let topic = events.hub.topic(SigningScope::Channel(None));
    let publish = |state: &ChannelActorState| {
        topic.write().publish(|| SigningChange::Channel {
            channel_id: state.get_id(),
            status: crate::rpc::channel::to_rpc_channel_signing_status(state),
        })
    };
    publish(&state); // Unchanged initial status is deduplicated.
    publish(&unrelated);
    assert!(
        matches!(all.try_next().await.unwrap().unwrap(), SubscriptionEvent::ChannelSigning(result) if result.channel_id == unrelated.id.into())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), filtered.next())
            .await
            .is_err()
    );
    topic.write().publish(|| SigningChange::Removed(id));
    for stream in [&mut all, &mut filtered] {
        assert!(
            matches!(stream.try_next().await.unwrap().unwrap(), SubscriptionEvent::Removed { channel_id } if channel_id == id.into())
        );
    }
    // Duplicate and unknown removals generate no notification.
    topic.write().publish(|| SigningChange::Removed(id));
    assert!(tokio::time::timeout(Duration::from_millis(20), all.next())
        .await
        .is_err());
}

#[tokio::test]
async fn event_lag_ends_subscription_without_database_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let events = SigningEvents::default();
    let mut stream = SubscriptionService::new(true, false)
        .subscribe(
            source(&store, &events, NodeId::local()),
            SubscriptionTopic::ChannelSigning,
            SubscriptionOptions::default(),
        )
        .unwrap();
    drop(store);
    let topic = events.hub.topic(SigningScope::Channel(None));
    for _ in 0..=SIGNING_EVENT_BUFFER {
        topic
            .write()
            .publish(|| SigningChange::Removed(Hash256::from([1; 32])));
    }
    let error = stream.try_next().await.unwrap_err();
    assert!(error
        .to_string()
        .contains("reconnect to recover pending requests"));
    assert!(stream.next().await.is_none());
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn watchtower_replays_pending_then_processes_memory_events() {
    use crate::watchtower::WatchtowerStore;
    use fiber_json_types::WatchtowerSigningStatus;
    use fiber_types::{
        OnchainKeyPurpose, OnchainSigningContent, Privkey, SettlementData,
        WatchtowerExternalSignerState, WatchtowerSignerState,
    };

    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let events = SigningEvents::default();
    let node = NodeId::from_bytes(vec![2; 33]);
    let id = Hash256::from([1; 32]);
    store.insert_watch_channel(
        node.clone(),
        id,
        None,
        None,
        Privkey::from(&[1; 32]).pubkey(),
        Privkey::from(&[2; 32]).pubkey(),
        Privkey::from(&[3; 32]).pubkey(),
        Privkey::from(&[4; 32]).pubkey(),
        SettlementData {
            local_amount: 100,
            remote_amount: 200,
            tlcs: vec![],
        },
    );
    let request_id = Hash256::from([3; 32]);
    store.put_watchtower_signer(
        &node,
        &id,
        WatchtowerSignerState::External(WatchtowerExternalSignerState {
            pending_requests: [(
                request_id,
                OnchainSigningContent {
                    key_purpose: OnchainKeyPurpose::Settlement,
                    transaction: ckb_types::core::TransactionBuilder::default()
                        .build()
                        .data(),
                },
            )]
            .into(),
            ..Default::default()
        }),
    );
    let mut stream = SubscriptionService::new(false, true)
        .subscribe(
            source(&store, &events, node.clone()),
            SubscriptionTopic::WatchtowerSigning,
            SubscriptionOptions::default(),
        )
        .unwrap();
    drop(store);
    assert!(
        matches!(stream.try_next().await.unwrap().unwrap(), SubscriptionEvent::WatchtowerSigning(result)
        if matches!(result.status, WatchtowerSigningStatus::SignatureRequired {request_id: actual, ..} if actual == request_id.into()))
    );
    let topic = events.hub.topic(SigningScope::Watchtower(node));
    // Orphan signer state is hidden until a watched-channel creation is known.
    let orphan = Hash256::from([5; 32]);
    topic.write().publish(|| SigningChange::WatchtowerUpdated {
        channel_id: orphan,
        status: WatchtowerSigningStatus::NoSignatureRequired,
    });
    topic.write().publish(|| SigningChange::WatchtowerUpdated {
        channel_id: id,
        status: WatchtowerSigningStatus::NoSignatureRequired,
    });
    assert!(
        matches!(stream.try_next().await.unwrap().unwrap(), SubscriptionEvent::WatchtowerSigning(result)
        if result.channel_id == id.into() && matches!(result.status, WatchtowerSigningStatus::NoSignatureRequired))
    );
    topic.write().publish(|| SigningChange::WatchtowerCreated {
        channel_id: orphan,
        status: WatchtowerSigningStatus::NoSignatureRequired,
    });
    assert!(
        matches!(stream.try_next().await.unwrap().unwrap(), SubscriptionEvent::WatchtowerSigning(result) if result.channel_id == orphan.into())
    );
    topic.write().publish(|| SigningChange::Removed(id));
    assert!(
        matches!(stream.try_next().await.unwrap().unwrap(), SubscriptionEvent::Removed {channel_id} if channel_id == id.into())
    );
}
