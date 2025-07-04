use core::panic;
use std::{
    cmp::max,
    collections::{HashMap, HashSet},
    marker::PhantomData,
    sync::Arc,
    time::Duration,
};

use ckb_hash::blake2b_256;
use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::OutPoint,
};
use ractor::{
    async_trait as rasync_trait, call, call_t, concurrency::JoinHandle, Actor, ActorCell,
    ActorProcessingErr, ActorRef, ActorRuntime, MessagingErr, OutputPort, RpcReplyPort,
    SupervisionEvent,
};
use secp256k1::Message;
use tentacle::{
    async_trait as tasync_trait,
    builder::MetaBuilder,
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, SessionContext},
    secio::PeerId,
    service::{ProtocolHandle, ProtocolMeta, ServiceAsyncControl, SessionType},
    traits::ServiceProtocol,
    utils::{is_reachable, multiaddr_to_socketaddr},
    SessionId,
};
use tokio::sync::oneshot;
use tracing::{debug, error, info, trace, warn};

use crate::{
    ckb::{CkbChainMessage, GetBlockTimestampRequest, GetTxResponse},
    fiber::{network::DEFAULT_CHAIN_ACTOR_TIMEOUT, types::secp256k1_instance},
    now_timestamp_as_millis_u64, unwrap_or_return, Error,
};

