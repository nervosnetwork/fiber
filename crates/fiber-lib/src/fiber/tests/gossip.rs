use std::{collections::HashSet, sync::Arc};

use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::{Bytes, OutPoint},
    prelude::{Builder, Entity, Unpack},
};
use molecule::prelude::Byte;
use ractor::{concurrency::Duration, Actor, ActorProcessingErr, ActorRef};
use tokio::sync::RwLock;

use crate::tests::test_utils::{
    establish_channel_between_nodes, ChannelParameters, NetworkNode, NetworkNodeConfigBuilder,
};
use crate::{
    ckb::tests::test_utils::{MockChainActor, MockChainState},
    fiber::types::{ChannelUpdateChannelFlags, NodeAnnouncement},
};
use crate::{
    ckb::{
        contracts::{get_script_by_contract, Contract},
        tests::test_utils::MockCkbChainClient,
    },
    fiber::{
        gossip::{GossipActorMessage, GossipConfig, GossipService},
        network::{get_chain_hash, PeerChannelIndex},
        ChannelAnnouncement,
    },
};
use crate::{
    ckb::{tests::test_utils::submit_tx, CkbChainMessage},
    fiber::{
        gossip::{
            ExtendedGossipMessageStoreMessage, GossipMessageStore, GossipMessageUpdates,
            SubscribableGossipMessageStore,
        },
        types::{BroadcastMessage, BroadcastMessageWithTimestamp, Cursor},
    },
    gen_node_announcement_from_privkey, gen_rand_node_announcement,
    store::{open_store, Store},
};
use crate::{create_invalid_ecdsa_signature, now_timestamp_as_millis_u64, ChannelTestContext};

use crate::test_utils::{get_test_root_actor, TempDir};

struct GossipTestingContext {
    chain_actor: ActorRef<CkbChainMessage>,
    gossip_actor: ActorRef<GossipActorMessage>,
    gossip_service: GossipService<Store, MockCkbChainClient>,
}

impl GossipTestingContext {
    async fn new() -> Self {
        Self::new_with_gossip_config(GossipConfig::default()).await
    }

    async fn new_with_gossip_config(gossip_config: GossipConfig) -> Self {
        let dir = TempDir::new("test-gossip-store");
        let store = open_store(dir).expect("created store failed");
        let shared_state = Arc::new(std::sync::RwLock::new(MockChainState::new()));
        let chain_actor = Actor::spawn(None, MockChainActor::new(), (None, shared_state.clone()))
            .await
            .expect("start mock chain actor")
            .0;
        let root_actor = get_test_root_actor().await;

        let (gossip_service, gossip_protocol_handle) = GossipService::start(
            gossip_config,
            store.clone(),
            chain_actor.clone(),
            MockCkbChainClient::new(shared_state.clone()),
            None,
            PeerChannelIndex::default(),
            root_actor.get_cell(),
        )
        .await;

        Self {
            chain_actor,
            gossip_actor: gossip_protocol_handle.actor().clone(),
            gossip_service,
        }
    }
}

impl GossipTestingContext {
    fn get_chain_actor(&self) -> &ActorRef<CkbChainMessage> {
        &self.chain_actor
    }

    fn get_store_update_subscriber(&self) -> impl SubscribableGossipMessageStore {
        self.gossip_service.get_subscriber()
    }

    fn get_store(&self) -> &Store {
        self.gossip_service.get_store()
    }

    fn get_extended_actor(&self) -> &ActorRef<ExtendedGossipMessageStoreMessage> {
        self.gossip_service.get_extended_actor()
    }

    async fn subscribe(&self, cursor: Cursor) -> Arc<RwLock<Vec<BroadcastMessageWithTimestamp>>> {
        let (subscriber, messages) = Subscriber::start_actor().await;
        self.get_store_update_subscriber()
            .subscribe(cursor, subscriber, |m| Some(SubscriberMessage::Update(m)))
            .await
            .expect("subscribe to store updates");
        messages
    }

