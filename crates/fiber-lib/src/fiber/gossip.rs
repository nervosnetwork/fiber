use core::panic;
use secp256k1::SECP256K1;
use std::{
    cmp::max,
    collections::{HashMap, HashSet, VecDeque},
    marker::PhantomData,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use ckb_hash::blake2b_256;
use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::OutPoint,
    prelude::Unpack,
};
use ractor::{
    call, call_t, concurrency::JoinHandle, Actor, ActorCell, ActorProcessingErr, ActorRef,
    ActorRuntime, MessagingErr, OutputPort, RpcReplyPort, SupervisionEvent,
};
use strum::AsRefStr;
use tentacle::{
    async_trait as tasync_trait,
    builder::MetaBuilder,
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, SessionContext},
    multiaddr::Protocol,
    secio::PeerId,
    service::{ProtocolHandle, ProtocolMeta, ServiceAsyncControl, SessionType},
    traits::ServiceProtocol,
    SessionId,
};
use tokio::sync::oneshot;
use tokio_util::codec::length_delimited;
use tracing::{debug, error, info, trace, warn};

#[cfg(any(test, feature = "bench"))]
use crate::fiber::network::DEFAULT_CHAIN_ACTOR_TIMEOUT;
use crate::{
    ckb::{client::CkbChainClient, CkbChainMessage, GetTxResponse},
    fiber::network::MAX_SERVICE_PROTOCOAL_DATA_SIZE,
    now_timestamp_as_millis_u64, unwrap_or_return,
    utils::actor::ActorHandleLogGuard,
    Error,
};

use super::{
    config::DEFAULT_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS,
    gossip_policy::{
        ChannelUpdateLimiter, ChannelUpdateLimiterKey, ChannelUpdateRateLimitConfig,
        GossipPolicyConfig, GossipPolicyState, GossipViolation,
    },
    network::{
        check_chain_hash, get_chain_hash, GossipMessageWithTarget, NetworkActorCommand,
        NetworkActorMessage, PeerChannelIndex, PeerDisconnectReason, GOSSIP_PROTOCOL_ID,
    },
    types::{
        BroadcastMessageQuery, BroadcastMessageQueryFlags, BroadcastMessageWithTimestamp,
        BroadcastMessagesFilter, BroadcastMessagesFilterResult, ChannelOnchainInfo,
        GetBroadcastMessages, GetBroadcastMessagesResult, GossipMessage, QueryBroadcastMessages,
        QueryBroadcastMessagesResult,
    },
    FiberConfig,
};
use fiber_types::{
    BroadcastMessage, BroadcastMessageID, ChannelAnnouncement, ChannelUpdate, Cursor, Hash256,
    NodeAnnouncement, Pubkey,
};

// The maximum duration drift between the broadcast message timestamp and latest cursor in store.
pub(crate) const MAX_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT: Duration =
    Duration::from_secs(60 * 60 * 2);

// The duration to consider a broadcast message as stale. We will start to sync messages no older than
// this duration. The current value is two weeks.
pub(crate) const SOFT_BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION: Duration =
    Duration::from_secs(60 * 60 * 24 * 14);

// The duration to delete a broadcast message from the store. The current value is four weeks.
pub(crate) const HARD_BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION: Duration =
    Duration::from_secs(60 * 60 * 24 * 28);

// The interval to prune stale broadcast messages. The current value is one day.
const PRUNE_STALE_BROADCAST_MESSAGES_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);

const MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT: Duration = Duration::from_secs(60);
const MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT_MILLIS: u64 =
    MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT.as_millis() as u64;

const MAX_NUM_OF_BROADCAST_MESSAGES: u16 = 1000;
pub(crate) const DEFAULT_NUM_OF_BROADCAST_MESSAGE: u16 = 100;

const MAX_NUM_OF_ACTIVE_SYNCING_PEERS: usize = 3;
const ACTOR_HANDLE_WARN_THRESHOLD_MS: u64 = 15_000;
const MIN_NUM_OF_PASSIVE_SYNCING_PEERS: usize = 3;

const NUM_SIMULTANEOUS_GET_REQUESTS: usize = 1;
const GET_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

// The maximum number of concurrent query tasks to run. We will wait for query previous task to exit
// before start new query tasks. This is to avoid consuming to much bandwidth.
const MAX_NUM_CONCURRENT_QUERY_TASKS: usize = 10;

const QUERY_BROADCAST_MESSAGES_TIMEOUT: Duration = Duration::from_secs(20);
const UPDATE_PEER_FILTER_RETRY_DELAY: Duration = Duration::from_millis(500);
const DEFERRED_SYNC_CURSOR_RETRY_DELAY: Duration = Duration::from_millis(500);
const GOSSIP_INGRESS_PASSIVE: &str = "passive";
const GOSSIP_INGRESS_ACTIVE_GET: &str = "active_get";
const GOSSIP_INGRESS_QUERY: &str = "query";

#[cfg(all(feature = "metrics", not(target_arch = "wasm32")))]
#[path = "gossip_metrics.rs"]
mod metrics_impl;
#[cfg(not(all(feature = "metrics", not(target_arch = "wasm32"))))]
#[path = "gossip_metrics_noop.rs"]
mod metrics_impl;

use self::metrics_impl::{
    observe_active_sync_completion, observe_active_sync_finished, observe_active_sync_started,
    observe_active_sync_timeout, observe_applied_broadcast_message,
    observe_applied_propagation_latency, observe_dependency_query_request,
    observe_duplicate_broadcast_message, observe_gossip_received_bytes, observe_gossip_sent_bytes,
    observe_missing_dependency_message, observe_received_broadcast_messages,
    observe_received_propagation_latencies, observe_rejected_broadcast_message,
};

fn max_acceptable_gossip_message_timestamp() -> u64 {
    now_timestamp_as_millis_u64() + MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT_MILLIS
}

fn broadcast_message_type(message: &BroadcastMessage) -> &'static str {
    match message {
        BroadcastMessage::ChannelAnnouncement(_) => "channel_announcement",
        BroadcastMessage::ChannelUpdate(_) => "channel_update",
        BroadcastMessage::NodeAnnouncement(_) => "node_announcement",
    }
}

pub trait GossipMessageStore {
    /// The implementors should guarantee that the returned messages are sorted by timestamp in the ascending order.
    /// `limit` controls the maximum number of messages to return; 0 means no limit (return all).
    fn get_broadcast_messages(
        &self,
        after_cursor: &Cursor,
        limit: usize,
    ) -> Vec<BroadcastMessageWithTimestamp>;

    /// The implementors should guarantee that the returned messages are sorted by timestamp in the descending order.
    fn get_broadcast_messages_reverse(
        &self,
        before_cursor: Option<&Cursor>,
        limit: usize,
    ) -> Vec<BroadcastMessageWithTimestamp>;

    fn query_broadcast_messages<I: IntoIterator<Item = BroadcastMessageQuery>>(
        &self,
        queries: I,
    ) -> (Vec<BroadcastMessageWithTimestamp>, Vec<u16>) {
        let mut results = Vec::new();
        let mut missing = Vec::new();
        for (index, query) in queries.into_iter().enumerate() {
            if let Some(message) = self.query_broadcast_message(query) {
                results.push(message);
            } else {
                missing.push(index as u16);
            }
        }
        (results, missing)
    }

    fn query_broadcast_message(
        &self,
        query: BroadcastMessageQuery,
    ) -> Option<BroadcastMessageWithTimestamp> {
        match query.flags {
            BroadcastMessageQueryFlags::ChannelAnnouncement => self
                .get_latest_channel_announcement(&query.channel_outpoint)
                .map(|(timestamp, channel_announcement)| {
                    BroadcastMessageWithTimestamp::ChannelAnnouncement(
                        timestamp,
                        channel_announcement,
                    )
                }),
            BroadcastMessageQueryFlags::ChannelUpdateOfNode1 => self
                .get_latest_channel_update(&query.channel_outpoint, true)
                .map(BroadcastMessageWithTimestamp::ChannelUpdate),
            BroadcastMessageQueryFlags::ChannelUpdateOfNode2 => self
                .get_latest_channel_update(&query.channel_outpoint, false)
                .map(BroadcastMessageWithTimestamp::ChannelUpdate),

            BroadcastMessageQueryFlags::NodeAnnouncementNode1
            | BroadcastMessageQueryFlags::NodeAnnouncementNode2 => self
                .get_latest_channel_announcement(&query.channel_outpoint)
                .and_then(|(_, channel_announcement)| {
                    let node = if query.flags == BroadcastMessageQueryFlags::NodeAnnouncementNode1 {
                        &channel_announcement.node1_id
                    } else {
                        &channel_announcement.node2_id
                    };
                    self.get_latest_node_announcement(node)
                        .map(BroadcastMessageWithTimestamp::NodeAnnouncement)
                }),
        }
    }

    fn get_broadcast_message_with_cursor(
        &self,
        cursor: &Cursor,
    ) -> Option<BroadcastMessageWithTimestamp>;

    fn get_latest_broadcast_message_cursor(&self) -> Option<Cursor>;

    fn get_latest_channel_announcement_timestamp(&self, outpoint: &OutPoint) -> Option<u64>;

    fn get_latest_channel_update_timestamp(
        &self,
        outpoint: &OutPoint,
        is_node1: bool,
    ) -> Option<u64>;

    fn get_latest_node_announcement_timestamp(&self, pk: &Pubkey) -> Option<u64>;

    fn get_latest_channel_announcement(
        &self,
        outpoint: &OutPoint,
    ) -> Option<(u64, ChannelAnnouncement)> {
        self.get_latest_channel_announcement_timestamp(outpoint)
            .and_then(|timestamp| {
                 self.get_broadcast_message_with_cursor(&Cursor::new(
                    timestamp,
                    BroadcastMessageID::ChannelAnnouncement(outpoint.clone()),
                )).map(|message| match message {
                    BroadcastMessageWithTimestamp::ChannelAnnouncement(
                        _,
                        channel_announcement,
                    ) => (timestamp, channel_announcement),
                    _ => panic!(
                        "get_latest_channel_announcement returned non-ChannelAnnouncement message from db: channel outpoint {:?}, message {:?}", outpoint, message
                    ),
                })
            })
    }

    fn get_latest_channel_update(
        &self,
        outpoint: &OutPoint,
        is_node1: bool,
    ) -> Option<ChannelUpdate> {
        self.get_latest_channel_update_timestamp(outpoint, is_node1)
            .and_then(|timestamp| {
                 self.get_broadcast_message_with_cursor(&Cursor::new(
                    timestamp,
                    BroadcastMessageID::ChannelUpdate(outpoint.clone()),
                )).map(|message| match message {
                    BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => channel_update,
                    _ => panic!("get_latest_channel_update returned non-ChannelUpdate message from db: channel outpoint {:?}, is_node1 {:?}, message {:?}", outpoint, is_node1, message),
                })
            })
    }

    fn get_latest_node_announcement(&self, pk: &Pubkey) -> Option<NodeAnnouncement> {
        self.get_latest_node_announcement_timestamp(pk).and_then(|timestamp| {
            self.get_broadcast_message_with_cursor(&Cursor::new(
                timestamp,
                BroadcastMessageID::NodeAnnouncement(*pk),
            )).map(|message|
                    match message {
                    BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => node_announcement,
                    _ => panic!("get_latest_node_announcement returned non-NodeAnnouncement message from db: pk {:?}, message {:?}", pk, message),
                    }
                )
            }
        )
    }

    fn delete_broadcast_message(&self, cursor: &Cursor);

    fn save_channel_announcement(&self, timestamp: u64, channel_announcement: ChannelAnnouncement);

    fn save_channel_update(&self, channel_update: ChannelUpdate);

    fn save_node_announcement(&self, node_announcement: NodeAnnouncement);

    fn get_channel_timestamps_iter(&self) -> impl IntoIterator<Item = (OutPoint, [u64; 3])>;

    fn delete_channel_timestamps(&self, outpoint: &OutPoint);
}

fn is_remote_cursor_candidate<S: GossipMessageStore>(
    store: &S,
    local_pubkey: &Pubkey,
    message: &BroadcastMessageWithTimestamp,
) -> bool {
    match message {
        BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
            node_announcement.node_id != *local_pubkey
        }
        BroadcastMessageWithTimestamp::ChannelAnnouncement(_, channel_announcement) => {
            channel_announcement.node1_id != *local_pubkey
                && channel_announcement.node2_id != *local_pubkey
        }
        BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => store
            .get_latest_channel_announcement(&channel_update.channel_outpoint)
            .map(|(_, channel_announcement)| {
                channel_announcement.node1_id != *local_pubkey
                    && channel_announcement.node2_id != *local_pubkey
            })
            .unwrap_or(false),
    }
}

pub(crate) fn get_latest_startup_broadcast_message_cursor<S: GossipMessageStore>(
    store: &S,
    local_pubkey: Option<&Pubkey>,
) -> Cursor {
    match local_pubkey {
        Some(local_pubkey) => {
            // TODO: Support lazy load of broadcast messages, know we just add a limit to the query to avoid loading too many messages into memory.
            store
                .get_broadcast_messages_reverse(None, 100)
                .into_iter()
                .filter(|message| is_remote_cursor_candidate(store, local_pubkey, message))
                .map(|message| message.cursor())
                .next()
                .unwrap_or_default()
        }
        None => store
            .get_latest_broadcast_message_cursor()
            .unwrap_or_default(),
    }
}

// A batch of gossip messages has been added to the store since the last time
// we pulled new messages/messages are pushed to us.
#[derive(Clone, Debug)]
pub struct GossipMessageUpdates {
    pub messages: Vec<BroadcastMessageWithTimestamp>,
}

impl GossipMessageUpdates {
    pub fn new(messages: Vec<BroadcastMessageWithTimestamp>) -> Self {
        Self { messages }
    }

    fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn create_broadcast_messages_filter_result(&self) -> Option<BroadcastMessagesFilterResult> {
        (!self.is_empty()).then_some(BroadcastMessagesFilterResult {
            messages: self.messages.iter().map(|m| m.clone().into()).collect(),
        })
    }
}

/// New messages will be added to the store every now and then.
/// These messages are not guaranteed to be saved to the store in order.
/// This trait provides a way to subscribe to the updates of the gossip message store.
/// The subscriber will receive a batch of messages that are added to the store since the last time
/// we sent messages to the subscriber.
#[async_trait::async_trait]
pub trait SubscribableGossipMessageStore {
    type Subscription;
    type Error: std::error::Error;

    /// Initialize a subscription for gossip message updates, the receiver will receive a batch of
    /// messages that are added to the store since the last time we sent messages to the receiver.
    /// These messages are first processed by the converter. When it is unideal to send messages to
    /// the receiver the converter should return a None, otherwise it can return some message of type
    /// TReceiverMsg, which would then be sent to the receiver actor.
    /// The cursor here specifies the starting point of the subscription.
    /// If there are already some messages in the store that are newer than the cursor, the receiver
    /// will receive these messages immediately after the subscription is created.
    /// Note that the messages are not guaranteed to be sent in ascending order of timestamp,
    /// or in the order of logic dependency (e.g. ChannelAnnouncement is sent before ChannelUpdate).
    /// But we guarantee that the dependencies of the messages will eventually be sent out.
    /// Or if an message is sent to the receiver, all its dependencies are already saved to the store.
    /// We only make this weak guarantee because it simplifies the implementation a lot.
    /// The above weak guarantee is enough for two of the use cases of this subscription:
    /// 1. Sending newer gossip messages to peers via BroadcastMessageFilterResult.
    /// 2. Updating graph data structures (e.g. ChannelManager) with the latest gossip messages.
    /// For the first use case, all the messages are cached first, so we won't directly save a
    /// message without unmet dependencies to the store. For the second use case, we can always
    /// read their dependencies from the store.
    async fn subscribe<
        TReceiverMsg: ractor::Message,
        F: Fn(GossipMessageUpdates) -> Option<TReceiverMsg> + Send + 'static,
    >(
        &self,
        cursor: Cursor,
        receiver: ActorRef<TReceiverMsg>,
        converter: F,
    ) -> Result<Self::Subscription, Self::Error>;