use super::{
    config::DEFAULT_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS,
    network::{check_chain_hash, get_chain_hash, GossipMessageWithPeerId, GOSSIP_PROTOCOL_ID},
    types::{
        BroadcastMessage, BroadcastMessageID, BroadcastMessageQuery, BroadcastMessageQueryFlags,
        BroadcastMessageWithTimestamp, BroadcastMessagesFilter, BroadcastMessagesFilterResult,
        ChannelAnnouncement, ChannelOnchainInfo, ChannelUpdate, Cursor, GetBroadcastMessages,
        GetBroadcastMessagesResult, GossipMessage, Hash256, NodeAnnouncement, Pubkey,
        QueryBroadcastMessages, QueryBroadcastMessagesResult,
    },
    FiberConfig,
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
const MIN_NUM_OF_PASSIVE_SYNCING_PEERS: usize = 3;

const NUM_SIMULTANEOUS_GET_REQUESTS: usize = 1;
const GET_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

// The maximum number of concurrent query tasks to run. We will wait for query previous task to exit
// before start new query tasks. This is to avoid consuming to much bandwidth.
const MAX_NUM_CONCURRENT_QUERY_TASKS: usize = 10;

const QUERY_BROADCAST_MESSAGES_TIMEOUT: Duration = Duration::from_secs(20);

fn max_acceptable_gossip_message_timestamp() -> u64 {
    now_timestamp_as_millis_u64() + MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT_MILLIS
}

pub trait GossipMessageStore {
    /// The implementors should guarantee that the returned messages are sorted by timestamp in the ascending order.
    fn get_broadcast_messages_iter(
        &self,
        after_cursor: &Cursor,
    ) -> impl IntoIterator<Item = BroadcastMessageWithTimestamp>;

    fn get_broadcast_messages(
        &self,
        after_cursor: &Cursor,
        count: Option<u16>,
    ) -> Vec<BroadcastMessageWithTimestamp> {
        self.get_broadcast_messages_iter(after_cursor)
            .into_iter()
            .take(count.unwrap_or(DEFAULT_NUM_OF_BROADCAST_MESSAGE) as usize)
            .collect()
    }

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
#[rasync_trait]
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

pub enum GossipActorMessage {
    // The control for the service async control is received.
    ReceivedControl(ServiceAsyncControl),

    // Network events to be processed by this actor.
    PeerConnected(PeerId, Pubkey, SessionContext),
    PeerDisconnected(PeerId, SessionContext),

    // The function of TickNetworkMaintenance is to maintain the network state.
    // Currently it will do the following things:
    // 1. Check if we have sufficient number of peers to receive broadcasts. If not, send more BroadcastMessageFilter.
    // 2. Check if there are any pending broadcast message queries. If so, broadcast them to the network.
    TickNetworkMaintenance,

    // Some channels in the gossip store haven't been updated for a long time. We will prune all
    // messages older than the given u64 milliseconds timestamp.
    PruneStaleGossipMessages(u64),

    // The active syncing process is finished for a peer.
    ActiveSyncingFinished(PeerId, Cursor),

    // A malicious peer is found. We should disconnect from the peer.
    MaliciousPeerFound(PeerId),

    // Query some broadcast messages from a peer.
    QueryBroadcastMessages(
        PeerId,
        Vec<BroadcastMessageQuery>,
        RpcReplyPort<Result<QueryBroadcastMessagesResult, GossipError>>,
    ),
    // The querying of broadcast messages from a peer has timed out.
    QueryBroadcastMessagesTimeout(PeerId, u64),
    // Try to broadcast BroadcastMessage created by us to the network.
    // We will save and broadcast the messages. Note that we don't check the dependencies of
    // these messages because we assume that the messages created by us are always valid.
    TryBroadcastMessages(Vec<BroadcastMessageWithTimestamp>),
    // Send gossip message to a peer.
    SendGossipMessage(GossipMessageWithPeerId),
    // Received GossipMessage from a peer
    GossipMessageReceived(GossipMessageWithPeerId),
}

pub(crate) fn get_gossip_actor_name(peer_id: &PeerId) -> String {
    format!("gossip actor {}", peer_id)
}

pub struct GossipConfig {
    pub(crate) peer_id: Option<PeerId>,
    pub(crate) gossip_network_maintenance_interval: Duration,
    pub(crate) gossip_store_maintenance_interval: Duration,
    pub(crate) gossip_store_prune_interval: Duration,
    pub(crate) announce_private_addr: bool,
    pub(crate) num_targeted_active_syncing_peers: usize,
    pub(crate) num_targeted_outbound_passive_syncing_peers: usize,
}

impl Default for GossipConfig {
    fn default() -> Self {
        Self {
            peer_id: None,
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
        }
    }
}

impl From<&FiberConfig> for GossipConfig {
    fn from(config: &FiberConfig) -> Self {
        Self {
            peer_id: None,
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
        }
    }
}

pub struct GossipService<S> {
    extended_store: ExtendedGossipMessageStore<S>,
}

impl<S> GossipService<S>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
{
    pub(crate) async fn start(
        gossip_config: GossipConfig,
        store: S,
        chain_actor: ActorRef<CkbChainMessage>,
        supervisor: ActorCell,
    ) -> (Self, GossipProtocolHandle) {
        let GossipConfig {
            peer_id,
            gossip_network_maintenance_interval,
            gossip_store_maintenance_interval,
            gossip_store_prune_interval,
            announce_private_addr,
            num_targeted_active_syncing_peers,
            num_targeted_outbound_passive_syncing_peers,
        } = gossip_config;

        let (network_control_sender, network_control_receiver) = oneshot::channel();

        let (store_sender, store_receiver) = oneshot::channel();

        let actor_name = peer_id.as_ref().map(get_gossip_actor_name);
        let (actor, _handle) = ActorRuntime::spawn_linked_instant(
            actor_name,
            GossipActor::new(),
            (
                network_control_receiver,
                store_sender,
                gossip_network_maintenance_interval,
                gossip_store_maintenance_interval,
                gossip_store_prune_interval,
                announce_private_addr,
                num_targeted_active_syncing_peers,
                num_targeted_outbound_passive_syncing_peers,
                store,
                chain_actor,
            ),
            supervisor,
        )
        .expect("start gossip actor");
        let store = store_receiver.await.expect("receive store");
        (
            Self {
                extended_store: store,
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

pub(crate) struct GossipActor<S> {
    _phantom: std::marker::PhantomData<S>,
}

impl<S> GossipActor<S>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
{
    fn new() -> Self {
        Self {
            _phantom: Default::default(),
        }
    }
}

#[derive(Debug, Default)]
struct SyncingPeerState {
    failed_times: usize,
}

pub struct GossipSyncingActorState<S> {
    peer_id: PeerId,
    gossip_actor: ActorRef<GossipActorMessage>,
    chain_actor: ActorRef<CkbChainMessage>,
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

impl<S> GossipSyncingActorState<S> {
    fn new(
        peer_id: PeerId,
        gossip_actor: ActorRef<GossipActorMessage>,
        chain_actor: ActorRef<CkbChainMessage>,
        store: ExtendedGossipMessageStore<S>,
        cursor: Cursor,
    ) -> Self {
        Self {
            peer_id,
            gossip_actor,
            chain_actor,
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

pub(crate) struct GossipSyncingActor<S> {
    _phantom: std::marker::PhantomData<S>,
}

impl<S> GossipSyncingActor<S> {
    fn new() -> Self {
        Self {
            _phantom: Default::default(),
        }
    }
}

pub(crate) enum GossipSyncingActorMessage {
    // A GetBroadcastMessages request to the syncing peer has timed out.
    RequestTimeout(u64),
    // A GetBroadcastMessagesResult response from the syncing peer.
    ResponseReceived(GetBroadcastMessagesResult),
    // Initiate a new GetBroadcastMessages request.
    // Mostly triggered by a timeout or a response received.
    NewGetRequest(),
}

#[rasync_trait]
impl<S> Actor for GossipSyncingActor<S>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
{
    type Msg = GossipSyncingActorMessage;
    type State = GossipSyncingActorState<S>;
    type Arguments = (
        PeerId,
        ActorRef<GossipActorMessage>,
        ActorRef<CkbChainMessage>,
        ExtendedGossipMessageStore<S>,
        Cursor,
    );

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (peer_id, gossip_actor, chain_actor, store, cursor): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself
            .send_message(GossipSyncingActorMessage::NewGetRequest())
            .expect("gossip syncing actor alive");
        Ok(GossipSyncingActorState::new(
            peer_id,
            gossip_actor,
            chain_actor,
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
        match message {
            GossipSyncingActorMessage::RequestTimeout(request_id) => {
                state.inflight_requests.remove(&request_id);
                // TODO: When the peer failed for too many times, we should consider disconnecting from the peer.
                state.peer_state.failed_times += 1;
                myself
                    .send_message(GossipSyncingActorMessage::NewGetRequest())
                    .expect("gossip syncing actor alive");
            }
            GossipSyncingActorMessage::ResponseReceived(result) => {
                trace!(
                    "Received GetBroadcastMessages response from peer {:?}: {:?}",
                    &state.peer_id,
                    result
                );
                if let Some(handle) = state.inflight_requests.remove(&result.id) {
                    // Stop the timeout notification.
                    handle.abort();
                    let messages = result.messages;
                    // If we are receiving an empty response, then the syncing process is finished.
                    match messages.last() {
                        Some(last_message) => {
                            // We need the message timestamp to construct a valid cursor.
                            match get_message_cursor(
                                last_message,
                                &state.store.store,
                                &state.chain_actor,
                            )
                            .await
                            {
                                Ok(cursor) => {
                                    state.cursor = cursor;
                                }
                                Err(error) => {
                                    warn!(
                                        "Failed to verify the last message in the response: message {:?}, peer {:?}",
                                        error, &state.peer_id
                                    );
                                    myself.stop(Some(
                                        "Failed to verify the last message in the response"
                                            .to_string(),
                                    ));
                                    state
                                        .gossip_actor
                                        .send_message(GossipActorMessage::MaliciousPeerFound(
                                            state.peer_id.clone(),
                                        ))
                                        .expect("gossip actor alive");
                                    return Ok(());
                                }
                            }
                        }
                        None => {
                            state
                                .gossip_actor
                                .send_message(GossipActorMessage::ActiveSyncingFinished(
                                    state.peer_id.clone(),
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
                            state.peer_id.clone(),
                            messages,
                        ))
                        .expect("store actor alive");
                    trace!("Sending new GetBroadcastMessages request after receiving response: peer_id {:?}", &state.peer_id);
                    myself
                        .send_message(GossipSyncingActorMessage::NewGetRequest())
                        .expect("gossip syncing actor alive");
                } else {
                    warn!(
                        "Received GetBroadcastMessages response from peer {:?} with unknown request id: {:?}",
                        state.peer_id, result
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
                        GossipMessageWithPeerId {
                            peer_id: state.peer_id.clone(),
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
        peer: PeerId,
        filter: Cursor,
        gossip_actor: ActorRef<GossipActorMessage>,
    ) -> Self
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
        .await
        .expect("start peer filter processor actor");
        Self { filter, actor }
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
    peer: PeerId,
    gossip_actor: ActorRef<GossipActorMessage>,
}

enum PeerFilterProcessorMessage {
    NewStoreUpdates(GossipMessageUpdates),
    UpdateFilter(Cursor),
}

#[rasync_trait]
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
                            GossipMessageWithPeerId {
                                peer_id: self.peer.clone(),
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
    async fn new(
        maintenance_interval: Duration,
        announce_private_addr: bool,
        store: S,
        gossip_actor: ActorRef<GossipActorMessage>,
        chain_actor: ActorRef<CkbChainMessage>,
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
                store.clone(),
                gossip_actor,
                chain_actor,
            ),
            supervisor,
        )
        .await
        .expect("start gossip message actor store");

        Self { store, actor }
    }
}

#[rasync_trait]
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

pub struct ExtendedGossipMessageStoreState<S> {
    announce_private_addr: bool,
    store: S,
    gossip_actor: ActorRef<GossipActorMessage>,
    chain_actor: ActorRef<CkbChainMessage>,
    next_id: u64,
    output_ports: HashMap<u64, BroadcastMessageOutput>,
    // A map from peer_id to the messages that need to be saved.
    // Our own messages are always saved directly to the store.
    messages_to_be_saved: HashMap<PeerId, HashSet<BroadcastMessage>>,
    num_query_tasks_running: usize,
}

impl<S: GossipMessageStore> ExtendedGossipMessageStoreState<S> {
    fn new(
        announce_private_addr: bool,
        store: S,
        gossip_actor: ActorRef<GossipActorMessage>,
        chain_actor: ActorRef<CkbChainMessage>,
    ) -> Self {
        Self {
            announce_private_addr,
            store,
            gossip_actor,
            chain_actor,
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
        let mut complete_messages = HashSet::new();
        for messages in self.messages_to_be_saved.values() {
            for message in messages {
                if self.has_dependencies_available(message) {
                    complete_messages.insert(message.clone());
                }
            }
        }
        self.messages_to_be_saved.retain(|_, messages| {
            // Remove completed messages
            messages.retain(|message| !complete_messages.contains(message));
            // If messages are empty now, delete the kv pair.
            !messages.is_empty()
        });

        let mut sorted_messages = complete_messages.into_iter().collect::<Vec<_>>();
        sorted_messages.sort_unstable();
        trace!(
            "Saving complete messages to the store: {:?}",
            &sorted_messages
        );

        let mut verified_sorted_messages = Vec::with_capacity(sorted_messages.len());
        for message in sorted_messages {
            match verify_and_save_broadcast_message(&message, &self.store, &self.chain_actor).await
            {
                Ok(message) => {
                    verified_sorted_messages.push(message);
                }
                Err(error) => {
                    trace!(
                        "Failed to verify and save message {:?}: {:?}",
                        message,
                        error
                    );
                }
            }
        }

        verified_sorted_messages
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
            let gossip_actor = self.gossip_actor.clone();
            let myself = myself.clone();
            let incomplete_messages = incomplete_messages.into_iter().collect::<Vec<_>>();

            ractor::concurrency::tokio_primitives::spawn(async move {
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
                    match call!(
                        gossip_actor,
                        GossipActorMessage::QueryBroadcastMessages,
                        peer.clone(),
                        queries.to_vec()
                    ) {
                        Ok(Ok(result)) => {
                            let mut all_messages = result.messages;
                            // We need also to save the incomplete messages to the store.
                            all_messages.extend(messages.iter().map(Clone::clone));
                            myself
                                .send_message(ExtendedGossipMessageStoreMessage::SaveMessages(
                                    peer.clone(),
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

    fn get_channel_annnouncement(&self, outpoint: &OutPoint) -> Option<ChannelAnnouncement> {
        self.store
            .get_latest_channel_announcement(outpoint)
            .map(|(_, m)| m)
            .or_else(|| self.get_channel_annnouncement_in_memory(outpoint))
    }

    fn get_channel_annnouncement_in_memory(
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
        peer_id: &PeerId,
        message: &BroadcastMessage,
    ) -> Result<(), GossipMessageProcessingError> {
        if let Some(existing_messages) = self.messages_to_be_saved.get(peer_id) {
            if existing_messages.contains(message) {
                return Ok(());
            }
        }

        if let Some(existing_message) = get_existing_newer_broadcast_message(message, &self.store) {
            if &BroadcastMessage::from(existing_message.clone()) != message {
                return Err(GossipMessageProcessingError::NewerMessageSaved(
                    existing_message,
                ));
            } else {
                return Ok(());
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

        if !self.announce_private_addr {
            if let BroadcastMessage::NodeAnnouncement(node_announcement) = &message {
                if !node_announcement.addresses.iter().any(|addr| {
                    multiaddr_to_socketaddr(addr)
                        .map(|socket_addr| is_reachable(socket_addr.ip()))
                        .unwrap_or_default()
                }) {
                    return Err(GossipMessageProcessingError::ProcessingError(
                        "private address node announcement".to_string(),
                    ));
                }
            }
        }

        trace!(
            "New gossip message saved to memory: peer {:?}, message {:?}",
            peer_id,
            message
        );
        self.messages_to_be_saved
            .entry(peer_id.clone())
            .or_default()
            .insert(message.clone());
        Ok(())
    }

    fn has_dependencies_available(&self, message: &BroadcastMessage) -> bool {
        match message {
            BroadcastMessage::ChannelUpdate(channel_update) => self
                .get_channel_annnouncement(&channel_update.channel_outpoint)
                .is_some(),
            _ => true,
        }
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
struct ExtendedGossipMessageStoreActor<S> {
    phantom: PhantomData<S>,
}

impl<S: GossipMessageStore> ExtendedGossipMessageStoreActor<S> {
    fn new() -> Self {
        Self {
            phantom: Default::default(),
        }
    }
}

#[rasync_trait]
impl<S: GossipMessageStore + Send + Sync + 'static> Actor for ExtendedGossipMessageStoreActor<S> {
    type Msg = ExtendedGossipMessageStoreMessage;
    type State = ExtendedGossipMessageStoreState<S>;
    type Arguments = (
        Duration,
        bool,
        S,
        ActorRef<GossipActorMessage>,
        ActorRef<CkbChainMessage>,
    );

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (
            gossip_store_maintenance_interval,
            announce_private_addr,
            store,
            gossip_actor,
            chain_actor,
        ): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_interval(gossip_store_maintenance_interval, || {
            ExtendedGossipMessageStoreMessage::Tick
        });
        Ok(ExtendedGossipMessageStoreState::new(
            announce_private_addr,
            store,
            gossip_actor,
            chain_actor,
        ))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
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
                    .get_broadcast_messages(&cursor, Some(DEFAULT_NUM_OF_BROADCAST_MESSAGE))
                    .into_iter()
                    .collect::<Vec<_>>();
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
                for message in messages {
                    if let Err(error) = state.insert_message_to_be_saved_list(&peer, &message).await
                    {
                        trace!("Failed to save message: {:?}, error: {:?}", message, error);
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
    peer: PeerId,
    is_success: bool,
}

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
    SaveMessages(PeerId, Vec<BroadcastMessage>),
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

pub(crate) struct GossipActorState<S> {
    store: ExtendedGossipMessageStore<S>,
    control: Option<ServiceAsyncControl>,
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
    query_reply_ports:
        HashMap<(PeerId, u64), RpcReplyPort<Result<QueryBroadcastMessagesResult, GossipError>>>,
    peer_states: HashMap<PeerId, PeerState>,
}

impl<S> GossipActorState<S>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
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

    fn outbound_passive_syncing_peers(&self) -> Vec<PeerId> {
        self.peer_states
            .iter()
            .filter_map(|(peer_id, state)| {
                (state.sync_status.is_passive_syncing() && state.session_type.is_outbound())
                    .then_some(peer_id.clone())
            })
            .collect()
    }

    fn passive_syncing_peers(&self) -> Vec<PeerId> {
        self.peer_states
            .iter()
            .filter_map(|(peer_id, state)| {
                state
                    .sync_status
                    .is_passive_syncing()
                    .then_some(peer_id.clone())
            })
            .collect()
    }

    fn peers_to_start_active_syncing(&self) -> Vec<PeerId> {
        match self.num_targeted_active_syncing_peers.checked_sub(
            self.num_finished_active_syncing_peers + self.num_of_active_syncing_peers(),
        ) {
            None => vec![],
            Some(num) => self
                .peer_states
                .iter()
                .filter(|(_, state)| state.sync_status.can_start_active_syncing())
                .take(num)
                .map(|(peer_id, _)| peer_id)
                .cloned()
                .collect(),
        }
    }

    fn new_outbound_peers_to_start_passive_syncing(&self) -> Vec<PeerId> {
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
                .map(|(peer_id, _)| peer_id)
                .cloned()
                .collect(),
        }
    }

    fn peers_to_start_passive_syncing(&self) -> Vec<PeerId> {
        [
            self.peers_to_start_mutual_passive_syncing().as_slice(),
            self.new_outbound_peers_to_start_passive_syncing()
                .as_slice(),
        ]
        .concat()
    }

    fn peers_to_start_mutual_passive_syncing(&self) -> Vec<PeerId> {
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
            .map(|(peer_id, _)| peer_id.clone())
            .collect::<Vec<_>>()
    }

    async fn start_new_active_syncer(&mut self, peer_id: &PeerId) {
        let safe_cursor = self.get_safe_cursor_to_start_syncing();
        let sync_actor = Actor::spawn_linked(
            Some(format!(
                "gossip syncing actor to peer {:?} supervised by {:?}",
                peer_id,
                self.myself.get_id()
            )),
            GossipSyncingActor::new(),
            (
                peer_id.clone(),
                self.myself.clone(),
                self.chain_actor.clone(),
                self.store.clone(),
                safe_cursor,
            ),
            self.myself.get_cell(),
        )
        .await
        .expect("start gossip syncing actor");

        self.peer_states
            .get_mut(peer_id)
            .expect("get peer state")
            .change_sync_status(PeerSyncStatus::ActiveGet(sync_actor.0));
    }

    async fn start_passive_syncer(&mut self, peer_id: &PeerId) {
        let cursor = self.get_safe_cursor_to_start_syncing();
        let filter = BroadcastMessagesFilter {
            chain_hash: get_chain_hash(),
            after_cursor: cursor.clone(),
        };

        match self.send_broadcast_message_filter(peer_id, filter).await {
            Ok(_) => {
                self.peer_states
                    .get_mut(peer_id)
                    .expect("get peer state")
                    .change_sync_status(PeerSyncStatus::PassiveFilter(cursor));
            }
            Err(e) => {
                error!(
                    "Failed to send BroadcastMessagesFilter to peer {:?}: {:?}",
                    peer_id, e
                );
            }
        }
    }

    async fn send_broadcast_message_filter(
        &self,
        peer_id: &PeerId,
        filter: BroadcastMessagesFilter,
    ) -> crate::Result<()> {
        let message = GossipMessage::BroadcastMessagesFilter(filter);
        self.send_message_to_peer(peer_id, message).await?;
        Ok(())
    }

    fn is_peer_connected(&self, peer_id: &PeerId) -> bool {
        self.peer_states.contains_key(peer_id)
    }

    fn get_store(&self) -> &S {
        self.store.get_store()
    }

    fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        self.peer_states.get(peer_id).map(|s| s.session_id)
    }

    fn get_latest_cursor(&self) -> Cursor {
        self.get_store()
            .get_latest_broadcast_message_cursor()
            .unwrap_or_default()
    }

    // A cursor that is "safe" to start syncing from. By "safe" we mean that
    // the node is mostly having the messages that are newer than this cursor or the messages
    // before this cursor are not important for the node to sync with the network.
    // We will start syncing from this cursor to avoid syncing from the very beginning of the network.
    fn get_safe_cursor_to_start_syncing(&self) -> Cursor {
        let latest_cursor_timestamp = self.get_latest_cursor().timestamp;
        let safe_cursor_timestamp = latest_cursor_timestamp
            .saturating_sub(MAX_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT.as_millis() as u64);
        let timestamp_after_considered_stale = now_timestamp_as_millis_u64()
            .saturating_sub(SOFT_BROADCAST_MESSAGES_CONSIDERED_STALE_DURATION.as_millis() as u64);
        let timestamp = max(safe_cursor_timestamp, timestamp_after_considered_stale);
        Cursor::new(timestamp, BroadcastMessageID::default())
    }

    async fn try_to_verify_and_save_broadcast_messages(
        &mut self,
        originator: PeerId,
        messages: Vec<BroadcastMessage>,
    ) {
        self.store
            .actor
            .send_message(ExtendedGossipMessageStoreMessage::SaveMessages(
                originator, messages,
            ))
            .expect("store actor alive");
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

    async fn send_message_to_peer(
        &self,
        peer_id: &PeerId,
        message: GossipMessage,
    ) -> crate::Result<()> {
        match self.get_peer_session(peer_id) {
            Some(session_id) => self.send_message_to_session(session_id, message).await,
            None => Err(Error::PeerNotFound(peer_id.clone())),
        }
    }

    fn get_and_increment_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }
}

async fn send_message_to_session(
    control: &ServiceAsyncControl,
    session_id: SessionId,
    message: GossipMessage,
) -> crate::Result<()> {
    control
        .send_message_to(session_id, GOSSIP_PROTOCOL_ID, message.to_molecule_bytes())
        .await?;
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
) -> Result<Cursor, Error> {
    match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
            let timestamp =
                get_channel_timestamp(&channel_announcement.channel_outpoint, store, chain).await?;
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
) -> Result<BroadcastMessageWithTimestamp, Error> {
    let timestamp = match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
            let on_chain_info =
                get_channel_on_chain_info(channel_announcement.out_point(), chain).await?;
            if !verify_channel_announcement(channel_announcement, &on_chain_info, store).await? {
                store.save_channel_announcement(
                    on_chain_info.timestamp,
                    channel_announcement.clone(),
                );
            }
            on_chain_info.timestamp
        }
        BroadcastMessage::ChannelUpdate(channel_update) => {
            if !verify_channel_update(channel_update, store)? {
                store.save_channel_update(channel_update.clone());
            }
            channel_update.timestamp
        }
        BroadcastMessage::NodeAnnouncement(node_announcement) => {
            if !verify_node_announcement(node_announcement, store)? {
                store.save_node_announcement(node_announcement.clone());
            }
            node_announcement.timestamp
        }
    };
    Ok((message.clone(), timestamp).into())
}

async fn get_channel_tx(
    outpoint: &OutPoint,
    chain: &ActorRef<CkbChainMessage>,
) -> Result<(TransactionView, Hash256), Error> {
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

    match call_t!(
        chain,
        CkbChainMessage::GetTx,
        DEFAULT_CHAIN_ACTOR_TIMEOUT,
        outpoint.tx_hash().into()
    ) {
        Ok(Ok(GetTxResponse {
            transaction: Some(tx),
            tx_status: TxStatus::Committed(_, block_hash, _)
        })) => Ok((tx, block_hash.into())),
        Ok(Err(err)) => Err(Error::InvalidParameter(format!(
            "Channel announcement transaction {:?} not found or not confirmed, result is: {:?}",
            &outpoint.tx_hash(),
            err
        ))),
        Err(err) => Err(Error::InvalidParameter(format!(
            "Channel announcement transaction {:?} not found or not confirmed, result is: {:?}",
            &outpoint.tx_hash(),
            err
        ))),
        _ => Err(Error::InvalidParameter(format!(
            "Channel announcement transaction {:?} not found or not confirmed, the reason is unknown",
            &outpoint.tx_hash(),
        ))),
    }
}

async fn get_channel_timestamp<S: GossipMessageStore>(
    outpoint: &OutPoint,
    store: &S,
    chain: &ActorRef<CkbChainMessage>,
) -> Result<u64, Error> {
    if let Some((timestamp, _)) = store.get_latest_channel_announcement(outpoint) {
        return Ok(timestamp);
    }

    let on_chain_info = get_channel_on_chain_info(outpoint, chain).await?;

    Ok(on_chain_info.timestamp)
}

async fn get_channel_on_chain_info(
    outpoint: &OutPoint,
    chain: &ActorRef<CkbChainMessage>,
) -> Result<ChannelOnchainInfo, Error> {
    let (tx, block_hash) = get_channel_tx(outpoint, chain).await?;
    let first_output = match tx.outputs().get(0) {
        None => {
            return Err(Error::InvalidParameter(format!(
                "On-chain transaction found but no output: {:?}",
                &outpoint
            )));
        }
        Some(output) => output.clone().into(),
    };

    let timestamp: u64 = match call_t!(
        chain,
        CkbChainMessage::GetBlockTimestamp,
        DEFAULT_CHAIN_ACTOR_TIMEOUT,
        GetBlockTimestampRequest::from_block_hash(block_hash)
    ) {
        Ok(Ok(Some(timestamp))) => timestamp,
        Ok(Ok(None)) => {
            return Err(Error::InternalError(anyhow::anyhow!(
                "Unable to find block {:?} for channel outpoint {:?}",
                &block_hash,
                &outpoint
            )));
        }
        Ok(Err(err)) => {
            return Err(Error::CkbRpcError(err));
        }
        Err(err) => {
            return Err(Error::InternalError(anyhow::Error::new(err).context(
                format!(
                    "Error while trying to obtain block {:?} for channel outpoint {:?}",
                    block_hash, &outpoint
                ),
            )));
        }
    };

    Ok(ChannelOnchainInfo {
        timestamp,
        first_output,
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
) -> Result<bool, Error> {
    if let Some((_, announcement)) =
        store.get_latest_channel_announcement(&channel_announcement.channel_outpoint)
    {
        if announcement == *channel_announcement {
            return Ok(true);
        } else {
            return Err(Error::InvalidParameter(format!(
                "Channel announcement message already exists but mismatched: {:?}, existing: {:?}",
                &channel_announcement, &announcement
            )));
        }
    }
    let message = channel_announcement.message_to_sign();
    if channel_announcement.node1_id == channel_announcement.node2_id {
        return Err(Error::InvalidParameter(format!(
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
            return Err(Error::InvalidParameter(format!(
                "Channel announcement message signature verification failed, some signatures are missing: {:?}",
                &channel_announcement
            )));
        }
    };

    if !node1_signature.verify(&channel_announcement.node1_id, &message) {
        return Err(Error::InvalidParameter(format!(
            "Channel announcement message signature verification failed for node 1: {:?}, message: {:?}, signature: {:?}, pubkey: {:?}",
            &channel_announcement,
            &message,
            &node1_signature,
            &channel_announcement.node1_id
        )));
    }

    if !node2_signature.verify(&channel_announcement.node2_id, &message) {
        return Err(Error::InvalidParameter(format!(
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
        return Err(Error::InvalidParameter(format!(
                    "On-chain transaction found but pubkey hash mismatched: on chain hash {:?}, pub key ({:?}) hash {:?}",
                    &output.lock.args.as_bytes(),
                    hex::encode(pubkey),
                    &pubkey_hash
                )));
    }
    let capacity: u128 = u64::from(output.capacity).into();
    match channel_announcement.udt_type_script {
        Some(_) => {
            // TODO: verify the capacity of the UDT
        }
        None => {
            if channel_announcement.capacity > capacity {
                return Err(Error::InvalidParameter(format!(
                            "On-chain transaction found but capacity mismatched: on chain capacity {:?} smaller than annoucned channel capacity {:?}",
                            &output.capacity, &channel_announcement.capacity
                        )));
            }
        }
    }

    if let Err(err) = secp256k1_instance().verify_schnorr(
        ckb_signature,
        &Message::from_digest(message),
        &channel_announcement.ckb_key,
    ) {
        return Err(Error::InvalidParameter(format!(
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
) -> Result<bool, Error> {
    if let Some(BroadcastMessageWithTimestamp::ChannelUpdate(existing)) =
        store.get_broadcast_message_with_cursor(&channel_update.cursor())
    {
        if existing == *channel_update {
            return Ok(true);
        } else {
            return Err(Error::InvalidParameter(format!(
                "Channel update message already exists but mismatched: {:?}, existing: {:?}",
                &channel_update, &existing
            )));
        }
    }
    let message = channel_update.message_to_sign();

    let signature = match channel_update.signature {
        Some(ref signature) => signature,
        None => {
            return Err(Error::InvalidParameter(format!(
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
                return Err(Error::InvalidParameter(format!(
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
            return Err(Error::InvalidParameter(format!(
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
) -> Result<bool, Error> {
    if let Some(BroadcastMessageWithTimestamp::NodeAnnouncement(announcement)) =
        store.get_broadcast_message_with_cursor(&node_announcement.cursor())
    {
        if announcement == *node_announcement {
            return Ok(true);
        } else {
            return Err(Error::InvalidParameter(format!(
                "Node announcement message already exists but mismatched: {:?}, existing: {:?}",
                &node_announcement, &announcement
            )));
        }
    }
    if !node_announcement.verify() {
        Err(Error::InvalidParameter(
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
            .service_handle(move || {
                let handle = Box::new(self);
                ProtocolHandle::Callback(handle)
            })
            .build()
    }
}

#[rasync_trait]
impl<S> Actor for GossipActor<S>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
{
    type Msg = GossipActorMessage;
    type State = GossipActorState<S>;
    type Arguments = (
        oneshot::Receiver<ServiceAsyncControl>,
        oneshot::Sender<ExtendedGossipMessageStore<S>>,
        Duration,
        Duration,
        Duration,
        bool,
        usize,
        usize,
        S,
        ActorRef<CkbChainMessage>,
    );

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (
            rx,
            tx,
            network_maintenance_interval,
            store_maintenance_interval,
            store_prune_interval,
            announce_private_addr,
            num_targeted_active_syncing_peers,
            num_targeted_outbound_passive_syncing_peers,
            store,
            chain_actor,
        ): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let store = ExtendedGossipMessageStore::new(
            store_maintenance_interval,
            announce_private_addr,
            store,
            myself.clone(),
            chain_actor.clone(),
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
        let state = Self::State {
            store,
            control: Default::default(),
            num_targeted_active_syncing_peers,
            num_targeted_outbound_passive_syncing_peers,
            myself,
            chain_actor,
            next_request_id: Default::default(),
            query_reply_ports: Default::default(),
            num_finished_active_syncing_peers: Default::default(),
            peer_states: Default::default(),
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
        match message {
            GossipActorMessage::ReceivedControl(control) => {
                state.control = Some(control);
            }

            GossipActorMessage::PeerConnected(peer_id, _pubkey, session) => {
                if state.is_peer_connected(&peer_id) {
                    return Ok(());
                }
                state
                    .peer_states
                    .insert(peer_id.clone(), PeerState::new(session.id, session.ty));
            }
            GossipActorMessage::PeerDisconnected(peer_id, _session) => {
                state.peer_states.remove(&peer_id);
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
                        state.query_reply_ports.insert((peer.clone(), id), reply);
                        myself.send_after(QUERY_BROADCAST_MESSAGES_TIMEOUT, move || {
                            GossipActorMessage::QueryBroadcastMessagesTimeout(peer.clone(), id)
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

            GossipActorMessage::TickNetworkMaintenance => {
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

            GossipActorMessage::ActiveSyncingFinished(peer_id, cursor) => {
                state.num_finished_active_syncing_peers += 1;
                if let Some(peer_state) = state.peer_states.get_mut(&peer_id) {
                    peer_state.change_sync_status(PeerSyncStatus::FinishedActiveSyncing(
                        now_timestamp_as_millis_u64(),
                        cursor,
                    ));
                }
            }

            GossipActorMessage::MaliciousPeerFound(peer_id) => {
                warn!("Malicious peer found: {:?}", &peer_id);
            }

            GossipActorMessage::SendGossipMessage(GossipMessageWithPeerId { peer_id, message }) => {
                if let Err(error) = state.send_message_to_peer(&peer_id, message).await {
                    error!(
                        "Failed to send gossip message to peer {:?}: {:?}",
                        &peer_id, error
                    );
                }
            }

            GossipActorMessage::GossipMessageReceived(GossipMessageWithPeerId {
                peer_id,
                message,
            }) => {
                match message {
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
                                &peer_id
                            );
                            state.peer_states.remove(&peer_id);
                            return Ok(());
                        }
                        match state.peer_states.get_mut(&peer_id) {
                            Some(peer_state) => {
                                match peer_state.filter_processor.as_mut() {
                                    Some(filter_processor) => {
                                        filter_processor.update_filter(&after_cursor);
                                        return Ok(());
                                    }
                                    _ => {
                                        peer_state.filter_processor = Some(
                                            PeerFilterProcessor::new(
                                                state.store.clone(),
                                                peer_id.clone(),
                                                after_cursor.clone(),
                                                myself,
                                            )
                                            .await,
                                        );
                                    }
                                };
                                // Also start passive syncer to peer so that we have less silos.
                                if peer_state.sync_status.can_start_passive_syncing() {
                                    state.start_passive_syncer(&peer_id).await;
                                }
                            }
                            None => {
                                warn!(
                                    "Received BroadcastMessagesFilter from unknown peer: {:?}",
                                    &peer_id
                                );
                                return Ok(());
                            }
                        };
                    }
                    GossipMessage::BroadcastMessagesFilterResult(
                        BroadcastMessagesFilterResult { messages },
                    ) => {
                        state
                            .try_to_verify_and_save_broadcast_messages(peer_id, messages)
                            .await;
                    }
                    GossipMessage::GetBroadcastMessages(get_broadcast_messages) => {
                        if let Err(e) = check_chain_hash(&get_broadcast_messages.chain_hash) {
                            error!("Failed to check chain hash: {:?}", e);
                            return Ok(());
                        }
                        if get_broadcast_messages.count > MAX_NUM_OF_BROADCAST_MESSAGES {
                            warn!(
                                "Received GetBroadcastMessages with too many messages: {:?}",
                                get_broadcast_messages.count
                            );
                            return Ok(());
                        }
                        let id = get_broadcast_messages.id;
                        let messages = state.get_store().get_broadcast_messages(
                            &get_broadcast_messages.after_cursor,
                            Some(get_broadcast_messages.count),
                        );
                        let result =
                            GossipMessage::GetBroadcastMessagesResult(GetBroadcastMessagesResult {
                                id,
                                messages: messages.into_iter().map(|m| m.into()).collect(),
                            });
                        if let Err(error) = state.send_message_to_peer(&peer_id, result).await {
                            error!(
                                "Failed to send GetBroadcastMessagesResult to peer {:?}: {:?}",
                                &peer_id, error
                            );
                        }
                    }
                    GossipMessage::GetBroadcastMessagesResult(result) => {
                        let peer_state = state.peer_states.get(&peer_id);
                        if let Some(PeerState {
                            sync_status: PeerSyncStatus::ActiveGet(actor),
                            ..
                        }) = peer_state
                        {
                            let _ = actor
                                .send_message(GossipSyncingActorMessage::ResponseReceived(result));
                        } else {
                            warn!(
                                "Received GetBroadcastMessagesResult from peer {:?} in state {:?}",
                                &peer_id, &peer_state
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
                        let result = GossipMessage::QueryBroadcastMessagesResult(
                            QueryBroadcastMessagesResult {
                                id,
                                messages: results.into_iter().map(|m| m.into()).collect(),
                                missing_queries,
                            },
                        );
                        if let Err(error) = state.send_message_to_peer(&peer_id, result).await {
                            error!(
                                "Failed to send QueryBroadcastMessagesResult to peer {:?}: {:?}",
                                &peer_id, error
                            );
                        }
                    }
                    GossipMessage::QueryBroadcastMessagesResult(result) => {
                        if let Some(reply) = state
                            .query_reply_ports
                            .remove(&(peer_id.clone(), result.id))
                        {
                            let _ = reply.send(Ok(result));
                        }
                    }
                }
            }
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
            let remote_peer_id = PeerId::from_public_key(&remote_pubkey);
            let _ = self.actor.send_message(GossipActorMessage::PeerConnected(
                remote_peer_id,
                remote_pubkey.into(),
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
                let remote_peer_id = PeerId::from_public_key(remote_pubkey);
                let _ = self
                    .actor
                    .send_message(GossipActorMessage::PeerDisconnected(
                        remote_peer_id,
                        context.session.clone(),
                    ));
            }
            None => {
                unreachable!("Received message without remote pubkey");
            }
        }
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        let message = unwrap_or_return!(GossipMessage::from_molecule_slice(&data), "parse message");
        match context.session.remote_pubkey.as_ref() {
            Some(pubkey) => {
                let peer_id = PeerId::from_public_key(pubkey);
                let _ = self
                    .actor
                    .send_message(GossipActorMessage::GossipMessageReceived(
                        GossipMessageWithPeerId { peer_id, message },
                    ));
            }
            None => {
                unreachable!("Received message without remote pubkey");
            }
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}