    fn save_message(&self, message: BroadcastMessage) {
        self.get_extended_actor()
            .send_message(ExtendedGossipMessageStoreMessage::SaveMessages(
                crate::gen_rand_fiber_public_key(),
                vec![message],
            ))
            .expect("send message");
    }

    fn process_remote_messages(&self, peer: crate::fiber::Pubkey, messages: Vec<BroadcastMessage>) {
        self.get_extended_actor()
            .send_message(ExtendedGossipMessageStoreMessage::SaveMessages(
                peer, messages,
            ))
            .expect("send message");
    }

    async fn submit_tx(&self, tx: TransactionView) -> TxStatus {
        submit_tx(self.get_chain_actor().clone(), tx).await
    }
}

// A subscriber which subscribes to the store updates and save all updates to a vector.
struct Subscriber {
    messages: Arc<RwLock<Vec<BroadcastMessageWithTimestamp>>>,
}

impl Subscriber {
    fn new() -> Self {
        Subscriber {
            messages: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn start_actor() -> (
        ActorRef<SubscriberMessage>,
        Arc<RwLock<Vec<BroadcastMessageWithTimestamp>>>,
    ) {
        let subscriber = Subscriber::new();
        let messages = subscriber.messages.clone();
        let (actor, _) = Actor::spawn(None, subscriber, ())
            .await
            .expect("start subscriber");
        (actor, messages)
    }
}

enum SubscriberMessage {
    Update(GossipMessageUpdates),
}

#[async_trait::async_trait]
impl Actor for Subscriber {
    type Msg = SubscriberMessage;
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SubscriberMessage::Update(updates) => {
                let mut messages = self.messages.write().await;
                messages.extend(updates.messages);
            }
        }
        Ok(())
    }
}

#[tokio::test]
// Not supported on wasm: requires filesystem access
async fn test_save_gossip_message() {
    let context = GossipTestingContext::new().await;
    let (_, announcement) = gen_rand_node_announcement();
    context.save_message(BroadcastMessage::NodeAnnouncement(announcement.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_node_announcement(&announcement.node_id)
        .expect("get latest node announcement");
    assert_eq!(new_announcement, announcement);
}

#[tokio::test]
async fn test_saving_unconfirmed_channel_announcement() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint());
    assert_eq!(new_announcement, None);
}

#[tokio::test]
async fn test_saving_confirmed_channel_announcement() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint());
    assert_ne!(new_announcement, None);
}

#[tokio::test]
// Not supported on wasm: requires filesystem access
async fn test_saving_invalid_channel_announcement() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    let tx = channel_context.funding_tx.clone();
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let output = tx.output(0).expect("get output").clone();
    let invalid_lock = output
        .lock()
        .as_builder()
        .args(
            Bytes::new_builder()
                .set(b"wrong lock args".iter().map(|b| Byte::new(*b)).collect())
                .build(),
        )
        .build();
    let invalid_output = output.as_builder().lock(invalid_lock).build();
    let invalid_tx = tx
        .as_advanced_builder()
        .set_outputs(vec![invalid_output])
        .build();
    let status = context.submit_tx(invalid_tx).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint());
    assert_eq!(new_announcement, None);
}