    /// Update the subscription to the gossip message store updates. The subscription parameter is the
    /// return value of the subscribe function. The new cursor will be used to determine the
    /// starting point of the next batch of messages that will be sent to the receiver.
    async fn update_subscription(
        &self,
        subscription: &Self::Subscription,
        cursor: Cursor,
    ) -> Result<(), Self::Error>;

    // Unsubscribe from the gossip message store updates. The subscription parameter is the return value
    // of the subscribe function. After this function is called, the receiver will no longer
    // receive messages from the store.
    async fn unsubscribe(&self, subscription: &Self::Subscription) -> Result<(), Self::Error>;
}

#[derive(AsRefStr)]
pub(crate) enum GossipActorMessage {
    // The control for the service async control is received.
    ReceivedControl(ServiceAsyncControl),

    // Network events to be processed by this actor.
    PeerConnected(Pubkey, SessionContext),
    PeerDisconnected(Pubkey, SessionContext),

    // The function of TickNetworkMaintenance is to maintain the network state.
    // Currently it will do the following things:
    // 1. Check if we have sufficient number of peers to receive broadcasts. If not, send more BroadcastMessageFilter.
    // 2. Check if there are any pending broadcast message queries. If so, broadcast them to the network.
    TickNetworkMaintenance,

    // Some channels in the gossip store haven't been updated for a long time. We will prune all
    // messages older than the given u64 milliseconds timestamp.
    PruneStaleGossipMessages(u64),

    // The active syncing process is finished for a peer.
    ActiveSyncingFinished(Pubkey, Cursor),

    // A malicious peer is found during gossip validation or policy checks.
    MaliciousPeerFound {
        peer: Pubkey,
        violation: GossipViolation,
    },

    // Query some broadcast messages from a peer.
    QueryBroadcastMessages(
        Pubkey,
        Vec<BroadcastMessageQuery>,
        RpcReplyPort<Result<QueryBroadcastMessagesResult, GossipError>>,
    ),
    // The querying of broadcast messages from a peer has timed out.
    QueryBroadcastMessagesTimeout(Pubkey, u64),
    // Try to broadcast BroadcastMessage created by us to the network.
    // We will save and broadcast the messages. Note that we don't check the dependencies of
    // these messages because we assume that the messages created by us are always valid.
    TryBroadcastMessages(Vec<BroadcastMessageWithTimestamp>),
    // Send gossip message to a peer.
    SendGossipMessage(GossipMessageWithTarget),
    // Flush reserved outbound gossip payloads whose rate-limit delay has elapsed.
    FlushDelayedOutbound,
    // Update peer filter
    UpdatePeerFilter(Pubkey, Cursor),
    // Received GossipMessage from a peer
    GossipMessageReceived(GossipMessageWithTarget),
}

pub(crate) fn get_gossip_actor_name(pubkey: &Pubkey) -> String {
    format!("gossip actor {pubkey:?}")
}

pub struct GossipConfig {
    pub(crate) pubkey: Option<Pubkey>,
    pub(crate) gossip_network_maintenance_interval: Duration,
    pub(crate) gossip_store_maintenance_interval: Duration,
    pub(crate) gossip_store_prune_interval: Duration,
    pub(crate) announce_private_addr: bool,
    pub(crate) num_targeted_active_syncing_peers: usize,
    pub(crate) num_targeted_outbound_passive_syncing_peers: usize,
    pub(crate) policy: GossipPolicyConfig,
}

impl Default for GossipConfig {
    fn default() -> Self {
        Self {
            pubkey: None,
            gossip_network_maintenance_interval: Duration::from_millis(
                DEFAULT_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS,
            ),
            gossip_store_maintenance_interval: Duration::from_millis(
                DEFAULT_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS,
            ),
            gossip_store_prune_interval: PRUNE_STALE_BROADCAST_MESSAGES_INTERVAL,
            announce_private_addr: true,
            num_targeted_active_syncing_peers: MAX_NUM_OF_ACTIVE_SYNCING_PEERS,
            num_targeted_outbound_passive_syncing_peers: MIN_NUM_OF_PASSIVE_SYNCING_PEERS,
            policy: GossipPolicyConfig::default(),
        }
    }
}

impl From<&FiberConfig> for GossipConfig {
    fn from(config: &FiberConfig) -> Self {
        Self {
            pubkey: None,
            gossip_network_maintenance_interval: Duration::from_millis(
                config.gossip_network_maintenance_interval_ms(),
            ),
            gossip_store_maintenance_interval: Duration::from_millis(
                config.gossip_store_maintenance_interval_ms(),
            ),
            gossip_store_prune_interval: PRUNE_STALE_BROADCAST_MESSAGES_INTERVAL,
            announce_private_addr: config.announce_private_addr(),
            num_targeted_active_syncing_peers: config
                .gossip_network_num_targeted_active_syncing_peers
                .unwrap_or(MAX_NUM_OF_ACTIVE_SYNCING_PEERS),
            num_targeted_outbound_passive_syncing_peers: config
                .gossip_network_num_targeted_outbound_passive_syncing_peers
                .unwrap_or(MIN_NUM_OF_PASSIVE_SYNCING_PEERS),
            policy: config.gossip_policy.clone(),
        }
    }
}

pub struct GossipService<S, C> {
    extended_store: ExtendedGossipMessageStore<S>,
    _marker: PhantomData<C>,
}

impl<S, C> GossipService<S, C>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    pub(crate) async fn start(
        gossip_config: GossipConfig,
        store: S,
        chain_actor: ActorRef<CkbChainMessage>,
        chain_client: C,
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        peer_channel_index: PeerChannelIndex,
        supervisor: ActorCell,
    ) -> (Self, GossipProtocolHandle) {
        let GossipConfig {
            pubkey,
            gossip_network_maintenance_interval,
            gossip_store_maintenance_interval,
            gossip_store_prune_interval,
            announce_private_addr,
            num_targeted_active_syncing_peers,
            num_targeted_outbound_passive_syncing_peers,
            policy,
        } = gossip_config;

        let (network_control_sender, network_control_receiver) = oneshot::channel();

        let (store_sender, store_receiver) = oneshot::channel();

        let actor_name = pubkey.as_ref().map(get_gossip_actor_name);
        let (actor, _handle) = ActorRuntime::spawn_linked_instant(
            actor_name,
            GossipActor::new(),
            (
                pubkey,
                network_control_receiver,
                store_sender,
                gossip_network_maintenance_interval,
                gossip_store_maintenance_interval,
                gossip_store_prune_interval,
                announce_private_addr,
                num_targeted_active_syncing_peers,
                num_targeted_outbound_passive_syncing_peers,
                policy,
                store,
                chain_actor,
                chain_client,
                network_actor,
                peer_channel_index,
            ),
            supervisor,
        )
        .expect("start gossip actor");
        let store = store_receiver.await.expect("receive store");
        (
            Self {
                extended_store: store,
                _marker: std::marker::PhantomData,
            },
            GossipProtocolHandle::new(actor, network_control_sender),
        )
    }

    #[cfg(test)]
    pub(crate) fn get_extended_actor(&self) -> &ActorRef<ExtendedGossipMessageStoreMessage> {
        &self.extended_store.actor
    }

    #[cfg(test)]
    pub(crate) fn get_store(&self) -> &S {
        &self.extended_store.store
    }

    pub(crate) fn get_subscriber(&self) -> impl SubscribableGossipMessageStore {
        self.extended_store.clone()
    }
}

pub(crate) struct GossipActor<S, C> {
    _phantom: std::marker::PhantomData<(S, C)>,
}

impl<S, C> GossipActor<S, C>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    fn new() -> Self {
        Self {
            _phantom: Default::default(),
        }
    }

    async fn update_peer_filter(
        &self,
        state: &mut GossipActorState<S, C>,
        pubkey: &Pubkey,
        after_cursor: &Cursor,
        myself: ActorRef<GossipActorMessage>,
    ) {
        match state.peer_states.get_mut(pubkey) {
            Some(peer_state) => {
                if let Some(filter_processor) = peer_state.filter_processor.as_mut() {
                    filter_processor.update_filter(after_cursor);
                    return;
                }
                let filter_processor = match PeerFilterProcessor::new(
                    state.store.clone(),
                    *pubkey,
                    after_cursor.clone(),
                    myself.clone(),
                )
                .await
                {
                    Ok(processor) => processor,
                    Err(error) => {
                        debug!(
                            "Failed to start peer filter processor actor {:?}, error: {:?}. maybe the previous actor haven't terminated, retry later",
                            pubkey, error
                        );
                        let retry_peer = *pubkey;
                        let retry_cursor = after_cursor.clone();
                        myself.send_after(UPDATE_PEER_FILTER_RETRY_DELAY, move || {
                            GossipActorMessage::UpdatePeerFilter(retry_peer, retry_cursor)
                        });
                        return;
                    }
                };
                peer_state.filter_processor = Some(filter_processor);
                // Also start passive syncer to peer so that we have less silos.
                if peer_state.sync_status.can_start_passive_syncing() {
                    state.start_passive_syncer(pubkey).await;
                }
            }
            None => {
                warn!(
                    "Received BroadcastMessagesFilter from unknown peer: {:?}",
                    pubkey
                );
            }
        };
    }
}

#[derive(Debug, Default)]
struct SyncingPeerState {
    failed_times: usize,
}

#[derive(Clone, Debug)]
struct DelayedGossipMessage {
    target: Pubkey,
    message: GossipMessage,
    reserved_bytes: u64,
    ready_at_ms: u64,
}

#[derive(Debug, Default)]
struct DelayedGossipMessageQueue {
    per_peer_capacity: usize,
    per_peer_counts: HashMap<Pubkey, usize>,
    messages: VecDeque<DelayedGossipMessage>,
}

impl DelayedGossipMessageQueue {
    fn new(capacity: usize) -> Self {
        Self {
            per_peer_capacity: capacity,
            per_peer_counts: HashMap::new(),
            messages: VecDeque::new(),
        }
    }

    fn push(&mut self, message: DelayedGossipMessage) -> bool {
        if self.is_full_for_peer(&message.target) {
            return false;
        }
        *self.per_peer_counts.entry(message.target).or_default() += 1;
        self.messages.push_back(message);
        true
    }

    fn is_full_for_peer(&self, peer: &Pubkey) -> bool {
        self.per_peer_counts.get(peer).copied().unwrap_or_default() >= self.per_peer_capacity
    }

    fn next_ready_at_ms(&self) -> Option<u64> {
        self.messages
            .iter()
            .map(|message| message.ready_at_ms)
            .min()
    }

    fn drain_ready(&mut self, now_ms: u64) -> Vec<DelayedGossipMessage> {
        let mut ready = Vec::new();
        let mut pending = VecDeque::with_capacity(self.messages.len());
        while let Some(message) = self.messages.pop_front() {
            if message.ready_at_ms <= now_ms {
                self.decrement_peer_count(&message.target);
                ready.push(message);
            } else {
                pending.push_back(message);
            }
        }
        self.messages = pending;
        ready
    }

    fn remove_peer(&mut self, peer: &Pubkey) -> Vec<DelayedGossipMessage> {
        let mut removed = Vec::new();
        let mut retained = VecDeque::with_capacity(self.messages.len());
        while let Some(message) = self.messages.pop_front() {
            if &message.target == peer {
                removed.push(message);
            } else {
                retained.push_back(message);
            }
        }
        self.messages = retained;
        self.per_peer_counts.remove(peer);
        removed
    }

    fn decrement_peer_count(&mut self, peer: &Pubkey) {
        let Some(count) = self.per_peer_counts.get_mut(peer) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.per_peer_counts.remove(peer);
        }
    }
}

pub struct GossipSyncingActorState<S, C> {
    peer_pubkey: Pubkey,
    gossip_actor: ActorRef<GossipActorMessage>,
    chain_actor: ActorRef<CkbChainMessage>,
    chain_client: C,
    store: ExtendedGossipMessageStore<S>,
    // The problem of using the cursor from the store is that a malicious peer may only
    // send large cursor to us, which may cause us to miss some messages.
    // So we decide to keep a cursor for each peer.
    // Of course, using different cursor for different peers will waste
    // some bandwidth by requesting the same messages from different peers.
    cursor: Cursor,
    peer_state: SyncingPeerState,
    request_id: u64,
    inflight_requests:
        HashMap<u64, JoinHandle<Result<(), MessagingErr<GossipSyncingActorMessage>>>>,
}

impl<S, C> GossipSyncingActorState<S, C> {
    fn new(
        peer_pubkey: Pubkey,
        gossip_actor: ActorRef<GossipActorMessage>,
        chain_actor: ActorRef<CkbChainMessage>,
        chain_client: C,
        store: ExtendedGossipMessageStore<S>,
        cursor: Cursor,
    ) -> Self {
        Self {
            peer_pubkey,
            gossip_actor,
            chain_actor,
            chain_client,
            store,
            cursor,
            peer_state: Default::default(),
            inflight_requests: Default::default(),
            request_id: 0,
        }
    }

    fn get_cursor(&self) -> &Cursor {
        &self.cursor
    }

    fn get_and_increment_request_id(&mut self) -> u64 {
        let id = self.request_id;
        self.request_id += 1;
        id
    }
}

pub(crate) struct GossipSyncingActor<S, C> {
    _phantom: std::marker::PhantomData<(S, C)>,
}

impl<S, C> GossipSyncingActor<S, C> {
    fn new() -> Self {
        Self {
            _phantom: Default::default(),
        }
    }
}

#[derive(AsRefStr)]
pub(crate) enum GossipSyncingActorMessage {
    // A GetBroadcastMessages request to the syncing peer has timed out.
    RequestTimeout(u64),
    // A GetBroadcastMessagesResult response from the syncing peer.
    ResponseReceived(GetBroadcastMessagesResult),
    // Initiate a new GetBroadcastMessages request.
    // Mostly triggered by a timeout or a response received.
    NewGetRequest(),
}

