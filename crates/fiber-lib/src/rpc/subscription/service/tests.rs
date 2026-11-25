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
    use fiber_json_types::{
        ChannelSigningStatus, ChannelSigningTransition, Musig2SignableContent,
        Musig2SigningContent, NoncePurpose, NonceSlot, SigningSessionEvidence,
    };
    let mut status = ChannelSigningStatus::SignatureRequired {
        request_id: Hash256::from([3; 32]).into(),
        transition: ChannelSigningTransition::SignChannelAnnouncement,
        content: Musig2SigningContent {
            v2: None,
            slot: NonceSlot {
                purpose: NoncePurpose::ChannelAnnouncement,
                commitment_number: 0,
            },
            commitment_counter: None,
            key_agg_ctx: vec![],
            agg_nonce: vec![],
            content: Musig2SignableContent::ChannelAnnouncement {
                unsigned_announcement: vec![1],
            },
        },
        session_evidence: SigningSessionEvidence {
            peer_public_nonce: vec![],
            peer_partial_signature: None,
        },
        settlement: None,
    };
    // The same request ID can carry changed nested evidence. Compare the complete
    // status, while suppressing repeated copies of each version.
    for nonce in [vec![1; 66], vec![2; 66]] {
        let ChannelSigningStatus::SignatureRequired {
            session_evidence, ..
        } = &mut status
        else {
            unreachable!();
        };
        session_evidence.peer_public_nonce = nonce;
        for _ in 0..2 {
            topic.write().publish(|| SigningChange::Channel {
                channel_id: id,
                status: Ok(status.clone()),
            });
        }
        for stream in [&mut all, &mut filtered] {
            let SubscriptionEvent::ChannelSigning(result) =
                stream.try_next().await.unwrap().unwrap()
            else {
                panic!("channel signing event");
            };
            assert_eq!(result.status, status);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), stream.next())
                    .await
                    .is_err()
            );
        }
    }
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
async fn watchtower_visibility_and_removal_use_only_memory_events() {
    use fiber_json_types::WatchtowerSigningStatus;

    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let events = SigningEvents::default();
    let node = NodeId::from_bytes(vec![2; 33]);
    let id = Hash256::from([1; 32]);
    let mut stream = SubscriptionService::new(false, true)
        .subscribe(
            source(&store, &events, node.clone()),
            SubscriptionTopic::WatchtowerSigning,
            SubscriptionOptions::default(),
        )
        .unwrap();
    drop(store);
    let topic = events.hub.topic(SigningScope::Watchtower(node));
    // Orphan signer state is hidden until a watched-channel creation is known.
    topic.write().publish(|| SigningChange::WatchtowerUpdated {
        channel_id: id,
        status: WatchtowerSigningStatus::NoSignatureRequired,
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), stream.next())
            .await
            .is_err()
    );
    topic.write().publish(|| SigningChange::WatchtowerCreated {
        channel_id: id,
        status: WatchtowerSigningStatus::NoSignatureRequired,
    });
    assert!(
        matches!(stream.try_next().await.unwrap().unwrap(), SubscriptionEvent::WatchtowerSigning(result)
        if result.channel_id == id.into() && matches!(result.status, WatchtowerSigningStatus::NoSignatureRequired))
    );
    topic.write().publish(|| SigningChange::Removed(id));
    assert!(
        matches!(stream.try_next().await.unwrap().unwrap(), SubscriptionEvent::Removed {channel_id} if channel_id == id.into())
    );
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn watchtower_subscription_replays_pending_external_funding_request() {
    use crate::watchtower::WatchtowerStore;
    use ckb_types::{
        core::TransactionBuilder,
        packed::{CellInput, OutPoint},
        prelude::Pack,
    };
    use fiber_json_types::WatchtowerSigningStatus;
    use fiber_types::{
        ChannelFeatures, OnchainKeyPurpose, OnchainSigningContent, PendingWatchtowerRequest,
        Privkey, SettlementData, SettlementKey, WatchChannelKeys, WatchtowerSignerState,
    };

    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let events = SigningEvents::default();
    let node = NodeId::from_bytes(vec![3; 33]);
    let id = Hash256::from([7; 32]);
    store.insert_watch_channel(
        node.clone(),
        id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::external(Privkey::from([1; 32]).pubkey()),
            remote_settlement: Privkey::from([2; 32]).pubkey(),
            local_funding: Privkey::from([3; 32]).pubkey(),
            remote_funding: Privkey::from([4; 32]).pubkey(),
        },
        SettlementData {
            local_amount: 100,
            remote_amount: 100,
            tlcs: vec![],
        },
        ChannelFeatures::V2,
    );
    let request_id = Hash256::from([0xaa; 32]);
    let expiry_since = 0x4000_0000_0000_0064u64;
    let tx = TransactionBuilder::default()
        .input(CellInput::new(OutPoint::new([0x11; 32].pack(), 0), 0))
        .build()
        .data();
    store
        .update_watchtower_signer(&node, &id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests.insert(
                    request_id,
                    PendingWatchtowerRequest {
                        content: OnchainSigningContent {
                            key_purpose: OnchainKeyPurpose::Settlement,
                            transaction: tx.clone(),
                        },
                        created_at_ms: crate::now_timestamp_as_millis_u64(),
                        required_expiry_since: Some(expiry_since),
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    let mut stream = SubscriptionService::new(false, true)
        .subscribe(
            source(&store, &events, node),
            SubscriptionTopic::WatchtowerSigning,
            SubscriptionOptions::default(),
        )
        .unwrap();
    let SubscriptionEvent::WatchtowerSigning(result) = stream.try_next().await.unwrap().unwrap()
    else {
        panic!("expected initial WatchtowerSigning replay event");
    };
    assert_eq!(result.channel_id, id.into());
    match result.status {
        WatchtowerSigningStatus::ExternalFundingRequired {
            request_id: got_id,
            required_expiry_since,
            ..
        } => {
            assert_eq!(got_id, request_id.into());
            assert_eq!(required_expiry_since, Some(expiry_since));
        }
        other => panic!("expected ExternalFundingRequired, got {other:?}"),
    }
}