#[tokio::test]
async fn test_reject_channel_announcement_with_outpoint_index_not_zero() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;

    let real_output = channel_context
        .funding_tx
        .output(0)
        .expect("get output")
        .clone();
    let dummy_lock = get_script_by_contract(Contract::Secp256k1Lock, b"dummy_placeholder");
    let dummy_output = real_output.clone().as_builder().lock(dummy_lock).build();

    let tx_with_output_at_index_1 = TransactionView::new_advanced_builder()
        .output(dummy_output)
        .output_data(Bytes::default())
        .output(real_output)
        .output_data(Bytes::default())
        .build();

    let outpoint_index_1 = OutPoint::new_builder()
        .tx_hash(tx_with_output_at_index_1.hash())
        .index(1u32)
        .build();
    let xonly = channel_context.funding_tx_sk.x_only_pub_key();
    let capacity: u64 = channel_context
        .funding_tx
        .output(0)
        .unwrap()
        .capacity()
        .unpack();
    let mut announcement = ChannelAnnouncement::new_unsigned(
        &channel_context.node1_sk.pubkey(),
        &channel_context.node2_sk.pubkey(),
        outpoint_index_1.clone(),
        get_chain_hash(),
        &xonly,
        capacity as u128,
        None,
    );
    let message = announcement.message_to_sign();
    announcement.ckb_signature = Some(channel_context.funding_tx_sk.sign_schnorr(message));
    announcement.node1_signature = Some(channel_context.node1_sk.sign(message));
    announcement.node2_signature = Some(channel_context.node2_sk.sign(message));

    context.save_message(BroadcastMessage::ChannelAnnouncement(announcement));
    let status = context.submit_tx(tx_with_output_at_index_1).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let saved = context
        .get_store()
        .get_latest_channel_announcement(&outpoint_index_1);
    assert!(
        saved.is_some(),
        "should accept ChannelAnnouncement when funding output matches the announced outpoint index"
    );
}

#[tokio::test]
// Not supported on wasm: requires filesystem
async fn test_saving_channel_update_after_saving_channel_announcement() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint());
    assert_ne!(new_announcement, None);
    for channel_update in [
        channel_context.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
        channel_context.create_channel_update_of_node2(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
    ] {
        context.save_message(BroadcastMessage::ChannelUpdate(channel_update.clone()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for b in [true, false] {
        let channel_update = context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), b);
        assert_ne!(channel_update, None);
    }
}

#[tokio::test]
async fn test_saving_channel_update_before_saving_channel_announcement() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;

    for channel_update in [
        channel_context.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
        channel_context.create_channel_update_of_node2(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
    ] {
        context.save_message(BroadcastMessage::ChannelUpdate(channel_update.clone()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for b in [true, false] {
        let channel_update = context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), b);
        assert_eq!(channel_update, None);
    }

    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint());
    assert_ne!(new_announcement, None);
    for b in [true, false] {
        let channel_update = context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), b);
        // The channel update messages are discarded because we thought they are invalid.
        assert_eq!(channel_update, None);
    }
}

#[tokio::test]
async fn test_deferred_channel_announcement_keeps_dependent_update_pending() {
    let context = GossipTestingContext::new().await;
    let peer = crate::gen_rand_fiber_public_key();
    let channel_context = ChannelTestContext::gen().await;
    let channel_update = channel_context.create_channel_update_of_node1(
        ChannelUpdateChannelFlags::empty(),
        42,
        42,
        42,
        None,
    );

    context.process_remote_messages(
        peer,
        vec![
            BroadcastMessage::ChannelAnnouncement(channel_context.channel_announcement.clone()),
            BroadcastMessage::ChannelUpdate(channel_update.clone()),
        ],
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None
    );
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        None
    );

    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None,
        "deferred channel announcement should remain pending until funding tx is visible",
    );
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        Some(channel_update),
        "dependent channel update should remain pending until deferred announcement is verified",
    );
}