#[async_trait::async_trait]
impl<S, C> Actor for GossipSyncingActor<S, C>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    type Msg = GossipSyncingActorMessage;
    type State = GossipSyncingActorState<S, C>;
    type Arguments = (
        Pubkey,
        ActorRef<GossipActorMessage>,
        ActorRef<CkbChainMessage>,
        C,
        ExtendedGossipMessageStore<S>,
        Cursor,
    );

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (peer_pubkey, gossip_actor, chain_actor, chain_client, store, cursor): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself
            .send_message(GossipSyncingActorMessage::NewGetRequest())
            .expect("gossip syncing actor alive");
        Ok(GossipSyncingActorState::new(
            peer_pubkey,
            gossip_actor,
            chain_actor,
            chain_client,
            store,
            cursor,
        ))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _handle_log_guard = ActorHandleLogGuard::new(
            "GossipStoreActor",
            message.as_ref().to_string(),
            "fiber.gossip_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            GossipSyncingActorMessage::RequestTimeout(request_id) => {
                state.inflight_requests.remove(&request_id);
                // TODO: When the peer failed for too many times, we should consider disconnecting from the peer.
                state.peer_state.failed_times += 1;
                observe_active_sync_timeout();
                myself
                    .send_message(GossipSyncingActorMessage::NewGetRequest())
                    .expect("gossip syncing actor alive");
            }
            GossipSyncingActorMessage::ResponseReceived(result) => {
                trace!(
                    "Received GetBroadcastMessages response from peer {:?}: {:?}",
                    &state.peer_pubkey,
                    result
                );
                if let Some(handle) = state.inflight_requests.remove(&result.id) {
                    // Stop the timeout notification.
                    handle.abort();
                    let messages = result.messages;
                    let mut retry_cursor_later = false;
                    // If we are receiving an empty response, then the syncing process is finished.
                    match messages.last() {
                        Some(last_message) => {
                            // We need the message timestamp to construct a valid cursor.
                            match get_message_cursor(
                                last_message,
                                &state.store.store,
                                &state.chain_actor,
                                &state.chain_client,
                            )
                            .await
                            {
                                Ok(cursor) => {
                                    state.cursor = cursor;
                                }
                                Err(Error::DeferredChannelAnnouncementVerification(
                                    outpoint,
                                    reason,
                                )) => {
                                    warn!(
                                        peer = format!("{:?}", &state.peer_pubkey),
                                        outpoint = format!("{:?}", outpoint),
                                        reason,
                                        "Deferring active sync cursor advancement until channel announcement can be verified locally"
                                    );
                                    retry_cursor_later = true;
                                }
                                Err(error) => {
                                    warn!(
                                        "Failed to verify the last message in the response: message {:?}, peer {:?}",
                                        error, &state.peer_pubkey
                                    );
                                    myself.stop(Some(
                                        "Failed to verify the last message in the response"
                                            .to_string(),
                                    ));
                                    state
                                        .gossip_actor
                                        .send_message(GossipActorMessage::MaliciousPeerFound {
                                            peer: state.peer_pubkey,
                                            violation: GossipViolation::InvalidSyncResponse,
                                        })
                                        .expect("gossip actor alive");
                                    return Ok(());
                                }
                            }
                        }
                        None => {
                            state
                                .gossip_actor
                                .send_message(GossipActorMessage::ActiveSyncingFinished(
                                    state.peer_pubkey,
                                    state.cursor.clone(),
                                ))
                                .expect("gossip actor alive");
                            myself.stop(Some("Active syncing finished".to_string()));
                            return Ok(());
                        }
                    }

                    state
                        .store
                        .actor
                        .send_message(ExtendedGossipMessageStoreMessage::SaveMessages(
                            state.peer_pubkey,
                            messages,
                        ))
                        .expect("store actor alive");
                    trace!(
                        "Sending new GetBroadcastMessages request after receiving response: pubkey {:?}",
                        &state.peer_pubkey
                    );
                    if retry_cursor_later {
                        std::mem::drop(myself.send_after(DEFERRED_SYNC_CURSOR_RETRY_DELAY, || {
                            GossipSyncingActorMessage::NewGetRequest()
                        }));
                    } else {
                        myself
                            .send_message(GossipSyncingActorMessage::NewGetRequest())
                            .expect("gossip syncing actor alive");
                    }
                } else {
                    warn!(
                        "Received GetBroadcastMessages response from peer {:?} with unknown request id: {:?}",
                        state.peer_pubkey, result
                    );
                }
            }
            GossipSyncingActorMessage::NewGetRequest() => {
                let latest_cursor = state.get_cursor().clone();
                let request_id = state.get_and_increment_request_id();
                trace!(
                    "Sending GetBroadcastMessages request to peers: request_id {}, latest_cursor {:?}",
                    request_id, latest_cursor
                );
                let request = GossipMessage::GetBroadcastMessages(GetBroadcastMessages {
                    id: request_id,
                    chain_hash: get_chain_hash(),
                    after_cursor: latest_cursor,
                    count: DEFAULT_NUM_OF_BROADCAST_MESSAGE,
                });
                // Send a new GetBroadcastMessages request to the newly-connected peer.
                // If we have less than NUM_SIMULTANEOUS_GET_REQUESTS requests inflight.
                if state.inflight_requests.len() > NUM_SIMULTANEOUS_GET_REQUESTS {
                    return Ok(());
                }
                state
                    .gossip_actor
                    .send_message(GossipActorMessage::SendGossipMessage(
                        GossipMessageWithTarget {
                            target: state.peer_pubkey,
                            message: request,
                        },
                    ))
                    .expect("gossip actor alive");
                // Send a timeout message to myself after 20 seconds, which will then send another GetRequest.
                let handle = myself.send_after(GET_REQUEST_TIMEOUT, move || {
                    GossipSyncingActorMessage::RequestTimeout(request_id)
                });
                // If the request with the same request_id is completed before the timeout,
                // we will use this handle to cancel the timeout notification.
                state.inflight_requests.insert(request_id, handle);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct PeerFilterProcessor {
    // It is sometimes useful to get the filter from the processor (e.g.
    // when we need to actively send a message to the peer).
    filter: Cursor,
    // The actor which watches the store updates and sends corresponding messages to the peer.
    actor: ActorRef<PeerFilterProcessorMessage>,
}

impl PeerFilterProcessor {
    async fn new<S>(
        store: S,
        peer: Pubkey,
        filter: Cursor,
        gossip_actor: ActorRef<GossipActorMessage>,
    ) -> Result<Self, ActorProcessingErr>
    where
        S: SubscribableGossipMessageStore + Clone + Send + Sync + 'static,
        S::Subscription: Send,
    {
        let supervisor = gossip_actor.get_cell();
        let (actor, _) = Actor::spawn_linked(
            Some(format!(
                "peer filter actor for peer {:?} supervised by {:?}",
                &peer,
                gossip_actor.get_id(),
            )),
            PeerFilterActor {
                store,
                peer,
                gossip_actor,
            },
            filter.clone(),
            supervisor,
        )
        .await?;
        Ok(Self { filter, actor })
    }

    fn update_filter(&mut self, filter: &Cursor) {
        self.filter = filter.clone();
        self.actor
            .send_message(PeerFilterProcessorMessage::UpdateFilter(filter.clone()))
            .expect("peer filter processor actor alive");
    }
}

struct PeerFilterActor<S> {
    store: S,
    peer: Pubkey,
    gossip_actor: ActorRef<GossipActorMessage>,
}

enum PeerFilterProcessorMessage {
    NewStoreUpdates(GossipMessageUpdates),
    UpdateFilter(Cursor),
}

#[async_trait::async_trait]
impl<S> Actor for PeerFilterActor<S>
where
    S: SubscribableGossipMessageStore + Clone + Send + Sync + 'static,
    S::Subscription: Send,
{
    type Msg = PeerFilterProcessorMessage;
    type State = S::Subscription;
    type Arguments = Cursor;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        filter_cursor: Cursor,
    ) -> Result<Self::State, ActorProcessingErr> {
        let subscription = self
            .store
            .subscribe(filter_cursor, myself, |m| {
                Some(PeerFilterProcessorMessage::NewStoreUpdates(m))
            })
            .await
            .expect("subscribe store updates");
        Ok(subscription)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        subscription: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            PeerFilterProcessorMessage::NewStoreUpdates(updates) => {
                if let Some(result) = updates.create_broadcast_messages_filter_result() {
                    self.gossip_actor
                        .send_message(GossipActorMessage::SendGossipMessage(
                            GossipMessageWithTarget {
                                target: self.peer,
                                message: GossipMessage::BroadcastMessagesFilterResult(result),
                            },
                        ))
                        .expect("gossip actor alive");
                }
            }
            PeerFilterProcessorMessage::UpdateFilter(cursor) => {
                self.store
                    .update_subscription(subscription, cursor)
                    .await
                    .expect("update subscription");
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        subscription: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _ = self.store.unsubscribe(subscription).await;
        Ok(())
    }
}

// The PeerSyncStatus is used to track the syncing status with a peer.
// Typically, on startup, we will actively obtain the latest messages from the peer.
// After the peer returns a empty response for broadcast messages, we deem that we
// are now in sync with the peer. We will then find if we have been in sync with
// enough number of peers. If not, continue above process.
// After we have been in sync with enough number of peers, we will send a
// BroadcastMessageFilter to enough number of peers to passively receive
// updates.
#[derive(Debug)]
#[allow(dead_code)]
enum PeerSyncStatus {
    // We are not syncing with the peer.
    NotSyncing(),
    // We are actively sending GetBroadcastMessages to the peer.
    // The actor here is responsible to get syncing process running.
    ActiveGet(ActorRef<GossipSyncingActorMessage>),
    // We are only passively receiving messages from the peer.
    // The cursor here is the filter that we sent to the peer.
    PassiveFilter(Cursor),
    // We have finished syncing with the peer. The cursor here is the latest cursor
    // that we have received from the peer. The u64 here is the timestamp
    // of the finishing syncing time.
    FinishedActiveSyncing(u64, Cursor),
}

impl PeerSyncStatus {
    fn is_passive_syncing(&self) -> bool {
        matches!(self, PeerSyncStatus::PassiveFilter(_))
    }

    fn is_active_syncing(&self) -> bool {
        matches!(self, PeerSyncStatus::ActiveGet(_))
    }

    fn is_finished_active_syncing(&self) -> bool {
        matches!(self, PeerSyncStatus::FinishedActiveSyncing(_, _))
    }

    fn can_start_active_syncing(&self) -> bool {
        !self.is_active_syncing()
            && !self.is_passive_syncing()
            && !self.is_finished_active_syncing()
    }

    fn can_start_passive_syncing(&self) -> bool {
        !self.is_passive_syncing() && !self.is_active_syncing()
    }
}

impl Default for PeerSyncStatus {
    fn default() -> Self {
        Self::NotSyncing()
    }
}

#[derive(Debug)]
struct PeerState {
    session_id: SessionId,
    session_type: SessionType,
    // The filter is a cursor that the peer sent to us. We will only send messages to the peer
    // that are newer than the cursor. If the peer has not sent us a filter, we will not actively
    // send messages to the peer. If the peer sends us a new filter, we will update this field,
    // and send all the messages after the cursor to the peer immediately.
    filter_processor: Option<PeerFilterProcessor>,
    // The status of the peer syncing.
    sync_status: PeerSyncStatus,
}

impl Drop for PeerState {
    fn drop(&mut self) {
        if let Some(filter_processor) = self.filter_processor.take() {
            filter_processor
                .actor
                .stop(Some("peer state dropped".to_string()));
        }
        if let PeerSyncStatus::ActiveGet(actor) = &self.sync_status {
            actor.stop(Some("peer state dropped".to_string()));
        }
    }
}

impl PeerState {
    fn new(session_id: SessionId, session_type: SessionType) -> Self {
        Self {
            session_id,
            session_type,
            filter_processor: Default::default(),
            sync_status: Default::default(),
        }
    }

    fn change_sync_status(&mut self, new_status: PeerSyncStatus) {
        self.sync_status = new_status;
    }
}

// This ExtendedGossipMessageStore is used to store the gossip messages and their dependencies.
// It enhances the GossipMessageStore trait with the ability to check the dependencies of the messages,
// and occasionally send out messages that are saved out of order (a message with smaller timestamp
// was saved before a message with larger timestamp).
#[derive(Clone)]
pub struct ExtendedGossipMessageStore<S> {
    // It is possible to re-implement all the store functions by message-passing to the actor.
    // But it is tedious to do so. So we just store the store here.
    // We need to get/save broadcast messages from/to the store. We can use this field directly.
    // Be careful while saving messages to the store. We should send SaveMessage message to the actor
    // because we must ask the actor do some bookkeeping work (e.g. check if the dependencies of
    // the message are already saved).
    pub(crate) store: S,
    // The actor that is responsible for book-keep the messages to be saved to the store,
    // and send messages to the subscribers.
    pub(crate) actor: ActorRef<ExtendedGossipMessageStoreMessage>,
}

impl<S> ExtendedGossipMessageStore<S> {
    fn get_store(&self) -> &S {
        &self.store
    }
}

impl<S> ExtendedGossipMessageStore<S>
where
    S: GossipMessageStore + Send + Sync + Clone + 'static,
{
    #[allow(clippy::too_many_arguments)]
    async fn new<C: CkbChainClient + Clone + Send + Sync + 'static>(
        maintenance_interval: Duration,
        announce_private_addr: bool,
        inbound_channel_update: ChannelUpdateRateLimitConfig,
        store: S,
        gossip_actor: ActorRef<GossipActorMessage>,
        latest_remote_broadcast_timestamp: Arc<AtomicU64>,
        chain_actor: ActorRef<CkbChainMessage>,
        chain_client: C,
        supervisor: ActorCell,
    ) -> Self {
        let (actor, _) = Actor::spawn_linked(
            Some(format!(
                "gossip message store actor supervised by {:?}",
                supervisor.get_id()
            )),
            ExtendedGossipMessageStoreActor::new(),
            (
                maintenance_interval,
                announce_private_addr,
                inbound_channel_update,
                store.clone(),
                gossip_actor,
                latest_remote_broadcast_timestamp.clone(),
                chain_actor,
                chain_client.clone(),
            ),
            supervisor,
        )
        .await
        .expect("start gossip message actor store");

        Self { store, actor }
    }
}

#[async_trait::async_trait]
impl<S: GossipMessageStore + Sync> SubscribableGossipMessageStore
    for ExtendedGossipMessageStore<S>
{
    type Subscription = u64;
    type Error = Error;

    async fn subscribe<
        TReceiverMsg: ractor::Message,
        F: Fn(GossipMessageUpdates) -> Option<TReceiverMsg> + Send + 'static,
    >(
        &self,
        cursor: Cursor,
        receiver: ActorRef<TReceiverMsg>,
        converter: F,
    ) -> Result<Self::Subscription, Self::Error> {
        match call!(
            &self.actor,
            ExtendedGossipMessageStoreMessage::NewSubscription,
            cursor
        ) {
            Ok((subscription, tx, output_port)) => {
                output_port.subscribe(receiver, converter);
                // It is sometimes possible that some messages are sent to the output_port even
                // before we subscribe to it (in this case we will miss these messages).
                // So we use a channel to notify that we have already subscribed to the output_port,
                // and messages can now be sent to the output_port.
                tx.send(()).expect("notify new subscription ready");
                Ok(subscription)
            }
            Err(e) => Err(Error::InternalError(anyhow::anyhow!(e.to_string()))),
        }
    }

    async fn update_subscription(
        &self,
        subscription: &Self::Subscription,
        cursor: Cursor,
    ) -> Result<(), Self::Error> {
        const DEFAULT_TIMEOUT: u64 = Duration::from_secs(5).as_millis() as u64;
        call_t!(
            &self.actor,
            ExtendedGossipMessageStoreMessage::UpdateSubscription,
            DEFAULT_TIMEOUT,
            *subscription,
            Some(cursor)
        )
        .map_err(|e| Error::InternalError(anyhow::anyhow!(e.to_string())))
    }

    async fn unsubscribe(&self, subscription: &Self::Subscription) -> Result<(), Self::Error> {
        const DEFAULT_TIMEOUT: u64 = Duration::from_secs(5).as_millis() as u64;
        call_t!(
            &self.actor,
            ExtendedGossipMessageStoreMessage::UpdateSubscription,
            DEFAULT_TIMEOUT,
            *subscription,
            None
        )
        .map_err(|e| Error::InternalError(anyhow::anyhow!(e.to_string())))
    }
}

struct BroadcastMessageOutput {
    // The filter that a subscriber has set.
    filter: Cursor,
    // A port that from which the subscriber will receive messages and from which we will send messages to the subscriber.
    output_port: Arc<OutputPort<GossipMessageUpdates>>,
}

impl BroadcastMessageOutput {
    fn new(filter: Cursor, output_port: Arc<OutputPort<GossipMessageUpdates>>) -> Self {
        Self {
            filter,
            output_port,
        }
    }
}

#[derive(thiserror::Error, Debug)]

pub enum GossipError {
    #[error("Request timeout")]
    Timeout,
    #[error("Failed to send message: {0:?}")]
    FailedToSendMessage(Error),
}

// This is the error type for processing the gossip messages.
// Sometimes we can't be very sure of if a gossip message is valid on a first sight.
// For example, a ChannelUpdate message may be signed with a different node's key,
// but we won't be able to find this out until we have the corresponding ChannelAnnouncement.
#[derive(Debug, Clone, thiserror::Error)]
pub enum GossipMessageProcessingError {
    #[error("The message timestamp is too far in the future: expected to before {1}, has {0}")]
    MessageTooNew(u64, u64),
    #[error("Failed to process the message: {0}")]
    ProcessingError(String),
    #[error("A newer message is already saved: {0:?}")]
    NewerMessageSaved(BroadcastMessageWithTimestamp),
}

impl GossipMessageProcessingError {
    fn violation(&self) -> Option<GossipViolation> {
        match self {
            GossipMessageProcessingError::MessageTooNew(_, _)
            | GossipMessageProcessingError::ProcessingError(_) => {
                Some(GossipViolation::PolicyRejectedMessage)
            }
            GossipMessageProcessingError::NewerMessageSaved(_) => None,
        }
    }

    fn rejected_broadcast_metrics_reason(&self) -> &'static str {
        match self {
            GossipMessageProcessingError::MessageTooNew(_, _) => "message_too_new",
            GossipMessageProcessingError::ProcessingError(_) => "processing_error",
            GossipMessageProcessingError::NewerMessageSaved(_) => "newer_message_saved",
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum VerifyBroadcastMessageError {
    #[error("InvalidParameter: {0}")]
    InvalidParameter(String),
    #[error("Deferred channel announcement verification for {0:?}: {1}")]
    DeferredChannelAnnouncementVerification(OutPoint, String),
}

impl VerifyBroadcastMessageError {
    fn rejected_broadcast_metrics_reason(&self) -> &'static str {
        match self {
            VerifyBroadcastMessageError::InvalidParameter(_) => "verify_invalid",
            VerifyBroadcastMessageError::DeferredChannelAnnouncementVerification(_, _) => {
                "verify_deferred_channel_announcement"
            }
        }
    }
}

impl From<VerifyBroadcastMessageError> for Error {
    fn from(error: VerifyBroadcastMessageError) -> Self {
        match error {
            VerifyBroadcastMessageError::InvalidParameter(message) => {
                Error::InvalidParameter(message)
            }
            VerifyBroadcastMessageError::DeferredChannelAnnouncementVerification(
                outpoint,
                message,
            ) => Error::DeferredChannelAnnouncementVerification(outpoint, message),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum InsertMessageStatus {
    Inserted,
    InsertedDuplicate,
    Duplicate,
}

pub struct ExtendedGossipMessageStoreState<S, C> {
    announce_private_addr: bool,
    store: S,
    gossip_actor: ActorRef<GossipActorMessage>,
    channel_update_limiter: ChannelUpdateLimiter,
    latest_remote_broadcast_timestamp: Arc<AtomicU64>,
    chain_actor: ActorRef<CkbChainMessage>,
    chain_client: C,
    next_id: u64,
    output_ports: HashMap<u64, BroadcastMessageOutput>,
    // A map from peer pubkey to the messages that need to be saved.
    // Our own messages are always saved directly to the store.
    messages_to_be_saved: HashMap<Pubkey, HashSet<BroadcastMessage>>,
    num_query_tasks_running: usize,
}

impl<S: GossipMessageStore, C: CkbChainClient> ExtendedGossipMessageStoreState<S, C> {
    fn new(
        announce_private_addr: bool,
        inbound_channel_update: ChannelUpdateRateLimitConfig,
        store: S,
        gossip_actor: ActorRef<GossipActorMessage>,
        latest_remote_broadcast_timestamp: Arc<AtomicU64>,
        chain_actor: ActorRef<CkbChainMessage>,
        chain_client: C,
    ) -> Self {
        Self {
            announce_private_addr,
            store,
            gossip_actor,
            channel_update_limiter: inbound_channel_update.build_limiter(),
            latest_remote_broadcast_timestamp,
            chain_actor,
            chain_client,
            next_id: Default::default(),
            output_ports: Default::default(),
            messages_to_be_saved: Default::default(),
            num_query_tasks_running: 0,
        }
    }

    // Obtaining all the messages whose transitive dependencies are already available,
    // check their validity and then save valid messages to store and
    // return the list of saved messages that can be sent to the subscribers.
    async fn prune_messages_to_be_saved(&mut self) -> Vec<BroadcastMessageWithTimestamp> {
        let mut complete_messages = HashMap::<BroadcastMessage, Vec<Pubkey>>::new();
        for (peer, messages) in &self.messages_to_be_saved {
            for message in messages {
                if self.has_dependencies_available(message) {
                    complete_messages
                        .entry(message.clone())
                        .or_default()
                        .push(*peer);
                }
            }
        }

        let mut complete_messages = complete_messages.into_iter().collect::<Vec<_>>();
        complete_messages.sort_unstable_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));
        trace!(
            "Saving complete messages to the store: {:?}",
            &complete_messages
        );

        let mut verified_sorted_messages = Vec::with_capacity(complete_messages.len());
        for (message, peers) in complete_messages {
            if self.should_defer_message_verification(&message) {
                trace!(
                    "Keeping message pending until dependencies are persisted: {:?}",
                    &message
                );
                continue;
            }
            match verify_and_save_broadcast_message(
                &message,
                &self.store,
                &self.chain_actor,
                &self.chain_client,
            )
            .await
            {
                Ok((verified_message, is_newly_applied)) => {
                    self.remove_pending_message(&message, &peers);
                    if is_newly_applied {
                        self.latest_remote_broadcast_timestamp
                            .fetch_max(verified_message.cursor().timestamp, Ordering::AcqRel);
                        observe_applied_broadcast_message();
                        observe_applied_propagation_latency(&verified_message);
                    } else {
                        observe_duplicate_broadcast_message();
                    }
                    verified_sorted_messages.push(verified_message);
                }
                Err(
                    error @ VerifyBroadcastMessageError::DeferredChannelAnnouncementVerification(
                        _,
                        _,
                    ),
                ) => {
                    trace!(
                        "Deferring retry for message {:?} from peers {:?}: {:?}",
                        message,
                        peers,
                        error
                    );
                }
                Err(error) => {
                    self.remove_pending_message(&message, &peers);
                    observe_rejected_broadcast_message(error.rejected_broadcast_metrics_reason());
                    trace!(
                        "Failed to verify and save message {:?} from peers {:?}: {:?}",
                        message,
                        peers,
                        error
                    );
                    if let Some(violation) =
                        classify_verify_and_save_broadcast_message_error(&message, &error)
                    {
                        for peer in peers {
                            self.gossip_actor
                                .send_message(GossipActorMessage::MaliciousPeerFound {
                                    peer,
                                    violation,
                                })
                                .expect("gossip actor alive");
                        }
                    }
                }
            }
        }
        self.messages_to_be_saved
            .retain(|_, messages| !messages.is_empty());

        verified_sorted_messages
    }

    fn remove_pending_message(&mut self, message: &BroadcastMessage, peers: &[Pubkey]) {
        for peer in peers {
            if let Some(messages) = self.messages_to_be_saved.get_mut(peer) {
                messages.remove(message);
            }
        }
    }

    fn store_messages(&self, messages: &[BroadcastMessageWithTimestamp]) {
        for message in messages {
            match message {
                BroadcastMessageWithTimestamp::ChannelAnnouncement(
                    timestamp,
                    channel_announcement,
                ) => {
                    self.store
                        .save_channel_announcement(*timestamp, channel_announcement.clone());
                }
                BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                    self.store.save_channel_update(channel_update.clone());
                }
                BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                    self.store.save_node_announcement(node_announcement.clone());
                }
            }
        }
    }

    fn broadcast_messages(&self, messages: &[BroadcastMessageWithTimestamp]) {
        if messages.is_empty() {
            return;
        }

        for (id, subscription) in self.output_ports.iter() {
            let messages_to_send = messages
                .iter()
                .filter(|m| m.cursor() > subscription.filter)
                .cloned()
                .collect::<Vec<_>>();
            trace!(
                "Sending complete messages in memory to subscription #{}: number of messages = {}",
                id,
                messages_to_send.len()
            );
            for chunk in messages_to_send.chunks(MAX_NUM_OF_BROADCAST_MESSAGES as usize) {
                subscription
                    .output_port
                    .send(GossipMessageUpdates::new(chunk.to_vec()));
            }
        }
    }

    fn store_and_broadcast_messages(&self, messages: &[BroadcastMessageWithTimestamp]) {
        self.store_messages(messages);
        self.broadcast_messages(messages);
    }