#[tokio::test]
async fn test_saving_invalid_channel_update() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint());
    assert_ne!(new_announcement, None);
    for mut channel_update in [
        channel_context.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
        channel_context.create_channel_update_of_node2(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
    ] {
        channel_update.signature = Some(create_invalid_ecdsa_signature());
        context.save_message(BroadcastMessage::ChannelUpdate(channel_update.clone()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for b in [true, false] {
        let channel_update = context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), b);
        assert_eq!(channel_update, None);
    }
}

#[tokio::test]
async fn test_saving_channel_update_independency() {
    async fn test(node1_has_invalid_signature: bool, node2_has_invalid_signature: bool) {
        let context = GossipTestingContext::new().await;
        let channel_context = ChannelTestContext::gen().await;
        context.save_message(BroadcastMessage::ChannelAnnouncement(
            channel_context.channel_announcement.clone(),
        ));
        let status = context.submit_tx(channel_context.funding_tx.clone()).await;
        assert!(matches!(status, TxStatus::Committed(..)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let new_announcement = context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint());
        assert_ne!(new_announcement, None);
        for mut channel_update in [
            channel_context.create_channel_update_of_node1(
                ChannelUpdateChannelFlags::empty(),
                42,
                42,
                42,
                None,
            ),
            channel_context.create_channel_update_of_node2(
                ChannelUpdateChannelFlags::empty(),
                42,
                42,
                42,
                None,
            ),
        ] {
            if channel_update.is_update_of_node_1() && node1_has_invalid_signature {
                channel_update.signature = Some(create_invalid_ecdsa_signature());
            }
            if channel_update.is_update_of_node_2() && node2_has_invalid_signature {
                channel_update.signature = Some(create_invalid_ecdsa_signature());
            }
            context.save_message(BroadcastMessage::ChannelUpdate(channel_update.clone()));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        for is_channel_update_of_node1 in [true, false] {
            let channel_update = context.get_store().get_latest_channel_update(
                channel_context.channel_outpoint(),
                is_channel_update_of_node1,
            );
            if is_channel_update_of_node1 {
                if node1_has_invalid_signature {
                    assert_eq!(channel_update, None);
                } else {
                    assert_ne!(channel_update, None);
                }
            } else if node2_has_invalid_signature {
                assert_eq!(channel_update, None);
            } else {
                assert_ne!(channel_update, None);
            }
        }
    }

    for node1_has_invalid_signature in [true, false] {
        for node2_has_invalid_signature in [true, false] {
            test(node1_has_invalid_signature, node2_has_invalid_signature).await;
        }
    }
}

#[tokio::test]
async fn test_channel_update_limiter_drops_excess_updates_for_same_direction() {
    let gossip_config = GossipConfig {
        policy: crate::fiber::gossip_policy::GossipPolicyConfig {
            inbound_channel_update: crate::fiber::gossip_policy::ChannelUpdateRateLimitConfig {
                interval_ms: 60_000,
                burst: 1,
            },
            ..crate::fiber::gossip_policy::GossipPolicyConfig::default()
        },
        ..GossipConfig::default()
    };
    let context = GossipTestingContext::new_with_gossip_config(gossip_config).await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let update1 = channel_context.create_channel_update_of_node1(
        ChannelUpdateChannelFlags::empty(),
        42,
        42,
        42,
        Some(1_000),
    );
    let update2 = channel_context.create_channel_update_of_node1(
        ChannelUpdateChannelFlags::empty(),
        43,
        43,
        43,
        Some(2_000),
    );
    context.process_remote_messages(
        crate::gen_rand_fiber_public_key(),
        vec![
            BroadcastMessage::ChannelUpdate(update1.clone()),
            BroadcastMessage::ChannelUpdate(update2),
        ],
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        Some(update1)
    );
}

#[tokio::test]
async fn test_channel_update_limiter_does_not_block_other_direction() {
    let gossip_config = GossipConfig {
        policy: crate::fiber::gossip_policy::GossipPolicyConfig {
            inbound_channel_update: crate::fiber::gossip_policy::ChannelUpdateRateLimitConfig {
                interval_ms: 60_000,
                burst: 1,
            },
            ..crate::fiber::gossip_policy::GossipPolicyConfig::default()
        },
        ..GossipConfig::default()
    };
    let context = GossipTestingContext::new_with_gossip_config(gossip_config).await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let update_of_node1 = channel_context.create_channel_update_of_node1(
        ChannelUpdateChannelFlags::empty(),
        42,
        42,
        42,
        Some(1_000),
    );
    let update_of_node2 = channel_context.create_channel_update_of_node2(
        ChannelUpdateChannelFlags::empty(),
        43,
        43,
        43,
        Some(2_000),
    );
    context.process_remote_messages(
        crate::gen_rand_fiber_public_key(),
        vec![
            BroadcastMessage::ChannelUpdate(update_of_node1.clone()),
            BroadcastMessage::ChannelUpdate(update_of_node2.clone()),
        ],
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        Some(update_of_node1)
    );
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), false),
        Some(update_of_node2)
    );
}