    fn spawn_query_tasks(&mut self, myself: &ActorRef<ExtendedGossipMessageStoreMessage>) {
        if MAX_NUM_CONCURRENT_QUERY_TASKS == self.num_query_tasks_running {
            return;
        }
        let peers_to_query = self
            .messages_to_be_saved
            .keys()
            .filter(|peer| {
                self.messages_to_be_saved
                    .get(peer)
                    .is_some_and(|messages| messages.iter().any(Self::message_requires_query))
            })
            .take(MAX_NUM_CONCURRENT_QUERY_TASKS - self.num_query_tasks_running)
            .cloned()
            .collect::<Vec<_>>();
        self.num_query_tasks_running += peers_to_query.len();

        for peer in peers_to_query {
            // The assumption is that when a peer sends us a message, it should have all the dependencies.
            // So here we will send queries to the peer for the missing messages.
            let incomplete_messages = self
                .messages_to_be_saved
                .remove(&peer)
                .expect("peer is a key of hashmap");
            let (messages_requiring_query, retained_messages): (Vec<_>, Vec<_>) =
                incomplete_messages
                    .into_iter()
                    .partition(Self::message_requires_query);
            if !retained_messages.is_empty() {
                self.messages_to_be_saved
                    .insert(peer, retained_messages.into_iter().collect());
            }
            let gossip_actor = self.gossip_actor.clone();
            let myself = myself.clone();
            let incomplete_messages = messages_requiring_query;

            ractor::concurrency::spawn(async move {
                let mut is_success = true;
                let n_queries = incomplete_messages.len();
                for messages in
                    incomplete_messages.chunks(DEFAULT_NUM_OF_BROADCAST_MESSAGE as usize)
                {
                    let queries = messages
                        .iter()
                        .filter_map(|m| match m {
                            BroadcastMessage::ChannelUpdate(channel_update) => {
                                Some(BroadcastMessageQuery {
                                    flags: BroadcastMessageQueryFlags::ChannelAnnouncement,
                                    channel_outpoint: channel_update.channel_outpoint.clone(),
                                })
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    if queries.is_empty() {
                        continue;
                    }
                    observe_dependency_query_request(queries.len());
                    match call!(
                        gossip_actor,
                        GossipActorMessage::QueryBroadcastMessages,
                        peer,
                        queries.to_vec()
                    ) {
                        Ok(Ok(result)) => {
                            let mut all_messages = result.messages;
                            // We need also to save the incomplete messages to the store.
                            all_messages.extend(messages.iter().map(Clone::clone));
                            myself
                                .send_message(ExtendedGossipMessageStoreMessage::SaveMessages(
                                    peer,
                                    all_messages,
                                ))
                                .expect("actor alive");
                        }
                        Ok(Err(e)) => {
                            error!("Failed to query messages from peer {:?}: {:?}", peer, e);
                            is_success = false;
                        }
                        Err(e) => {
                            error!("Failed to send query to peer {:?}: {:?}", peer, e);
                            is_success = false;
                        }
                    };
                }
                myself
                    .send_message(ExtendedGossipMessageStoreMessage::QueryTaskDone(
                        QueryResult {
                            n_queries,
                            peer,
                            is_success,
                        },
                    ))
                    .expect("actor alive")
            });
        }
    }

    fn get_channel_announcement(&self, outpoint: &OutPoint) -> Option<ChannelAnnouncement> {
        self.store
            .get_latest_channel_announcement(outpoint)
            .map(|(_, m)| m)
            .or_else(|| self.get_channel_announcement_in_memory(outpoint))
    }

    fn get_channel_announcement_in_memory(
        &self,
        outpoint: &OutPoint,
    ) -> Option<ChannelAnnouncement> {
        self.messages_to_be_saved
            .values()
            .flatten()
            .find_map(|m| match m {
                BroadcastMessage::ChannelAnnouncement(channel_announcement)
                    if &channel_announcement.channel_outpoint == outpoint =>
                {
                    Some(channel_announcement.clone())
                }
                _ => None,
            })
    }

    async fn insert_message_to_be_saved_list(
        &mut self,
        pubkey: &Pubkey,
        message: &BroadcastMessage,
    ) -> Result<InsertMessageStatus, GossipMessageProcessingError> {
        let msg_chain_hash = match message {
            BroadcastMessage::NodeAnnouncement(msg) => msg.chain_hash,
            BroadcastMessage::ChannelAnnouncement(msg) => msg.chain_hash,
            BroadcastMessage::ChannelUpdate(msg) => msg.chain_hash,
        };
        if msg_chain_hash != get_chain_hash() {
            return Err(GossipMessageProcessingError::ProcessingError(format!(
                "Invalid chain hash: expected {:?}, got {:?}",
                get_chain_hash(),
                msg_chain_hash
            )));
        }

        if let Some(existing_messages) = self.messages_to_be_saved.get(pubkey) {
            if existing_messages.contains(message) {
                return Ok(InsertMessageStatus::Duplicate);
            }
        }

        let duplicate_from_other_peer = self
            .messages_to_be_saved
            .iter()
            .any(|(peer, messages)| peer != pubkey && messages.contains(message));

        if let Some(existing_message) = get_existing_newer_broadcast_message(message, &self.store) {
            if &BroadcastMessage::from(existing_message.clone()) != message {
                return Err(GossipMessageProcessingError::NewerMessageSaved(
                    existing_message,
                ));
            } else {
                return Ok(InsertMessageStatus::Duplicate);
            }
        }

        if let Some(timestamp) = message.timestamp() {
            let max_acceptable_gossip_message_timestamp = max_acceptable_gossip_message_timestamp();
            if timestamp > max_acceptable_gossip_message_timestamp {
                return Err(GossipMessageProcessingError::MessageTooNew(
                    timestamp,
                    max_acceptable_gossip_message_timestamp,
                ));
            }
        }

        if let BroadcastMessage::NodeAnnouncement(node_announcement) = &message {
            let expected_peer_id = PeerId::from_public_key(&super::types::pubkey_to_tentacle(
                node_announcement.node_id,
            ));
            for addr in &node_announcement.addresses {
                for proto in addr.iter() {
                    if let Protocol::P2P(ref peer_id_bytes) = proto {
                        if peer_id_bytes.as_ref() != expected_peer_id.as_bytes() {
                            return Err(GossipMessageProcessingError::ProcessingError(
                                "NodeAnnouncement address /p2p does not match node_id".to_string(),
                            ));
                        }
                    }
                }
            }
        }

        if !self.announce_private_addr {
            if let BroadcastMessage::NodeAnnouncement(node_announcement) = &message {
                if !node_announcement
                    .addresses
                    .iter()
                    .any(crate::utils::is_addr_reachable)
                {
                    return Err(GossipMessageProcessingError::ProcessingError(
                        "private address node announcement".to_string(),
                    ));
                }
            }
        }

        trace!(
            "New gossip message saved to memory: peer {:?}, message {:?}",
            pubkey,
            message
        );
        self.messages_to_be_saved
            .entry(*pubkey)
            .or_default()
            .insert(message.clone());
        Ok(if duplicate_from_other_peer {
            InsertMessageStatus::InsertedDuplicate
        } else {
            InsertMessageStatus::Inserted
        })
    }

    fn has_dependencies_available(&self, message: &BroadcastMessage) -> bool {
        match message {
            BroadcastMessage::ChannelUpdate(channel_update) => self
                .get_channel_announcement(&channel_update.channel_outpoint)
                .is_some(),
            _ => true,
        }
    }

    fn should_defer_message_verification(&self, message: &BroadcastMessage) -> bool {
        match message {
            BroadcastMessage::ChannelUpdate(channel_update) => {
                self.store
                    .get_latest_channel_announcement(&channel_update.channel_outpoint)
                    .is_none()
                    && self
                        .get_channel_announcement_in_memory(&channel_update.channel_outpoint)
                        .is_some()
            }
            _ => false,
        }
    }

    fn message_requires_query(message: &BroadcastMessage) -> bool {
        matches!(message, BroadcastMessage::ChannelUpdate(_))
    }

    fn is_known_remote_message_replay(&self, peer: &Pubkey, message: &BroadcastMessage) -> bool {
        if self
            .messages_to_be_saved
            .get(peer)
            .is_some_and(|messages| messages.contains(message))
        {
            return true;
        }
        get_existing_broadcast_message(message, &self.store)
            .map(BroadcastMessage::from)
            .is_some_and(|existing| &existing == message)
    }

    fn allow_inbound_remote_broadcast_message(
        &mut self,
        peer: &Pubkey,
        message: &BroadcastMessage,
        now_ms: u64,
    ) -> bool {
        if self.is_known_remote_message_replay(peer, message) {
            return true;
        }

        let BroadcastMessage::ChannelUpdate(channel_update) = message else {
            return true;
        };

        let key = ChannelUpdateLimiterKey::new(
            *peer,
            channel_update.channel_outpoint.clone(),
            channel_update.is_update_of_node_1(),
        );
        let allowed = self.channel_update_limiter.try_acquire(&key, now_ms);
        if !allowed {
            debug!(
                peer = format!("{peer:?}"),
                outpoint = ?channel_update.channel_outpoint,
                is_node1 = channel_update.is_update_of_node_1(),
                "Dropping remote channel update due to gossip rate limiter"
            );
        }
        allowed
    }
}

// An extended gossip message store actor that can handle more complex operations than a normal gossip message store.
// Major features are added to this actor:
// 1). It can stash lagged messages (messages arrived at this node out of order) as as to
// send them to the subscribers eventually.
// 2). It can manage the dependencies of the messages and save them to the store in the correct order,
// which means that the messages in the store is always consistent.
// 3). Used in ExtendedGossipMessageStore, we can subscribe to the updates of the store, which means that
// it is possible to get a consistent view of the store without loading all the messages from the store.
struct ExtendedGossipMessageStoreActor<S, C> {
    phantom: PhantomData<(S, C)>,
}

impl<S: GossipMessageStore, C: CkbChainClient> ExtendedGossipMessageStoreActor<S, C> {
    fn new() -> Self {
        Self {
            phantom: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl<S: GossipMessageStore + Send + Sync + 'static, C: CkbChainClient + Send + Sync + 'static> Actor
    for ExtendedGossipMessageStoreActor<S, C>
{
    type Msg = ExtendedGossipMessageStoreMessage;
    type State = ExtendedGossipMessageStoreState<S, C>;
    type Arguments = (
        Duration,
        bool,
        ChannelUpdateRateLimitConfig,
        S,
        ActorRef<GossipActorMessage>,
        Arc<AtomicU64>,
        ActorRef<CkbChainMessage>,
        C,
    );

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (
            gossip_store_maintenance_interval,
            announce_private_addr,
            inbound_channel_update,
            store,
            gossip_actor,
            latest_remote_broadcast_timestamp,
            chain_actor,
            chain_client,
        ): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_interval(gossip_store_maintenance_interval, || {
            ExtendedGossipMessageStoreMessage::Tick
        });
        Ok(ExtendedGossipMessageStoreState::new(
            announce_private_addr,
            inbound_channel_update,
            store,
            gossip_actor,
            latest_remote_broadcast_timestamp,
            chain_actor,
            chain_client,
        ))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _handle_log_guard = ActorHandleLogGuard::new(
            "ExtendedGossipMessageStoreActor",
            message.as_ref().to_string(),
            "fiber.extended_gossip_message_store_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            ExtendedGossipMessageStoreMessage::NewSubscription(cursor, reply) => {
                trace!(
                    "Creating subscription to the store updates with cursor {:?}",
                    cursor
                );
                let id = state.next_id;
                state.next_id += 1;
                let (tx, rx) = oneshot::channel();
                let output_port = Arc::new(OutputPort::default());
                if let Err(error) = reply.send((id, tx, Arc::clone(&output_port))) {
                    error!(
                        "Failed to send reply to new subscription (has the caller exited?): {:?}",
                        error
                    );
                    return Ok(());
                }
                rx.await.expect("receive notification");
                trace!(
                    "Loading messages from store for subscriber {}: subscription cursor {:?}",
                    id,
                    cursor
                );
                // Since the handling of LoadMessagesFromStore interleaves with the handling of Tick,
                // we may send the messages in an order that is different from both the dependency order
                // and the timestamp order. This means that we may send a ChannelUpdate while handling
                // Tick and later we will send the corresponding ChannelAnnouncement.
                // So the downstream consumer need to either cache some of the messages and wait for the
                // dependent messages to arrive or read the messages from the store directly.
                myself
                    .send_message(ExtendedGossipMessageStoreMessage::LoadMessagesFromStore(
                        id,
                        cursor.clone(),
                    ))
                    .expect("myself alive");
                state.output_ports.insert(
                    id,
                    BroadcastMessageOutput::new(cursor, Arc::clone(&output_port)),
                );
            }

            ExtendedGossipMessageStoreMessage::UpdateSubscription(id, cursor, reply) => {
                trace!(
                    "Updating subscription to store updates for #{} with cursor {:?}",
                    id,
                    cursor
                );

                match cursor {
                    Some(cursor) => {
                        if let Some(output) = state.output_ports.get_mut(&id) {
                            output.filter = cursor;
                        }
                    }
                    _ => {
                        state.output_ports.remove(&id);
                    }
                }
                let _ = reply.send(());
            }

            ExtendedGossipMessageStoreMessage::LoadMessagesFromStore(id, cursor) => {
                let subscription = match state.output_ports.get_mut(&id) {
                    Some(output) => output,
                    // Subscriber has already unsubscribed, early return.
                    None => return Ok(()),
                };
                let messages = state
                    .store
                    .get_broadcast_messages(&cursor, DEFAULT_NUM_OF_BROADCAST_MESSAGE as usize);

                trace!(
                    "Loaded messages for subscription #{} with cursor {:?} (number of messages {:?})",
                    id,
                    cursor,
                    messages.len()
                );
                match messages.last() {
                    Some(m) => {
                        myself
                            .send_message(ExtendedGossipMessageStoreMessage::LoadMessagesFromStore(
                                id,
                                m.cursor(),
                            ))
                            .expect("actor alive");
                        subscription
                            .output_port
                            .send(GossipMessageUpdates::new(messages));
                    }
                    None => {
                        // We have finished initial loading.
                    }
                }
            }

            ExtendedGossipMessageStoreMessage::SaveMessages(peer, messages) => {
                let now_ms = now_timestamp_as_millis_u64();
                for message in messages {
                    if !state.allow_inbound_remote_broadcast_message(&peer, &message, now_ms) {
                        continue;
                    }
                    match state.insert_message_to_be_saved_list(&peer, &message).await {
                        Ok(InsertMessageStatus::Inserted) => {
                            if !state.has_dependencies_available(&message) {
                                observe_missing_dependency_message(broadcast_message_type(
                                    &message,
                                ));
                            }
                        }
                        Ok(InsertMessageStatus::InsertedDuplicate) => {
                            observe_duplicate_broadcast_message();
                        }
                        Ok(InsertMessageStatus::Duplicate) => {
                            observe_duplicate_broadcast_message();
                        }
                        Err(error) => {
                            observe_rejected_broadcast_message(
                                error.rejected_broadcast_metrics_reason(),
                            );
                            trace!("Failed to save message: {:?}, error: {:?}", message, error);
                            if let Some(violation) = error.violation() {
                                state
                                    .gossip_actor
                                    .send_message(GossipActorMessage::MaliciousPeerFound {
                                        peer,
                                        violation,
                                    })
                                    .expect("gossip actor alive");
                            }
                        }
                    }
                }
            }

            ExtendedGossipMessageStoreMessage::SaveAndBroadcastMessages(messages) => {
                state.store_and_broadcast_messages(&messages);
            }

            ExtendedGossipMessageStoreMessage::QueryTaskDone(QueryResult {
                n_queries,
                peer,
                is_success,
            }) => {
                trace!(
                    n_queries = n_queries,
                    peer = format!("{:?}", peer),
                    is_success = is_success,
                    "Querying task done"
                );
                state.num_query_tasks_running -= 1;
            }

            ExtendedGossipMessageStoreMessage::Tick => {
                trace!(
                    "Gossip store maintenance ticked: #subscriptions = {},  #messages_to_be_saved = {}",
                    state.output_ports.len(),
                    state.messages_to_be_saved.values().map(|s| s.len()).sum::<usize>(),
                );

                // These are the messages that have complete dependencies and can be sent to the subscribers.
                let complete_messages = state.prune_messages_to_be_saved().await;
                state.broadcast_messages(&complete_messages);
                state.spawn_query_tasks(&myself);
            }
        }
        Ok(())
    }
}

pub struct QueryResult {
    n_queries: usize,
    peer: Pubkey,
    is_success: bool,
}

#[derive(AsRefStr)]
pub enum ExtendedGossipMessageStoreMessage {
    // A new subscription for gossip message updates. We will send a batch of messages to the subscriber
    // via the returned output port.
    NewSubscription(
        Cursor,
        RpcReplyPort<(
            u64,
            oneshot::Sender<()>, // A channel to notify the subscriber that the subscription is ready.
            Arc<OutputPort<GossipMessageUpdates>>,
        )>,
    ),
    // Update the subscription. If this Option is None, the subscription will be cancelled.
    // Otherwise the new cursor will be used to filter the messages that are sent to the subscriber.
    UpdateSubscription(u64, Option<Cursor>, RpcReplyPort<()>),
    // Save new broadcast messages to the store. The messages will be first saved to the memory,
    // then if all the dependencies are met, they are periodically saved to the store and sent to the subscribers.
    SaveMessages(Pubkey, Vec<BroadcastMessage>),
    // Save new messages to the store, and broadcast them to the subscribers immediately.
    // These messages will not be saved to the memory and wait for the dependencies to be met.
    // We normally use this variant to send our own messages to the subscribers.
    SaveAndBroadcastMessages(Vec<BroadcastMessageWithTimestamp>),
    // Send broadcast messages after the cursor to the subscriber specified in the u64 id.
    // This is normally called immediately after a new subscription is created. This is the time when
    // we need to send existing messages to the subscriber.
    LoadMessagesFromStore(u64, Cursor),
    QueryTaskDone(QueryResult),
    // A tick message that is sent periodically to check if there are any messages that are saved out of order.
    // If there are, we will send them to the subscribers.
    Tick,
}

pub(crate) struct GossipActorState<S, C> {
    store: ExtendedGossipMessageStore<S>,
    control: Option<ServiceAsyncControl>,
    network_actor: Option<ActorRef<NetworkActorMessage>>,
    peer_channel_index: PeerChannelIndex,
    policy: GossipPolicyState,
    // The number of active syncing peers that we have finished syncing with.
    // Together with the number of current active syncing peers, this is
    // used to determine if we should start a new active syncing peer.
    num_finished_active_syncing_peers: usize,
    // The number of targeted active syncing peers that we want to have.
    // Currently we will only start this many active syncing peers.
    num_targeted_active_syncing_peers: usize,
    // The number of outbound passive syncing peers that we want to have.
    // We only count outbound peers because the purpose of this number is to avoid eclipse attacks.
    // By maintaining a certain number of outbound passive syncing peers, we can ensure that we are
    // not isolated from the network.
    num_targeted_outbound_passive_syncing_peers: usize,
    next_request_id: u64,
    myself: ActorRef<GossipActorMessage>,
    chain_actor: ActorRef<CkbChainMessage>,
    chain_client: C,
    delayed_outbound_messages: DelayedGossipMessageQueue,
    next_delayed_flush_at_ms: Option<u64>,
    query_reply_ports:
        HashMap<(Pubkey, u64), RpcReplyPort<Result<QueryBroadcastMessagesResult, GossipError>>>,
    peer_states: HashMap<Pubkey, PeerState>,
    latest_remote_broadcast_timestamp: Arc<AtomicU64>,
    active_sync_started_at: HashMap<Pubkey, u64>,
}

impl<S, C> GossipActorState<S, C>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    fn is_ready_for_passive_syncing(&self) -> bool {
        self.num_finished_active_syncing_peers > 0
    }

    fn num_of_active_syncing_peers(&self) -> usize {
        self.peer_states
            .values()
            .filter(|state| state.sync_status.is_active_syncing())
            .count()
    }

    fn num_of_passive_syncing_peers(&self) -> usize {
        self.passive_syncing_peers().len()
    }

    fn num_of_outbound_passive_syncing_peers(&self) -> usize {
        self.outbound_passive_syncing_peers().len()
    }

    fn outbound_passive_syncing_peers(&self) -> Vec<Pubkey> {
        self.peer_states
            .iter()
            .filter_map(|(pubkey, state)| {
                (state.sync_status.is_passive_syncing() && state.session_type.is_outbound())
                    .then_some(*pubkey)
            })
            .collect()
    }

    fn passive_syncing_peers(&self) -> Vec<Pubkey> {
        self.peer_states
            .iter()
            .filter_map(|(pubkey, state)| state.sync_status.is_passive_syncing().then_some(*pubkey))
            .collect()
    }

    fn peers_to_start_active_syncing(&self) -> Vec<Pubkey> {
        match self.num_targeted_active_syncing_peers.checked_sub(
            self.num_finished_active_syncing_peers + self.num_of_active_syncing_peers(),
        ) {
            None => vec![],
            Some(num) => self
                .peer_states
                .iter()
                .filter(|(_, state)| state.sync_status.can_start_active_syncing())
                .take(num)
                .map(|(pubkey, _)| pubkey)
                .cloned()
                .collect(),
        }
    }

    fn new_outbound_peers_to_start_passive_syncing(&self) -> Vec<Pubkey> {
        if !self.is_ready_for_passive_syncing() {
            return vec![];
        }
        match self
            .num_targeted_outbound_passive_syncing_peers
            .checked_sub(self.num_of_outbound_passive_syncing_peers())
        {
            None => vec![],
            Some(num) => self
                .peer_states
                .iter()
                .filter(|(_, state)| {
                    state.session_type.is_outbound()
                        && state.sync_status.can_start_passive_syncing()
                })
                .take(num)
                .map(|(pubkey, _)| pubkey)
                .cloned()
                .collect(),
        }
    }

    fn peers_to_start_passive_syncing(&self) -> Vec<Pubkey> {
        [
            self.peers_to_start_mutual_passive_syncing().as_slice(),
            self.new_outbound_peers_to_start_passive_syncing()
                .as_slice(),
        ]
        .concat()
    }

    fn peers_to_start_mutual_passive_syncing(&self) -> Vec<Pubkey> {
        if !self.is_ready_for_passive_syncing() {
            return vec![];
        }
        self.peer_states
            .iter()
            .filter(|(_, state)| {
                // A peer that has subscribed to us is a good candidate for passive syncing.
                // By mutual subscription, we can ensure that both us and the peer have the same set of messages.
                state.filter_processor.is_some() && state.sync_status.can_start_passive_syncing()
            })
            .map(|(pubkey, _)| *pubkey)
            .collect::<Vec<_>>()
    }

    async fn start_new_active_syncer(&mut self, pubkey: &Pubkey) {
        let safe_cursor = self.get_safe_cursor_to_start_syncing();
        let started_at = now_timestamp_as_millis_u64();
        let sync_actor = Actor::spawn_linked(
            Some(format!(
                "gossip syncing actor to peer {:?} supervised by {:?}",
                pubkey,
                self.myself.get_id()
            )),
            GossipSyncingActor::new(),
            (
                *pubkey,
                self.myself.clone(),
                self.chain_actor.clone(),
                self.chain_client.clone(),
                self.store.clone(),
                safe_cursor,
            ),
            self.myself.get_cell(),
        )
        .await
        .expect("start gossip syncing actor");

        self.peer_states
            .get_mut(pubkey)
            .expect("get peer state")
            .change_sync_status(PeerSyncStatus::ActiveGet(sync_actor.0));
        self.active_sync_started_at.insert(*pubkey, started_at);
        observe_active_sync_started();
    }

    async fn start_passive_syncer(&mut self, pubkey: &Pubkey) {
        let cursor = self.get_safe_cursor_to_start_syncing();
        let filter = BroadcastMessagesFilter {
            chain_hash: get_chain_hash(),
            after_cursor: cursor.clone(),
        };

        match self.send_broadcast_message_filter(pubkey, filter).await {
            Ok(_) => {
                self.peer_states
                    .get_mut(pubkey)
                    .expect("get peer state")
                    .change_sync_status(PeerSyncStatus::PassiveFilter(cursor));
            }
            Err(e) => {
                error!(
                    "Failed to send BroadcastMessagesFilter to peer {:?}: {:?}",
                    pubkey, e
                );
            }
        }
    }

    async fn send_broadcast_message_filter(
        &mut self,
        pubkey: &Pubkey,
        filter: BroadcastMessagesFilter,
    ) -> crate::Result<()> {
        let message = GossipMessage::BroadcastMessagesFilter(filter);
        self.send_message_to_peer(pubkey, message).await?;
        Ok(())
    }

    fn is_peer_connected(&self, pubkey: &Pubkey) -> bool {
        self.peer_states.contains_key(pubkey)
    }

    fn get_store(&self) -> &S {
        self.store.get_store()
    }

    fn get_peer_session(&self, pubkey: &Pubkey) -> Option<SessionId> {
        self.peer_states.get(pubkey).map(|s| s.session_id)
    }

    // A cursor that is "safe" to start syncing from. By "safe" we mean that
    // the node is mostly having the messages that are newer than this cursor or the messages
    // before this cursor are not important for the node to sync with the network.
    // We will start syncing from this cursor to avoid syncing from the very beginning of the network.
    fn get_safe_cursor_to_start_syncing(&self) -> Cursor {
        let latest_cursor_timestamp = self
            .latest_remote_broadcast_timestamp
            .load(Ordering::Acquire);
        let safe_cursor_timestamp = latest_cursor_timestamp
            .saturating_sub(MAX_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT.as_millis() as u64);
        let timestamp_after_considered_stale = now_timestamp_as_millis_u64()
            .saturating_sub(SOFT_BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION.as_millis() as u64);
        let timestamp = max(safe_cursor_timestamp, timestamp_after_considered_stale);
        Cursor::new(timestamp, BroadcastMessageID::default())
    }

    fn get_control(&self) -> &ServiceAsyncControl {
        self.control.as_ref().expect("control exists")
    }

    async fn send_message_to_session(
        &self,
        session_id: SessionId,
        message: GossipMessage,
    ) -> crate::Result<()> {
        send_message_to_session(self.get_control(), session_id, message).await?;
        Ok(())
    }

    fn maybe_reserve_or_delay_outbound_message(
        &mut self,
        pubkey: &Pubkey,
        message: GossipMessage,
    ) -> crate::Result<Option<GossipMessage>> {
        let now_ms = now_timestamp_as_millis_u64();
        let decision = reserve_or_delay_outbound_message(
            &mut self.policy,
            &mut self.delayed_outbound_messages,
            pubkey,
            message,
            now_ms,
        )?;
        if decision.is_none() {
            self.schedule_next_delayed_flush();
        }
        Ok(decision)
    }

    async fn send_message_to_peer_without_budget(
        &self,
        pubkey: &Pubkey,
        message: GossipMessage,
    ) -> crate::Result<()> {
        match self.get_peer_session(pubkey) {
            Some(session_id) => self.send_message_to_session(session_id, message).await,
            None => Err(Error::PeerNotFound(*pubkey)),
        }
    }

    async fn send_message_to_peer(
        &mut self,
        pubkey: &Pubkey,
        message: GossipMessage,
    ) -> crate::Result<()> {
        let Some(message) = self.maybe_reserve_or_delay_outbound_message(pubkey, message)? else {
            return Ok(());
        };
        self.send_message_to_peer_without_budget(pubkey, message)
            .await
    }

    fn schedule_next_delayed_flush(&mut self) {
        let Some(ready_at_ms) = self.delayed_outbound_messages.next_ready_at_ms() else {
            self.next_delayed_flush_at_ms = None;
            return;
        };
        if self
            .next_delayed_flush_at_ms
            .is_some_and(|scheduled_ms| scheduled_ms <= ready_at_ms)
        {
            return;
        }

        self.next_delayed_flush_at_ms = Some(ready_at_ms);
        let delay_ms = ready_at_ms.saturating_sub(now_timestamp_as_millis_u64());
        self.myself.send_after(Duration::from_millis(delay_ms), || {
            GossipActorMessage::FlushDelayedOutbound
        });
    }

    async fn flush_delayed_outbound_messages(&mut self) {
        self.next_delayed_flush_at_ms = None;
        let now_ms = now_timestamp_as_millis_u64();
        for delayed_message in self.delayed_outbound_messages.drain_ready(now_ms) {
            if let Err(error) = self
                .send_message_to_peer_without_budget(
                    &delayed_message.target,
                    delayed_message.message,
                )
                .await
            {
                self.policy.refund_outbound_message(
                    &delayed_message.target,
                    delayed_message.reserved_bytes,
                    now_ms,
                );
                match error {
                    Error::PeerNotFound(_) => {
                        trace!(
                            peer = format!("{:?}", delayed_message.target),
                            "Dropping delayed outbound gossip payload for disconnected peer"
                        );
                    }
                    other => {
                        error!(
                            "Failed to flush delayed gossip message to peer {:?}: {:?}",
                            delayed_message.target, other
                        );
                    }
                }
            }
        }
        self.schedule_next_delayed_flush();
    }

    fn should_disconnect_for_ban(&self, peer: &Pubkey) -> bool {
        self.is_peer_connected(peer) && !self.peer_channel_index.has_channels(peer)
    }

    async fn disconnect_banned_peer_if_needed(&self, peer: &Pubkey) {
        if !self.should_disconnect_for_ban(peer) {
            return;
        }
        let Some(network_actor) = self.network_actor.as_ref() else {
            return;
        };
        if let Err(error) = network_actor.send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(*peer, PeerDisconnectReason::Banned, None),
        )) {
            error!(
                "Failed to request disconnect for gossip-banned peer {:?}: {:?}",
                peer, error
            );
        }
    }

    async fn handle_malicious_peer_found(&mut self, peer: &Pubkey, violation: GossipViolation) {
        let now_ms = now_timestamp_as_millis_u64();
        let banned = self.policy.record_violation(peer, violation, now_ms);
        if banned || self.policy.is_banned(peer, now_ms) {
            self.disconnect_banned_peer_if_needed(peer).await;
        }
    }

    fn get_and_increment_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    fn drop_delayed_outbound_messages_for_peer(&mut self, peer: &Pubkey, now_ms: u64) {
        for delayed_message in self.delayed_outbound_messages.remove_peer(peer) {
            self.policy.refund_outbound_message(
                &delayed_message.target,
                delayed_message.reserved_bytes,
                now_ms,
            );
        }
    }
}

fn outbound_gossip_message_cost(message: &GossipMessage) -> Option<u64> {
    match message {
        GossipMessage::BroadcastMessagesFilter(_)
        | GossipMessage::GetBroadcastMessages(_)
        | GossipMessage::QueryBroadcastMessages(_) => None,
        GossipMessage::BroadcastMessagesFilterResult(_)
        | GossipMessage::GetBroadcastMessagesResult(_)
        | GossipMessage::QueryBroadcastMessagesResult(_) => {
            Some(message.clone().to_molecule_bytes().len() as u64)
        }
    }
}

fn should_bypass_outbound_rate_limit_for_message(
    policy: &GossipPolicyState,
    pubkey: &Pubkey,
    message: &GossipMessage,
    now_ms: u64,
) -> bool {
    let Some(bytes) = outbound_gossip_message_cost(message) else {
        return false;
    };
    policy.should_bypass_oversized_outbound_message(pubkey, bytes, now_ms)
}

fn reserve_immediate_outbound_message_budget(
    policy: &mut GossipPolicyState,
    pubkey: &Pubkey,
    message: &GossipMessage,
    now_ms: u64,
) -> Option<u64> {
    let bytes = outbound_gossip_message_cost(message)?;
    Some(policy.reserve_outbound_message(pubkey, bytes, now_ms))
}

fn reserve_or_delay_outbound_message(
    policy: &mut GossipPolicyState,
    delayed_outbound_messages: &mut DelayedGossipMessageQueue,
    pubkey: &Pubkey,
    message: GossipMessage,
    now_ms: u64,
) -> crate::Result<Option<GossipMessage>> {
    let Some(bytes) = outbound_gossip_message_cost(&message) else {
        return Ok(Some(message));
    };

    if should_bypass_outbound_rate_limit_for_message(policy, pubkey, &message, now_ms) {
        // Preserve protocol progress for the first oversized payload after buckets refill. Once
        // debt exists, later oversized pages must respect the queued delay path.
        let reserved_delay_ms =
            reserve_immediate_outbound_message_budget(policy, pubkey, &message, now_ms)
                .expect("chargeable outbound message");
        trace!(
            peer = format!("{pubkey:?}"),
            bytes,
            delay_ms = reserved_delay_ms,
            "Bypassing outbound gossip rate limiter for single oversized payload"
        );
        return Ok(Some(message));
    }

    let delay_ms = policy.estimate_outbound_message_delay(pubkey, bytes, now_ms);
    if delay_ms == 0 {
        let reserved_delay_ms = policy.reserve_outbound_message(pubkey, bytes, now_ms);
        debug_assert_eq!(reserved_delay_ms, 0);
        return Ok(Some(message));
    }

    if delayed_outbound_messages.is_full_for_peer(pubkey) {
        trace!(
            peer = format!("{pubkey:?}"),
            bytes,
            delay_ms,
            "Rejecting delayed outbound gossip payload due to full per-peer queue"
        );
        return Err(Error::InternalError(anyhow::anyhow!(
            "delayed outbound gossip queue is full for peer"
        )));
    }

    let reserved_delay_ms = policy.reserve_outbound_message(pubkey, bytes, now_ms);
    debug_assert!(reserved_delay_ms >= delay_ms);
    let queued = delayed_outbound_messages.push(DelayedGossipMessage {
        target: *pubkey,
        message,
        reserved_bytes: bytes,
        ready_at_ms: now_ms.saturating_add(reserved_delay_ms),
    });
    debug_assert!(queued);

    trace!(
        peer = format!("{pubkey:?}"),
        bytes,
        delay_ms = reserved_delay_ms,
        "Delaying outbound gossip payload due to rate limiter"
    );
    Ok(None)
}

fn classify_verify_and_save_broadcast_message_error(
    message: &BroadcastMessage,
    error: &VerifyBroadcastMessageError,
) -> Option<GossipViolation> {
    match message {
        BroadcastMessage::ChannelAnnouncement(_)
            if matches!(
                error,
                VerifyBroadcastMessageError::DeferredChannelAnnouncementVerification(_, _)
            ) =>
        {
            None
        }
        _ => Some(GossipViolation::InvalidBroadcastMessage),
    }
}

async fn send_message_to_session(
    control: &ServiceAsyncControl,
    session_id: SessionId,
    message: GossipMessage,
) -> crate::Result<()> {
    let payload = message.to_molecule_bytes();
    let payload_len = payload.len();
    control
        .send_message_to(session_id, GOSSIP_PROTOCOL_ID, payload)
        .await?;
    observe_gossip_sent_bytes(payload_len);
    Ok(())
}

pub(crate) struct GossipProtocolHandle {
    actor: ActorRef<GossipActorMessage>,
    sender: Option<oneshot::Sender<ServiceAsyncControl>>,
}

async fn get_message_cursor<S: GossipMessageStore>(
    message: &BroadcastMessage,
    store: &S,
    chain: &ActorRef<CkbChainMessage>,
    client: &impl CkbChainClient,
) -> Result<Cursor, Error> {
    match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
            let timestamp =
                get_channel_timestamp(&channel_announcement.channel_outpoint, store, chain, client)
                    .await?;
            Ok(Cursor::new(
                timestamp,
                BroadcastMessageID::ChannelAnnouncement(
                    channel_announcement.channel_outpoint.clone(),
                ),
            ))
        }
        BroadcastMessage::ChannelUpdate(channel_update) => Ok(Cursor::new(
            channel_update.timestamp,
            BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone()),
        )),
        BroadcastMessage::NodeAnnouncement(node_announcement) => Ok(Cursor::new(
            node_announcement.timestamp,
            BroadcastMessageID::NodeAnnouncement(node_announcement.node_id),
        )),
    }
}