#[tokio::test]
async fn test_channel_update_limiter_isolated_per_peer_for_same_direction() {
    let gossip_config = GossipConfig {
        policy: crate::fiber::gossip_policy::GossipPolicyConfig {
            inbound_channel_update: crate::fiber::gossip_policy::ChannelUpdateRateLimitConfig {
                interval_ms: 60_000,
                burst: 1,
            },
            ..crate::fiber::gossip_policy::GossipPolicyConfig::default()
        },
        ..GossipConfig::default()
    };
    let context = GossipTestingContext::new_with_gossip_config(gossip_config).await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut invalid = channel_context.create_channel_update_of_node1(
        ChannelUpdateChannelFlags::empty(),
        41,
        41,
        41,
        Some(1_000),
    );
    invalid.signature = Some(create_invalid_ecdsa_signature());
    let valid = channel_context.create_channel_update_of_node1(
        ChannelUpdateChannelFlags::empty(),
        42,
        42,
        42,
        Some(2_000),
    );

    context.process_remote_messages(
        crate::gen_rand_fiber_public_key(),
        vec![BroadcastMessage::ChannelUpdate(invalid)],
    );
    context.process_remote_messages(
        crate::gen_rand_fiber_public_key(),
        vec![BroadcastMessage::ChannelUpdate(valid.clone())],
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        Some(valid)
    );
}

#[tokio::test]
async fn test_saving_channel_update_with_invalid_channel_announcement() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let tx = channel_context.funding_tx.clone();
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let output = tx.output(0).expect("get output").clone();
    let invalid_lock = output
        .lock()
        .as_builder()
        .args(
            Bytes::new_builder()
                .set(b"wrong lock args".iter().map(|b| Byte::new(*b)).collect())
                .build(),
        )
        .build();
    let invalid_output = output.as_builder().lock(invalid_lock).build();
    let invalid_tx = tx
        .as_advanced_builder()
        .set_outputs(vec![invalid_output])
        .build();
    let status = context.submit_tx(invalid_tx).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_announcement = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint());
    assert_eq!(new_announcement, None);
    for channel_update in [
        channel_context.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
        channel_context.create_channel_update_of_node2(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            None,
        ),
    ] {
        context.save_message(BroadcastMessage::ChannelUpdate(channel_update.clone()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for b in [true, false] {
        let channel_update = context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), b);
        assert_eq!(channel_update, None);
    }
}