fn get_existing_broadcast_message<S: GossipMessageStore>(
    message: &BroadcastMessage,
    store: &S,
) -> Option<BroadcastMessageWithTimestamp> {
    match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => store
            .get_latest_channel_announcement(&channel_announcement.channel_outpoint)
            .map(|(timestamp, channel_announcement)| {
                BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement)
            }),
        BroadcastMessage::ChannelUpdate(channel_update) => store
            .get_latest_channel_update(
                &channel_update.channel_outpoint,
                channel_update.is_update_of_node_1(),
            )
            .map(|store_channel_update| {
                BroadcastMessageWithTimestamp::ChannelUpdate(store_channel_update)
            }),
        BroadcastMessage::NodeAnnouncement(node_announcement) => store
            .get_latest_node_announcement(&node_announcement.node_id)
            .map(|store_node_announcement| {
                BroadcastMessageWithTimestamp::NodeAnnouncement(store_node_announcement)
            }),
    }
}

fn get_existing_newer_broadcast_message<S: GossipMessageStore>(
    message: &BroadcastMessage,
    store: &S,
) -> Option<BroadcastMessageWithTimestamp> {
    get_existing_broadcast_message(message, store).and_then(|existing_message| {
        match message.cursor() {
            Some(cursor) if cursor > existing_message.cursor() => None,
            _ => Some(existing_message),
        }
    })
}

// Verify and save broadcast messages to the store.
// Note that we can't relialy verify a message until we have all the messages that it depends on.
// So this function should be called by the dependency order of the messages.
// E.g. channel updates depends on channel announcements to obtain the node public keys,
// so we should call this method to save and verify channel announcements before channel updates.
async fn verify_and_save_broadcast_message<S: GossipMessageStore>(
    message: &BroadcastMessage,
    store: &S,
    chain: &ActorRef<CkbChainMessage>,
    client: &impl CkbChainClient,
) -> Result<(BroadcastMessageWithTimestamp, bool), VerifyBroadcastMessageError> {
    let msg_chain_hash = match message {
        BroadcastMessage::NodeAnnouncement(msg) => msg.chain_hash,
        BroadcastMessage::ChannelAnnouncement(msg) => msg.chain_hash,
        BroadcastMessage::ChannelUpdate(msg) => msg.chain_hash,
    };
    if msg_chain_hash != get_chain_hash() {
        return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
            "Invalid chain hash: expected {:?}, got {:?}",
            get_chain_hash(),
            msg_chain_hash
        )));
    }

    let (timestamp, is_newly_applied) = match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
            let on_chain_info =
                get_channel_on_chain_info(channel_announcement.out_point(), chain, client).await?;
            let already_saved =
                verify_channel_announcement(channel_announcement, &on_chain_info, store).await?;
            if !already_saved {
                store.save_channel_announcement(
                    on_chain_info.timestamp,
                    channel_announcement.clone(),
                );
            }
            (on_chain_info.timestamp, !already_saved)
        }
        BroadcastMessage::ChannelUpdate(channel_update) => {
            let already_saved = verify_channel_update(channel_update, store)?;
            if !already_saved {
                store.save_channel_update(channel_update.clone());
            }
            (channel_update.timestamp, !already_saved)
        }
        BroadcastMessage::NodeAnnouncement(node_announcement) => {
            let already_saved = verify_node_announcement(node_announcement, store)?;
            if !already_saved {
                store.save_node_announcement(node_announcement.clone());
            }
            (node_announcement.timestamp, !already_saved)
        }
    };
    Ok(((message.clone(), timestamp).into(), is_newly_applied))
}

async fn get_channel_tx(
    outpoint: &OutPoint,
    chain: &ActorRef<CkbChainMessage>,
    client: &impl CkbChainClient,
) -> Result<(TransactionView, Hash256), VerifyBroadcastMessageError> {
    // Wait for the tx to be available in test.
    //
    // In the payment test, channels are created first, then the funding
    // transactions are synchronized to all the mock ckb chain actors in all
    // nodes. Thus there is a time window that when the channel announcement
    // is broadcasted, the funding transaction is still unknown.
    #[cfg(any(test, feature = "bench"))]
    let _ = call_t!(
        chain,
        |callback| CkbChainMessage::CreateTxTracer(crate::ckb::CkbTxTracer {
            tx_hash: outpoint.tx_hash().into(),
            confirmations: 0,
            mask: crate::ckb::CkbTxTracingMask::all_flags(),
            callback,
        }),
        DEFAULT_CHAIN_ACTOR_TIMEOUT
    );
    #[cfg(not(any(test, feature = "bench")))]
    let _ = chain;

    match client.get_transaction(outpoint.tx_hash().unpack()).await {
        Ok(GetTxResponse {
            transaction: Some(tx),
            tx_status: TxStatus::Committed(_, block_hash, _),
        }) => Ok((tx, block_hash.into())),
        Ok(response) => Err(VerifyBroadcastMessageError::InvalidParameter(format!(
            "Channel announcement transaction {:?} not committed: {:?}",
            &outpoint.tx_hash(),
            response.tx_status
        ))),
        Err(err) => Err(
            VerifyBroadcastMessageError::DeferredChannelAnnouncementVerification(
                outpoint.clone(),
                format!(
                    "funding transaction not found or not confirmed, result is: {:?}",
                    err
                ),
            ),
        ),
    }
}

async fn get_channel_timestamp<S: GossipMessageStore>(
    outpoint: &OutPoint,
    store: &S,
    chain: &ActorRef<CkbChainMessage>,
    client: &impl CkbChainClient,
) -> Result<u64, Error> {
    if let Some((timestamp, _)) = store.get_latest_channel_announcement(outpoint) {
        return Ok(timestamp);
    }

    let on_chain_info = get_channel_on_chain_info(outpoint, chain, client)
        .await
        .map_err(Error::from)?;

    Ok(on_chain_info.timestamp)
}

async fn get_channel_on_chain_info(
    outpoint: &OutPoint,
    chain: &ActorRef<CkbChainMessage>,
    client: &impl CkbChainClient,
) -> Result<ChannelOnchainInfo, VerifyBroadcastMessageError> {
    let (tx, block_hash) = get_channel_tx(outpoint, chain, client).await?;
    let first_output = match tx.outputs().get(0) {
        None => {
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "On-chain transaction found but no output: {:?}",
                &outpoint
            )));
        }
        Some(output) => output.clone().into(),
    };

    let first_output_data = match tx.outputs_data().get(0) {
        None => {
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "On-chain transaction found but no output data: {:?}",
                &outpoint
            )));
        }
        Some(data) => data,
    };

    let timestamp: u64 = match client.get_block_timestamp(block_hash).await {
        Ok(Some(timestamp)) => timestamp,
        Ok(None) => {
            return Err(
                VerifyBroadcastMessageError::DeferredChannelAnnouncementVerification(
                    outpoint.clone(),
                    format!("unable to find block timestamp for block {:?}", &block_hash),
                ),
            );
        }
        Err(err) => {
            return Err(
                VerifyBroadcastMessageError::DeferredChannelAnnouncementVerification(
                    outpoint.clone(),
                    format!(
                        "error while trying to obtain block {:?} timestamp: {:?}",
                        block_hash, err
                    ),
                ),
            );
        }
    };

    Ok(ChannelOnchainInfo {
        timestamp,
        first_output,
        first_output_data,
    })
}

// Verify the channel announcement message. If any error occurs, return the error.
// Otherwise, return the timestamp of the channel announcement and a bool value indicating if the
// the channel announcement is already saved to the store. If it is already saved, the bool value
// is true, otherwise it is false.
async fn verify_channel_announcement<S: GossipMessageStore>(
    channel_announcement: &ChannelAnnouncement,
    on_chain_info: &ChannelOnchainInfo,
    store: &S,
) -> Result<bool, VerifyBroadcastMessageError> {
    if let Some((_, announcement)) =
        store.get_latest_channel_announcement(&channel_announcement.channel_outpoint)
    {
        if announcement == *channel_announcement {
            return Ok(true);
        } else {
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "Channel announcement message already exists but mismatched: {:?}, existing: {:?}",
                &channel_announcement, &announcement
            )));
        }
    }
    let message = channel_announcement.message_to_sign();
    if channel_announcement.node1_id == channel_announcement.node2_id {
        return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
            "Channel announcement node had a channel with itself: {:?}",
            &channel_announcement
        )));
    }
    let (node1_signature, node2_signature, ckb_signature) = match (
        &channel_announcement.node1_signature,
        &channel_announcement.node2_signature,
        &channel_announcement.ckb_signature,
    ) {
        (Some(node1_signature), Some(node2_signature), Some(ckb_signature)) => {
            (node1_signature, node2_signature, ckb_signature)
        }
        _ => {
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "Channel announcement message signature verification failed, some signatures are missing: {:?}",
                &channel_announcement
            )));
        }
    };

    if !node1_signature.verify(&channel_announcement.node1_id, &message) {
        return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
            "Channel announcement message signature verification failed for node 1: {:?}, message: {:?}, signature: {:?}, pubkey: {:?}",
            &channel_announcement,
            &message,
            &node1_signature,
            &channel_announcement.node1_id
        )));
    }

    if !node2_signature.verify(&channel_announcement.node2_id, &message) {
        return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
            "Channel announcement message signature verification failed for node 2: {:?}, message: {:?}, signature: {:?}, pubkey: {:?}",
            &channel_announcement,
            &message,
            &node2_signature,
            &channel_announcement.node2_id
        )));
    }

    let pubkey = channel_announcement.ckb_key.serialize();
    let pubkey_hash = &blake2b_256(pubkey.as_slice())[0..20];

    let output = &on_chain_info.first_output;
    if output.lock.args.as_bytes() != pubkey_hash {
        return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                    "On-chain transaction found but pubkey hash mismatched: on chain hash {:?}, pub key ({:?}) hash {:?}",
                    &output.lock.args.as_bytes(),
                    hex::encode(pubkey),
                    &pubkey_hash
                )));
    }
    let capacity: u128 = u64::from(output.capacity).into();
    match &channel_announcement.udt_type_script {
        Some(announced_udt_script) => {
            match &output.type_ {
                Some(on_chain_script) => {
                    let on_chain_packed: ckb_types::packed::Script = on_chain_script.clone().into();
                    if &on_chain_packed != announced_udt_script {
                        return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                            "UDT type script mismatch: announced {:?}, on-chain {:?}",
                            announced_udt_script, on_chain_packed
                        )));
                    }
                }
                None => {
                    return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                        "UDT channel announcement but output has no type script: {:?}",
                        &channel_announcement.channel_outpoint
                    )));
                }
            }
            let data = on_chain_info.first_output_data.raw_data();
            if data.len() < 16 {
                return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                    "UDT output data too short for amount: {} bytes, expected at least 16",
                    data.len()
                )));
            }
            let udt_amount = u128::from_le_bytes(data[..16].try_into().expect("infallible"));
            if channel_announcement.capacity != udt_amount {
                return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                    "Announced UDT capacity {} does not match on-chain UDT amount {}",
                    channel_announcement.capacity, udt_amount
                )));
            }
        }
        None => {
            if channel_announcement.capacity > capacity {
                return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                    "On-chain transaction found but capacity mismatched: on chain capacity {:?} smaller than annoucned channel capacity {:?}",
                    &output.capacity, &channel_announcement.capacity
                )));
            }
        }
    }

    if let Err(err) =
        SECP256K1.verify_schnorr(ckb_signature, &message, &channel_announcement.ckb_key)
    {
        return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
            "Channel announcement message signature verification failed for ckb: {:?}, message: {:?}, signature: {:?}, pubkey: {:?}, error: {:?}",
            &channel_announcement,
            &message,
            &ckb_signature,
            &channel_announcement.ckb_key,
            &err
        )));
    }

    Ok(false)
}

// Verify the signature of the channel update message. If there is any error, an error will be returned.
// Else if the channel update is already saved to the store, true will be returned.
// Otherwise false will be returned. The caller may use this value to determine if the channel update
// message should be saved to the store.
fn verify_channel_update<S: GossipMessageStore>(
    channel_update: &ChannelUpdate,
    store: &S,
) -> Result<bool, VerifyBroadcastMessageError> {
    if let Some(BroadcastMessageWithTimestamp::ChannelUpdate(existing)) =
        store.get_broadcast_message_with_cursor(&channel_update.cursor())
    {
        if existing == *channel_update {
            return Ok(true);
        } else {
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "Channel update message already exists but mismatched: {:?}, existing: {:?}",
                &channel_update, &existing
            )));
        }
    }
    let message = channel_update.message_to_sign();

    let signature = match channel_update.signature {
        Some(ref signature) => signature,
        None => {
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "Channel update message signature verification failed (signature not found): {:?}",
                &channel_update
            )));
        }
    };
    match store.get_latest_channel_announcement(&channel_update.channel_outpoint) {
        Some((_, channel_announcement)) => {
            let pubkey = if channel_update.is_update_of_node_1() {
                channel_announcement.node1_id
            } else {
                channel_announcement.node2_id
            };
            if !signature.verify(&pubkey, &message) {
                return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                    "Channel update message signature verification failed (invalid signature): {:?}",
                    &channel_update
                )));
            }
            Ok(false)
        }
        None => {
            // It is possible that the channel update message is received before the channel announcement message.
            // In this case, we should temporarily store the channel update message and verify it later
            // when the channel announcement message is received.
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "Channel announcement message not found for channel update message: {:?}",
                &channel_update.channel_outpoint
            )));
        }
    }
}

// Verify the signature of the node announcement message. If there is any error, an error will be returned.
// Else if the node announcement is already saved to the store, true will be returned.
// Otherwise false will be returned. The caller may use this value to determine if the node announcement
// message should be saved to the store.
fn verify_node_announcement<S: GossipMessageStore>(
    node_announcement: &NodeAnnouncement,
    store: &S,
) -> Result<bool, VerifyBroadcastMessageError> {
    if let Some(BroadcastMessageWithTimestamp::NodeAnnouncement(announcement)) =
        store.get_broadcast_message_with_cursor(&node_announcement.cursor())
    {
        if announcement == *node_announcement {
            return Ok(true);
        } else {
            return Err(VerifyBroadcastMessageError::InvalidParameter(format!(
                "Node announcement message already exists but mismatched: {:?}, existing: {:?}",
                &node_announcement, &announcement
            )));
        }
    }
    if !node_announcement.verify() {
        Err(VerifyBroadcastMessageError::InvalidParameter(
            "Node announcement message signature verification failed".to_string(),
        ))
    } else {
        Ok(false)
    }
}

#[allow(clippy::too_many_arguments)]
impl GossipProtocolHandle {
    pub(crate) fn new(
        actor: ActorRef<GossipActorMessage>,
        sender: oneshot::Sender<ServiceAsyncControl>,
    ) -> Self {
        Self {
            actor,
            sender: Some(sender),
        }
    }

    pub(crate) fn actor(&self) -> &ActorRef<GossipActorMessage> {
        &self.actor
    }