#[tokio::test]
async fn test_save_outdated_gossip_message() {
    let context = GossipTestingContext::new().await;
    let (sk, old_announcement) = gen_rand_node_announcement();
    // Make sure new announcement has a different timestamp
    tokio::time::sleep(Duration::from_millis(2)).await;
    let new_announcement = gen_node_announcement_from_privkey(&sk);
    context.save_message(BroadcastMessage::NodeAnnouncement(new_announcement.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let announcement_in_store = context
        .get_store()
        .get_latest_node_announcement(&new_announcement.node_id)
        .expect("get latest node announcement");
    assert_eq!(announcement_in_store, new_announcement);

    context.save_message(BroadcastMessage::NodeAnnouncement(old_announcement.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let announcement_in_store = context
        .get_store()
        .get_latest_node_announcement(&new_announcement.node_id)
        .expect("get latest node announcement");
    assert_eq!(announcement_in_store, new_announcement);
}

#[tokio::test]
async fn test_gossip_store_updates_basic_subscription() {
    let context = GossipTestingContext::new().await;
    let messages = context.subscribe(Default::default()).await;
    let (_, announcement) = gen_rand_node_announcement();
    context.save_message(BroadcastMessage::NodeAnnouncement(announcement.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let messages = messages.read().await;
    assert!(messages.len() == 1);
    assert_eq!(
        messages[0],
        BroadcastMessageWithTimestamp::NodeAnnouncement(announcement)
    );
}

#[tokio::test]
async fn test_gossip_store_updates_repeated_saving() {
    let context = GossipTestingContext::new().await;
    let messages = context.subscribe(Default::default()).await;
    let (_, announcement) = gen_rand_node_announcement();
    for _ in 0..10 {
        context.save_message(BroadcastMessage::NodeAnnouncement(announcement.clone()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let messages = messages.read().await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(
        messages[0],
        BroadcastMessageWithTimestamp::NodeAnnouncement(announcement)
    );
}

#[tokio::test]
async fn test_gossip_store_updates_saving_multiple_messages() {
    let context = GossipTestingContext::new().await;
    let messages = context.subscribe(Default::default()).await;
    let announcements = (0..10)
        .map(|_| gen_rand_node_announcement().1)
        .collect::<Vec<_>>();
    for announcement in &announcements {
        context.save_message(BroadcastMessage::NodeAnnouncement(announcement.clone()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let messages = messages.read().await;
    assert_eq!(
        messages.iter().cloned().collect::<HashSet<_>>(),
        announcements
            .into_iter()
            .map(BroadcastMessageWithTimestamp::NodeAnnouncement)
            .collect::<HashSet<_>>()
    );
}

#[tokio::test]
async fn test_gossip_store_updates_saving_outdated_message() {
    let context = GossipTestingContext::new().await;
    let messages = context.subscribe(Default::default()).await;
    let (sk, old_announcement) = gen_rand_node_announcement();
    // Make sure new announcement has a different timestamp
    tokio::time::sleep(Duration::from_millis(2)).await;
    let new_announcement = gen_node_announcement_from_privkey(&sk);
    for announcement in [&old_announcement, &new_announcement] {
        context.save_message(BroadcastMessage::NodeAnnouncement(announcement.clone()));
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let messages = messages.read().await;
    // The subscriber may or may not receive the old announcement, but it should always receive the
    // new announcement.
    assert_eq!(
        messages[messages.len() - 1],
        BroadcastMessageWithTimestamp::NodeAnnouncement(new_announcement)
    );
}

async fn check_two_node_announcements_with_one_invalid(
    valid_announcement: NodeAnnouncement,
    invalid_announcement: NodeAnnouncement,
) {
    // Checking both saving orders (valid first, invalid first)
    for announcements in [
        [&valid_announcement, &invalid_announcement],
        [&invalid_announcement, &valid_announcement],
    ] {
        let context = GossipTestingContext::new().await;
        let messages = context.subscribe(Default::default()).await;
        for announcement in announcements {
            context.save_message(BroadcastMessage::NodeAnnouncement(announcement.clone()));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let messages = messages.read().await;
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            BroadcastMessageWithTimestamp::NodeAnnouncement(valid_announcement.clone())
        );
    }
}

// Old message is invalid, new message is valid
#[tokio::test]
async fn test_gossip_store_updates_saving_invalid_message_1() {
    let (sk, mut old_announcement) = gen_rand_node_announcement();
    old_announcement.signature = Some(create_invalid_ecdsa_signature());
    // Make sure new announcement has a different timestamp
    tokio::time::sleep(Duration::from_millis(2)).await;
    let new_announcement = gen_node_announcement_from_privkey(&sk);

    check_two_node_announcements_with_one_invalid(new_announcement, old_announcement).await;
}

// New message is invalid, old message is valid
#[tokio::test]
async fn test_gossip_store_updates_saving_invalid_message_2() {
    let (sk, old_announcement) = gen_rand_node_announcement();
    // Make sure new announcement has a different timestamp
    tokio::time::sleep(Duration::from_millis(2)).await;
    let mut new_announcement = gen_node_announcement_from_privkey(&sk);
    new_announcement.signature = Some(create_invalid_ecdsa_signature());

    check_two_node_announcements_with_one_invalid(old_announcement, new_announcement).await;
}

// Both messages have the same timestamp, but there is one invalid message
#[tokio::test]
async fn test_gossip_store_updates_saving_invalid_message_3() {
    let (_, old_announcement) = gen_rand_node_announcement();
    let mut new_announcement = old_announcement.clone();
    new_announcement.signature = Some(create_invalid_ecdsa_signature());

    check_two_node_announcements_with_one_invalid(old_announcement, new_announcement).await;
}

#[tokio::test]
async fn test_our_own_channel_gossip_message_propagated() {
    crate::tests::test_utils::init_tracing();
    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 11800000000;

    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (_new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters::new(node_a_funding_amount, node_b_funding_amount),
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    for node in [&node_a, &node_b] {
        node.with_network_graph(|graph| {
            let channels = graph.channels().collect::<Vec<_>>();
            assert_eq!(channels.len(), 1);

            let channel = channels[0].clone();
            assert!(channel.update_of_node1.is_some());
            assert!(channel.update_of_node2.is_some());

            let nodes = graph.nodes().collect::<Vec<_>>();
            assert_eq!(nodes.len(), 2);
        })
        .await;
    }
}

// Regression test: verify that gossip syncing starts immediately when a peer
// connects, without waiting for the periodic TickNetworkMaintenance interval.
// With a 1-hour maintenance interval, syncing would be severely delayed unless
// PeerConnected triggers an immediate tick (as introduced by this fix).
#[tokio::test]
async fn test_gossip_sync_starts_immediately_on_peer_connect() {
    crate::tests::test_utils::init_tracing();

    // Use a very large gossip maintenance interval to ensure sync is driven by
    // the immediate PeerConnected trigger, not the periodic tick.
    const LARGE_INTERVAL_MS: u64 = 3_600_000; // 1 hour

    // Create node A with large maintenance interval and inject a node announcement
    // before connecting node B, so B must sync it via active sync on connection.
    let mut node_a = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .fiber_config_updater(move |c| {
                c.gossip_network_maintenance_interval_ms = Some(LARGE_INTERVAL_MS);
            })
            .build(),
    )
    .await;

    let (_, announcement) = gen_rand_node_announcement();
    node_a.send_message_to_gossip_actor(GossipActorMessage::TryBroadcastMessages(vec![
        BroadcastMessageWithTimestamp::NodeAnnouncement(announcement.clone()),
    ]));

    // Give node A a moment to store the announcement before node B connects.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut node_b = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .fiber_config_updater(move |c| {
                c.gossip_network_maintenance_interval_ms = Some(LARGE_INTERVAL_MS);
            })
            .build(),
    )
    .await;

    // Connect B to A: this should immediately trigger active sync on B.
    node_b.connect_to(&mut node_a).await;

    // Wait a short time (well under the 1-hour maintenance interval).
    // With the fix, B should have actively synced A's announcement by now.
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let synced = node_b
        .get_store()
        .get_latest_node_announcement(&announcement.node_id);
    assert!(
        synced.is_some(),
        "node B should have synced the gossip announcement from node A immediately after connecting, \
         without waiting for the periodic maintenance interval"
    );
}

// We may need to run this test multiple times to check if the gossip messages are really propagated.
#[tokio::test]
async fn test_never_miss_any_message() {
    let (_, announcement) = gen_rand_node_announcement();
    let context = GossipTestingContext::new().await;
    let messages = context.subscribe(Default::default()).await;
    context.save_message(BroadcastMessage::NodeAnnouncement(announcement.clone()));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let messages = messages.read().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0],
        BroadcastMessageWithTimestamp::NodeAnnouncement(announcement)
    );
}

#[tokio::test]
async fn test_gossip_store_prune_all_messages() {
    let context = GossipTestingContext::new().await;
    let num_messages = 1000usize;
    for _i in 1..=num_messages {
        let channel_context = ChannelTestContext::gen().await;
        let status = context.submit_tx(channel_context.funding_tx.clone()).await;
        assert!(matches!(status, TxStatus::Committed(..)));
        context.save_message(BroadcastMessage::ChannelAnnouncement(
            channel_context.channel_announcement.clone(),
        ));
    }
    // Wait for the message to be saved
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0)
            .len(),
        num_messages
    );

    context
        .gossip_actor
        .send_message(GossipActorMessage::PruneStaleGossipMessages(
            now_timestamp_as_millis_u64() + 1,
        ))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0)
            .len(),
        0
    );
}

#[tokio::test]
async fn test_gossip_store_prune_channel_announcement() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let channel_timestamp = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint())
        .expect("channel saved")
        .0;

    context
        .gossip_actor
        .send_message(GossipActorMessage::PruneStaleGossipMessages(
            channel_timestamp - 1,
        ))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_ne!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None
    );
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0)
            .len(),
        1
    );

    context
        .gossip_actor
        .send_message(GossipActorMessage::PruneStaleGossipMessages(
            channel_timestamp + 1,
        ))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None
    );
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0),
        vec![]
    );
}