    pub(crate) fn create_meta(self) -> ProtocolMeta {
        MetaBuilder::new()
            .id(GOSSIP_PROTOCOL_ID)
            .codec(move || {
                Box::new(
                    length_delimited::Builder::new()
                        .max_frame_length(MAX_SERVICE_PROTOCOAL_DATA_SIZE)
                        .new_codec(),
                )
            })
            .service_handle(move || {
                let handle = Box::new(self);
                ProtocolHandle::Callback(handle)
            })
            .build()
    }
}
#[async_trait::async_trait]
impl<S, C> Actor for GossipActor<S, C>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    type Msg = GossipActorMessage;
    type State = GossipActorState<S, C>;
    type Arguments = (
        Option<Pubkey>,
        oneshot::Receiver<ServiceAsyncControl>,
        oneshot::Sender<ExtendedGossipMessageStore<S>>,
        Duration,
        Duration,
        Duration,
        bool,
        usize,
        usize,
        GossipPolicyConfig,
        S,
        ActorRef<CkbChainMessage>,
        C,
        Option<ActorRef<NetworkActorMessage>>,
        PeerChannelIndex,
    );

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (
            local_pubkey,
            rx,
            tx,
            network_maintenance_interval,
            store_maintenance_interval,
            store_prune_interval,
            announce_private_addr,
            num_targeted_active_syncing_peers,
            num_targeted_outbound_passive_syncing_peers,
            policy,
            store,
            chain_actor,
            chain_client,
            network_actor,
            peer_channel_index,
        ): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let latest_remote_broadcast_timestamp = Arc::new(AtomicU64::new(
            get_latest_startup_broadcast_message_cursor(&store, local_pubkey.as_ref()).timestamp,
        ));
        let store = ExtendedGossipMessageStore::new(
            store_maintenance_interval,
            announce_private_addr,
            policy.inbound_channel_update.clone(),
            store,
            myself.clone(),
            latest_remote_broadcast_timestamp.clone(),
            chain_actor.clone(),
            chain_client.clone(),
            myself.get_cell(),
        )
        .await;
        if tx.send(store.clone()).is_err() {
            panic!("failed to send store to the caller");
        }

        let cloned_myself = myself.clone();
        ractor::concurrency::spawn(async move {
            match rx.await {
                Ok(control) => {
                    if let Err(error) =
                        cloned_myself.send_message(GossipActorMessage::ReceivedControl(control))
                    {
                        error!(
                            "Failed to send ReceivedControl message to gossip actor: {:?}",
                            error
                        );
                    }
                }
                Err(_) => {
                    error!("Failed to receive control");
                }
            }
        });

        myself.send_interval(network_maintenance_interval, || {
            GossipActorMessage::TickNetworkMaintenance
        });
        myself.send_interval(store_prune_interval, || {
            let prune_duration = HARD_BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION;
            let stale_timestamp = now_timestamp_as_millis_u64()
                .checked_sub(prune_duration.as_millis() as u64)
                .unwrap_or_default();
            GossipActorMessage::PruneStaleGossipMessages(stale_timestamp)
        });
        let delayed_outbound_messages =
            DelayedGossipMessageQueue::new(policy.outbound_delay_queue_capacity);
        let state = Self::State {
            store,
            control: Default::default(),
            network_actor,
            peer_channel_index,
            policy: GossipPolicyState::new(policy),
            num_targeted_active_syncing_peers,
            num_targeted_outbound_passive_syncing_peers,
            myself,
            chain_actor,
            chain_client,
            next_request_id: Default::default(),
            delayed_outbound_messages,
            next_delayed_flush_at_ms: Default::default(),
            query_reply_ports: Default::default(),
            num_finished_active_syncing_peers: Default::default(),
            peer_states: Default::default(),
            latest_remote_broadcast_timestamp,
            active_sync_started_at: Default::default(),
        };
        Ok(state)
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorTerminated(who, _, _) => {
                debug!("{:?} terminated", who);
            }
            SupervisionEvent::ActorFailed(who, err) => {
                panic!("Actor unexpectedly panicked (id: {:?}): {:?}", who, err);
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _handle_log_guard = ActorHandleLogGuard::new(
            "GossipActor",
            message.as_ref().to_string(),
            "fiber.gossip_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            GossipActorMessage::ReceivedControl(control) => {
                state.control = Some(control);
                // If peers have already connected before control was received,
                // immediately trigger network maintenance so we don't wait for
                // the next periodic tick (up to 60s) to start syncing.
                if !state.peer_states.is_empty() {
                    myself.send_message(GossipActorMessage::TickNetworkMaintenance)?;
                }
            }

            GossipActorMessage::PeerConnected(pubkey, session) => {
                if state.is_peer_connected(&pubkey) {
                    return Ok(());
                }
                state
                    .peer_states
                    .insert(pubkey, PeerState::new(session.id, session.ty));
                if state
                    .policy
                    .is_banned(&pubkey, now_timestamp_as_millis_u64())
                {
                    state.disconnect_banned_peer_if_needed(&pubkey).await;
                } else if state.control.is_some() {
                    // Immediately trigger network maintenance to start syncing with
                    // the new peer, rather than waiting for the next periodic
                    // TickNetworkMaintenance (up to 60s in production).
                    // This eliminates the initial gossip sync delay after connecting
                    // to a bootnode, which is critical for WASM nodes that need
                    // peer addresses from gossip data quickly.
                    // Only send the tick if control is already available; otherwise
                    // the tick will be sent from the ReceivedControl handler once
                    // control arrives (to avoid a panic in send_message_to_peer).
                    myself.send_message(GossipActorMessage::TickNetworkMaintenance)?;
                }
            }
            GossipActorMessage::PeerDisconnected(pubkey, _session) => {
                state.peer_states.remove(&pubkey);
                state.active_sync_started_at.remove(&pubkey);
                state.drop_delayed_outbound_messages_for_peer(
                    &pubkey,
                    now_timestamp_as_millis_u64(),
                );
                state.schedule_next_delayed_flush();
            }
            GossipActorMessage::QueryBroadcastMessagesTimeout(peer, request_id) => {
                if let Some(reply) = state.query_reply_ports.remove(&(peer, request_id)) {
                    let _ = reply.send(Err(GossipError::Timeout));
                }
            }
            GossipActorMessage::QueryBroadcastMessages(peer, queries, reply) => {
                let id = state.get_and_increment_request_id();
                match state
                    .send_message_to_peer(
                        &peer,
                        GossipMessage::QueryBroadcastMessages(QueryBroadcastMessages {
                            id,
                            chain_hash: get_chain_hash(),
                            queries,
                        }),
                    )
                    .await
                {
                    Err(error) => {
                        let _ = reply.send(Err(GossipError::FailedToSendMessage(error)));
                    }
                    Ok(_) => {
                        state.query_reply_ports.insert((peer, id), reply);
                        myself.send_after(QUERY_BROADCAST_MESSAGES_TIMEOUT, move || {
                            GossipActorMessage::QueryBroadcastMessagesTimeout(peer, id)
                        });
                    }
                }
            }
            GossipActorMessage::TryBroadcastMessages(messages) => {
                trace!("Trying to broadcast message: {:?}", &messages);
                state
                    .store
                    .actor
                    .send_message(ExtendedGossipMessageStoreMessage::SaveAndBroadcastMessages(
                        messages,
                    ))
                    .expect("store actor alive");
            }
            GossipActorMessage::UpdatePeerFilter(pubkey, cursor) => {
                self.update_peer_filter(state, &pubkey, &cursor, myself)
                    .await;
            }

            GossipActorMessage::TickNetworkMaintenance => {
                state.policy.prune_bans(now_timestamp_as_millis_u64());
                trace!(
                    "Gossip network maintenance ticked, current state: num of peers: {}, num of finished syncing peers: {}, num of active syncing peers: {}, num of passive syncing peers: {}",
                    state.peer_states.len(),
                    state.num_finished_active_syncing_peers,
                    state.num_of_active_syncing_peers(),
                    state.num_of_passive_syncing_peers(),
                );
                for peer in state.peers_to_start_active_syncing() {
                    state.start_new_active_syncer(&peer).await;
                }

                for peer in state.peers_to_start_passive_syncing() {
                    state.start_passive_syncer(&peer).await;
                }
            }

            GossipActorMessage::FlushDelayedOutbound => {
                state.flush_delayed_outbound_messages().await;
            }

            GossipActorMessage::PruneStaleGossipMessages(stale_timestamp) => {
                if stale_timestamp == 0 {
                    return Ok(());
                }
                let store = state.store.get_store().clone();

                // TODO: we will iterate over all the channel timestamps here even if they are not stale.
                for (outpoint, timestamps) in store.get_channel_timestamps_iter() {
                    let max_timestamp = timestamps.into_iter().max().unwrap_or_default();
                    if max_timestamp < stale_timestamp {
                        store.delete_broadcast_message(&Cursor::new(
                            timestamps[0],
                            BroadcastMessageID::ChannelAnnouncement(outpoint.clone()),
                        ));
                        for channel_timestamp in &timestamps[1..] {
                            store.delete_broadcast_message(&Cursor::new(
                                *channel_timestamp,
                                BroadcastMessageID::ChannelUpdate(outpoint.clone()),
                            ));
                        }
                        store.delete_channel_timestamps(&outpoint);
                    }
                }
            }

            GossipActorMessage::ActiveSyncingFinished(pubkey, cursor) => {
                state.num_finished_active_syncing_peers += 1;
                if let Some(started_at) = state.active_sync_started_at.remove(&pubkey) {
                    let now = now_timestamp_as_millis_u64();
                    observe_active_sync_completion(now.saturating_sub(started_at));
                }
                observe_active_sync_finished();
                if let Some(peer_state) = state.peer_states.get_mut(&pubkey) {
                    peer_state.change_sync_status(PeerSyncStatus::FinishedActiveSyncing(
                        now_timestamp_as_millis_u64(),
                        cursor,
                    ));
                }
            }

            GossipActorMessage::MaliciousPeerFound { peer, violation } => {
                warn!("Malicious gossip peer found {:?}: {:?}", &peer, violation);
                state.handle_malicious_peer_found(&peer, violation).await;
            }

            GossipActorMessage::SendGossipMessage(GossipMessageWithTarget { target, message }) => {
                if let Err(error) = state.send_message_to_peer(&target, message).await {
                    error!(
                        "Failed to send gossip message to peer {:?}: {:?}",
                        target, error
                    );
                }
            }

            GossipActorMessage::GossipMessageReceived(GossipMessageWithTarget {
                target: pubkey,
                message,
            }) => match message {
                GossipMessage::BroadcastMessagesFilter(BroadcastMessagesFilter {
                    chain_hash,
                    after_cursor,
                }) => {
                    if let Err(e) = check_chain_hash(&chain_hash) {
                        error!("Failed to check chain hash: {:?}", e);
                        return Ok(());
                    }
                    if after_cursor.is_max() {
                        info!(
                                "Received BroadcastMessagesFilter with max cursor from peer, stopping filter processor to {:?}",
                                pubkey
                            );
                        state.peer_states.remove(&pubkey);
                        return Ok(());
                    }
                    self.update_peer_filter(state, &pubkey, &after_cursor, myself)
                        .await;
                }
                GossipMessage::BroadcastMessagesFilterResult(BroadcastMessagesFilterResult {
                    messages,
                }) => {
                    observe_received_broadcast_messages(messages.len());
                    observe_received_propagation_latencies(&messages, GOSSIP_INGRESS_PASSIVE);
                    state
                        .store
                        .actor
                        .send_message(ExtendedGossipMessageStoreMessage::SaveMessages(
                            pubkey, messages,
                        ))
                        .expect("store actor alive");
                }
                GossipMessage::GetBroadcastMessages(get_broadcast_messages) => {
                    if let Err(e) = check_chain_hash(&get_broadcast_messages.chain_hash) {
                        error!("Failed to check chain hash: {:?}", e);
                        return Ok(());
                    }
                    if get_broadcast_messages.count == 0
                        || get_broadcast_messages.count > MAX_NUM_OF_BROADCAST_MESSAGES
                    {
                        warn!(
                            "Received GetBroadcastMessages with invalid count: {:?}",
                            get_broadcast_messages.count
                        );
                        return Ok(());
                    }
                    let id = get_broadcast_messages.id;
                    let messages = state.get_store().get_broadcast_messages(
                        &get_broadcast_messages.after_cursor,
                        get_broadcast_messages.count as usize,
                    );
                    let result =
                        GossipMessage::GetBroadcastMessagesResult(GetBroadcastMessagesResult {
                            id,
                            messages: messages.into_iter().map(|m| m.into()).collect(),
                        });
                    if let Err(error) = state.send_message_to_peer(&pubkey, result).await {
                        error!(
                            "Failed to send GetBroadcastMessagesResult to peer {:?}: {:?}",
                            pubkey, error
                        );
                    }
                }
                GossipMessage::GetBroadcastMessagesResult(result) => {
                    observe_received_broadcast_messages(result.messages.len());
                    observe_received_propagation_latencies(
                        &result.messages,
                        GOSSIP_INGRESS_ACTIVE_GET,
                    );
                    let peer_state = state.peer_states.get(&pubkey);
                    if let Some(PeerState {
                        sync_status: PeerSyncStatus::ActiveGet(actor),
                        ..
                    }) = peer_state
                    {
                        let _ =
                            actor.send_message(GossipSyncingActorMessage::ResponseReceived(result));
                    } else {
                        warn!(
                            "Received GetBroadcastMessagesResult from peer {:?} in state {:?}",
                            pubkey, &peer_state
                        );
                    }
                }
                GossipMessage::QueryBroadcastMessages(QueryBroadcastMessages {
                    id,
                    chain_hash,
                    queries,
                }) => {
                    if let Err(e) = check_chain_hash(&chain_hash) {
                        error!("Failed to check chain hash: {:?}", e);
                        return Ok(());
                    }
                    if queries.len() > MAX_NUM_OF_BROADCAST_MESSAGES as usize {
                        warn!(
                            "Received QueryBroadcastMessages with too many queries: {:?}",
                            queries.len()
                        );
                        return Ok(());
                    }
                    let (results, missing_queries) =
                        state.get_store().query_broadcast_messages(queries);
                    let result =
                        GossipMessage::QueryBroadcastMessagesResult(QueryBroadcastMessagesResult {
                            id,
                            messages: results.into_iter().map(|m| m.into()).collect(),
                            missing_queries,
                        });
                    if let Err(error) = state.send_message_to_peer(&pubkey, result).await {
                        error!(
                            "Failed to send QueryBroadcastMessagesResult to peer {:?}: {:?}",
                            pubkey, error
                        );
                    }
                }
                GossipMessage::QueryBroadcastMessagesResult(result) => {
                    observe_received_broadcast_messages(result.messages.len());
                    observe_received_propagation_latencies(&result.messages, GOSSIP_INGRESS_QUERY);
                    if let Some(reply) = state.query_reply_ports.remove(&(pubkey, result.id)) {
                        let _ = reply.send(Ok(result));
                    }
                }
            },
        }

        Ok(())
    }
}

#[tasync_trait]
impl ServiceProtocol for GossipProtocolHandle {
    async fn init(&mut self, context: &mut ProtocolContext) {
        let sender = self
            .sender
            .take()
            .expect("service control sender set and init called once");
        if sender.send(context.control().clone()).is_err() {
            panic!("Failed to send service control");
        }
    }

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        trace!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id,
            context.session.id,
            context.session.address,
            context.session.ty,
            version
        );

        if let Some(remote_pubkey) = context.session.remote_pubkey.clone() {
            let _ = self.actor.send_message(GossipActorMessage::PeerConnected(
                super::types::pubkey_from_tentacle(remote_pubkey),
                context.session.clone(),
            ));
        } else {
            warn!("Peer connected without remote pubkey {:?}", context.session);
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        trace!(
            "proto id [{}] close on session [{}], address: [{}], type: [{:?}]",
            context.proto_id,
            context.session.id,
            &context.session.address,
            &context.session.ty
        );

        match context.session.remote_pubkey.as_ref() {
            Some(remote_pubkey) => {
                let _ = self
                    .actor
                    .send_message(GossipActorMessage::PeerDisconnected(
                        super::types::pubkey_from_tentacle(remote_pubkey.clone()),
                        context.session.clone(),
                    ));
            }
            None => {
                unreachable!("Received message without remote pubkey");
            }
        }
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        observe_gossip_received_bytes(data.len());
        let message = unwrap_or_return!(GossipMessage::from_molecule_slice(&data), "parse message");
        match context.session.remote_pubkey.as_ref() {
            Some(pubkey) => {
                let _ = self
                    .actor
                    .send_message(GossipActorMessage::GossipMessageReceived(
                        GossipMessageWithTarget {
                            target: super::types::pubkey_from_tentacle(pubkey.clone()),
                            message,
                        },
                    ));
            }
            None => {
                unreachable!("Received message without remote pubkey");
            }
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}