#[tokio::test]
async fn test_gossip_store_prune_channel_update() {
    let context = GossipTestingContext::new().await;
    let channel_context = ChannelTestContext::gen().await;
    context.save_message(BroadcastMessage::ChannelAnnouncement(
        channel_context.channel_announcement.clone(),
    ));
    let status = context.submit_tx(channel_context.funding_tx.clone()).await;
    assert!(matches!(status, TxStatus::Committed(..)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let channel_announcement_timestamp = context
        .get_store()
        .get_latest_channel_announcement(channel_context.channel_outpoint())
        .expect("channel saved")
        .0;
    // The difference between the timestamp of the channel announcement below is 4.
    // This value is used because we have a convention of using even/odd to differentiate the timestamps
    // of the channel updates from different nodes. I didn't bother to look up which one is even/odd.
    // I just use 4 to make sure they are different.
    for channel_update in [
        channel_context.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            Some(channel_announcement_timestamp + 4),
        ),
        channel_context.create_channel_update_of_node2(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            Some(channel_announcement_timestamp + 8),
        ),
    ] {
        context.save_message(BroadcastMessage::ChannelUpdate(channel_update.clone()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None,
        "channel announcement should be saved"
    );

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        None,
        "channel update of node 1 should be saved"
    );

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), false),
        None,
        "channel update of node 2 should be saved"
    );
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0)
            .len(),
        3
    );

    context
        .gossip_actor
        .send_message(GossipActorMessage::PruneStaleGossipMessages(
            channel_announcement_timestamp + 2,
        ))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_ne!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None,
        "channel announcement should not be pruned if there are active channel updates"
    );

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        None,
        "channel update of node 1 should not be pruned as it is active"
    );

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), false),
        None,
        "channel update of node 2 should not be pruned as it is active"
    );
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0)
            .len(),
        3
    );

    context
        .gossip_actor
        .send_message(GossipActorMessage::PruneStaleGossipMessages(
            channel_announcement_timestamp + 6,
        ))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None,
        "channel announcement should not be pruned if there are active channel updates"
    );

    assert_ne!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        None,
        "channel update of node 1 should not be pruned as channel update of node 2 is active"
    );
    assert_ne!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), false),
        None,
        "channel update of node 2 should not be pruned as it is active"
    );
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0)
            .len(),
        3
    );

    context
        .gossip_actor
        .send_message(GossipActorMessage::PruneStaleGossipMessages(
            channel_announcement_timestamp + 10,
        ))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        context
            .get_store()
            .get_latest_channel_announcement(channel_context.channel_outpoint()),
        None,
        "channel announcement should be pruned because there is no active channel updates"
    );

    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), true),
        None,
        "channel update of node 1 should be pruned as it is outdated"
    );
    assert_eq!(
        context
            .get_store()
            .get_latest_channel_update(channel_context.channel_outpoint(), false),
        None,
        "channel update of node 2 should be pruned as it is outdated"
    );
    assert_eq!(
        context
            .get_store()
            .get_broadcast_messages(&Cursor::default(), 0),
        vec![]
    );
}
