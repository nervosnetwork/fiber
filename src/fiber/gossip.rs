use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
    sync::Arc,
    time::Duration,
};

use ckb_hash::blake2b_256;
use ckb_jsonrpc_types::{Status, TxStatus};
use ckb_types::packed::OutPoint;
use ractor::{
    async_trait as rasync_trait, call, call_t,
    concurrency::{timeout, JoinHandle},
    Actor, ActorCell, ActorProcessingErr, ActorRef, ActorRuntime, MessagingErr, OutputPort,
    RpcReplyPort, SupervisionEvent,
};
use secp256k1::Message;
use tentacle::{
    async_trait as tasync_trait,
    builder::MetaBuilder,
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, SessionContext},
    secio::PeerId,
    service::{ProtocolHandle, ProtocolMeta, ServiceAsyncControl},
    traits::ServiceProtocol,
    SessionId,
};
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, error, info, trace, warn};

use crate::{
    ckb::{CkbChainMessage, GetBlockTimestampRequest, TraceTxRequest, TraceTxResponse},
    fiber::{network::DEFAULT_CHAIN_ACTOR_TIMEOUT, types::secp256k1_instance},
    now_timestamp_as_millis_u64, unwrap_or_return, Error,
};

use super::{
    network::{check_chain_hash, get_chain_hash, GossipMessageWithPeerId, GOSSIP_PROTOCOL_ID},
    types::{
        BroadcastMessage, BroadcastMessageID, BroadcastMessageQuery, BroadcastMessageQueryFlags,
        BroadcastMessageWithTimestamp, BroadcastMessagesFilter, BroadcastMessagesFilterResult,
        ChannelAnnouncement, ChannelUpdate, Cursor, GetBroadcastMessages,
        GetBroadcastMessagesResult, GossipMessage, NodeAnnouncement, Pubkey,
        QueryBroadcastMessages, QueryBroadcastMessagesResult,
    },
};

// The maximum duration drift between the broadcast message timestamp and latest cursor in store.
pub(crate) const MAX_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT: Duration =
    Duration::from_secs(60 * 60 * 2);

const MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT: Duration = Duration::from_secs(60);
const MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT_MILLIS: u64 =
    MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT.as_millis() as u64;

const MAX_NUM_OF_BROADCAST_MESSAGES: u16 = 1000;
pub(crate) const DEFAULT_NUM_OF_BROADCAST_MESSAGE: u16 = 100;

const MAX_NUM_OF_ACTIVE_SYNCING_PEERS: usize = 1;
const MIN_NUM_OF_PASSIVE_SYNCING_PEERS: usize = 2;

const NUM_SIMULTANEOUS_GET_REQUESTS: usize = 1;
const GET_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

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
            .take(count.unwrap_or(DEFAULT_NUM_OF_BROADCAST_MESSAGE as u16) as usize)
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
            BroadcastMessageQueryFlags::ChannelUpdateNode1 => self
                .get_latest_channel_update(&query.channel_outpoint, true)
                .map(|channel_update| BroadcastMessageWithTimestamp::ChannelUpdate(channel_update)),
            BroadcastMessageQueryFlags::ChannelUpdateNode2 => self
                .get_latest_channel_update(&query.channel_outpoint, false)
                .map(|channel_update| BroadcastMessageWithTimestamp::ChannelUpdate(channel_update)),

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
                        .map(|m| BroadcastMessageWithTimestamp::NodeAnnouncement(m))
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
                )).and_then(|message| match message {
                    BroadcastMessageWithTimestamp::ChannelAnnouncement(
                        _,
                        channel_announcement,
                    ) => Some((timestamp, channel_announcement)),
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
                )).and_then(|message| match message {
                    BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => Some(channel_update),
                    _ => panic!("get_latest_channel_update returned non-ChannelUpdate message from db: channel outpoint {:?}, is_node1 {:?}, message {:?}", outpoint, is_node1, message),
                })
            })
    }

    fn get_latest_node_announcement(&self, pk: &Pubkey) -> Option<NodeAnnouncement> {
        self.get_latest_node_announcement_timestamp(pk).and_then(|timestamp| {
            self.get_broadcast_message_with_cursor(&Cursor::new(
                timestamp,
                BroadcastMessageID::NodeAnnouncement(pk.clone()),
            )).and_then(|message|
                    match message {
                    BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => Some(node_announcement),
                    _ => panic!("get_lastest_node_announcement returned non-NodeAnnouncement message from db: pk {:?}, message {:?}", pk, message),
                    }
                )
            }
        )
    }

    fn save_channel_announcement(&self, timestamp: u64, channel_announcement: ChannelAnnouncement);

    fn save_channel_update(&self, channel_update: ChannelUpdate);

    fn save_node_announcement(&self, node_announcement: NodeAnnouncement);
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
    /// The cursor here specifies the starting point of the subscription. If it is None, the subscription
    /// will start from the very latest message in the store.
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
        cursor: Option<Cursor>,
        receiver: ActorRef<TReceiverMsg>,
        converter: F,
    ) -> Result<Self::Subscription, Self::Error>;

    /// Unsubscribe from the gossip message store updates. The subscription parameter is the return value
    /// of the subscribe function. The new cursor will be used to determine the starting point of the
    /// next batch of messages that will be sent to the receiver.
    async fn update_subscription(
        &self,
        subscription: &Self::Subscription,
        cursor: Option<Cursor>,
    ) -> Result<(), Self::Error>;

    // Unsubscribe from the gossip message store updates. The subscription parameter is the return value
    // of the subscribe function. After this function is called, the receiver will no longer
    // receive messages from the store.
    async fn unsubscribe(&self, subscription: &Self::Subscription) -> Result<(), Self::Error>;
}

#[derive(Debug)]
pub(crate) enum GossipActorMessage {
    // Network events to be processed by this actor.
    PeerConnected(PeerId, Pubkey, SessionContext),
    PeerDisconnected(PeerId, SessionContext),

    // The function of TickNetworkMaintenance is to maintain the network state.
    // Currently it will do the following things:
    // 1. Check if we have sufficient number of peers to receive broadcasts. If not, send more BroadcastMessageFilter.
    // 2. Check if there are any pending broadcast messages. If so, broadcast them to the network.
    TickNetworkMaintenance,

    // The active syncing process is finished for a peer.
    ActiveSyncingFinished(PeerId, Cursor),

    // A malicious peer is found. We should disconnect from the peer.
    MaliciousPeerFound(PeerId),

    // Process BroadcastMessage from the network. This is mostly used to save a broadcast message
    // not received from gossip message protocol to the store. Examples of such messages are
    // our own node announcement messages, channel updates from the onion error packets, etc.
    ProcessBroadcastMessage(BroadcastMessage),
    // Try to broadcast BroadcastMessage created by us to the network.
    // These messages will be saved to the store and when their dependencies are met,
    // we will broadcast them to the network. If not, we will wait for the dependencies
    // to be met.
    TryBroadcastMessages(Vec<BroadcastMessage>),
    // Broadcast a message to the network. The message here must have all its dependencies met.
    // This is normally the case when we saved a message to the store.
    BroadcastMessageImmediately(BroadcastMessageWithTimestamp),
    // Send gossip message to a peer.
    SendGossipMessage(GossipMessageWithPeerId),
    // Received GossipMessage from a peer
    GossipMessageReceived(GossipMessageWithPeerId),
}

pub(crate) struct GossipActor<S> {
    _phantom: std::marker::PhantomData<S>,
}

impl<S> GossipActor<S> {
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
    // The problem of using different cursor for different peers is that we may waste
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

    // fn select_a_node(&self) -> Option<&PeerId> {
    //     self.select_n_nodes(1).into_iter().next()
    // }

    // fn select_n_nodes(&self, n: usize) -> impl IntoIterator<Item = &PeerId> {
    //     let mut peers = self.peers.iter().collect::<Vec<_>>();
    //     peers.sort_by_key(|(_, state)| state.failed_times);
    //     peers.into_iter().take(n).map(|(peer, _)| peer)
    // }

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
    RequestTimeout(u64),
    ResponseReceived(GetBroadcastMessagesResult),
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
                debug!(
                    "Sending new GetBroadcastMessages request after timeout: id {}",
                    request_id
                );
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
                                last_message.clone(),
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

                    for message in messages {
                        let _ = state
                            .store
                            .actor
                            .call(
                                |reply| {
                                    ExtendedGossipMessageStoreMessage::SaveMessage(
                                        message, false, reply,
                                    )
                                },
                                None,
                            )
                            .await
                            .expect("store actor alive");
                    }
                    debug!("Sending new GetBroadcastMessages request after receiving response: peer_id {:?}", &state.peer_id);
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
                debug!(
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
                    debug!("Not sending new GetBroadcastMessages request because there are already {} requests inflight (max {})", state.inflight_requests.len(), NUM_SIMULTANEOUS_GET_REQUESTS);
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

    fn get_filter(&self) -> &Cursor {
        &self.filter
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
        _myself: ActorRef<Self::Msg>,
        filter_cursor: Cursor,
    ) -> Result<Self::State, ActorProcessingErr> {
        let subscription = self
            .store
            .subscribe(Some(filter_cursor.clone()), _myself, |m| {
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
                    .update_subscription(subscription, Some(cursor))
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
#[derive(Debug, Clone)]
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
    FinishedSyncing(u64, Cursor),
}

impl PeerSyncStatus {
    fn is_passive_syncing(&self) -> bool {
        match self {
            PeerSyncStatus::PassiveFilter(_) => true,
            _ => false,
        }
    }

    fn is_active_syncing(&self) -> bool {
        match self {
            PeerSyncStatus::ActiveGet(_) => true,
            _ => false,
        }
    }

    fn has_finished_syncing(&self) -> bool {
        match self {
            PeerSyncStatus::FinishedSyncing(_, _) => true,
            _ => false,
        }
    }

    fn can_start_active_syncing(&self) -> bool {
        !self.is_active_syncing() && !self.is_passive_syncing()
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

#[derive(Default, Debug)]
struct PeerState {
    session: SessionId,
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
        match &self.sync_status {
            PeerSyncStatus::ActiveGet(actor) => {
                actor.stop(Some("peer state dropped".to_string()));
            }
            _ => {}
        }
    }
}

impl PeerState {
    fn new(session: SessionId) -> Self {
        Self {
            session,
            filter_processor: Default::default(),
            sync_status: Default::default(),
        }
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
    store: S,
    // The actor that is responsible for book-keep the messages to be saved to the store,
    // and send messages to the subscribers.
    actor: ActorRef<ExtendedGossipMessageStoreMessage>,
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
        store: S,
        chain_actor: ActorRef<CkbChainMessage>,
        supervisor: ActorCell,
    ) -> Self {
        let (actor, _) = Actor::spawn_linked(
            Some(format!(
                "gossip message store actor supervised by {:?}",
                supervisor.get_id()
            )),
            ExtendedGossipMessageStoreActor::new(),
            (maintenance_interval, store.clone(), chain_actor),
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
        cursor: Option<Cursor>,
        receiver: ActorRef<TReceiverMsg>,
        converter: F,
    ) -> Result<Self::Subscription, Self::Error> {
        debug!(
            "Creating a new subscription from cursor {:?} with receiver {:?}",
            &cursor, &receiver
        );
        const DEFAULT_TIMEOUT: u64 = Duration::from_secs(5).as_millis() as u64;
        match call_t!(
            &self.actor,
            ExtendedGossipMessageStoreMessage::NewSubscription,
            DEFAULT_TIMEOUT,
            cursor
        ) {
            Ok((subscription, output_port)) => {
                output_port.subscribe(receiver, converter);
                Ok(subscription)
            }
            Err(e) => Err(Error::InternalError(anyhow::anyhow!(e.to_string()))),
        }
    }

    async fn update_subscription(
        &self,
        subscription: &Self::Subscription,
        cursor: Option<Cursor>,
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
    // The id of the subscriber. Mostly for debugging.
    id: u64,
    // This is the last cursor of the ExtendedGossipMessageStore when the subscriber is created.
    // We have to send all messages up to this cursor to the subscriber immediately after the
    // subscription is created. Other messages (messages after this cursor) are automatically
    // forwarded to the subscriber by the store actor.
    store_last_cursor_while_starting: Cursor,
    // The filter that a subscriber has set. We will only send messages that are newer than this filter.
    // This is normally a cursor that the subscriber is confident that it has received all the messages
    // before this cursor.
    filter: Option<Cursor>,
    // A port that from which the subscriber will receive messages and from which we will send messages to the subscriber.
    output_port: Arc<OutputPort<GossipMessageUpdates>>,
}

impl BroadcastMessageOutput {
    fn new(
        id: u64,
        store_last_cursor_while_starting: Cursor,
        filter: Option<Cursor>,
        output_port: Arc<OutputPort<GossipMessageUpdates>>,
    ) -> Self {
        Self {
            id,
            store_last_cursor_while_starting,
            filter,
            output_port,
        }
    }
}

// This is the error type for processing the gossip messages.
// Sometimes we can't be very sure of if a gossip message is valid on a first sight.
// For example, a ChannelUpdate message may be signed with a different node's key,
// but we won't be able to find this out until we have the corresponding ChannelAnnouncement.
#[derive(Debug, Clone, thiserror::Error)]
pub enum GossipMessageProcessingError {
    #[error("Failed to process the message: {0}")]
    ProcessingError(String),
    #[error("Failed to save the message as a newer message is already saved: {0}")]
    NewerMessageSaved(String),
}

// We use this notifier to notify the caller that the message has been saved to the store.
pub type GossipMessageSavingNotificationSender =
    broadcast::Sender<Result<BroadcastMessageWithTimestamp, GossipMessageProcessingError>>;
pub type GossipMessageSavingNotificationReceiver =
    broadcast::Receiver<Result<BroadcastMessageWithTimestamp, GossipMessageProcessingError>>;

pub struct ExtendedGossipMessageStoreState<S> {
    store: S,
    chain_actor: ActorRef<CkbChainMessage>,
    next_id: u64,
    output_ports: HashMap<u64, BroadcastMessageOutput>,
    last_cursor: Cursor,
    messages_to_be_saved: HashSet<BroadcastMessageWithTimestamp>,
    // TODO: we need to remove the notifiers that are there for a long time to avoid memory leak
    // when the node is running for a long time.
    message_saving_notifier: HashMap<Cursor, GossipMessageSavingNotificationSender>,
}

impl<S: GossipMessageStore> ExtendedGossipMessageStoreState<S> {
    fn new(store: S, chain_actor: ActorRef<CkbChainMessage>) -> Self {
        Self {
            store,
            chain_actor,
            next_id: Default::default(),
            output_ports: Default::default(),
            last_cursor: Default::default(),
            messages_to_be_saved: Default::default(),
            message_saving_notifier: Default::default(),
        }
    }

    fn create_message_saving_notifier(
        &mut self,
        message: &BroadcastMessageWithTimestamp,
    ) -> GossipMessageSavingNotificationReceiver {
        let cursor = message.cursor();
        self.message_saving_notifier
            .entry(cursor)
            .or_insert_with(|| {
                let (sender, _receiver) = broadcast::channel(1);
                sender
            })
            .subscribe()
    }

    fn notify_message_saving_result(
        &mut self,
        cursor: &Cursor,
        result: Result<BroadcastMessageWithTimestamp, GossipMessageProcessingError>,
    ) {
        if let Some(notifier) = self.message_saving_notifier.remove(cursor) {
            debug!(
                "ExtendedGossipMessageActor sending message saving notifier: cursor {:?}, result {:?}",
                &cursor, &result
            );
            let _ = notifier.send(result);
        }
    }

    fn update_last_cursor(&mut self, cursor: Cursor) {
        if cursor > self.last_cursor {
            self.last_cursor = cursor;
        }
    }

    fn save_broadcast_message(&mut self, message: BroadcastMessageWithTimestamp) {
        match message.clone() {
            BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement) => {
                self.store
                    .save_channel_announcement(timestamp, channel_announcement)
            }
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                self.store.save_channel_update(channel_update)
            }
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                self.store.save_node_announcement(node_announcement)
            }
        }
        self.update_last_cursor(message.cursor());
        self.notify_message_saving_result(&message.cursor(), Ok(message));
    }

    // Saving all the messages whose transitive dependencies are already available.
    // We will also change the relevant state (e.g. update the latest cursor).
    // The returned list may be sent to the subscribers.
    fn prune_messages_to_be_saved(&mut self) -> Vec<BroadcastMessageWithTimestamp> {
        let complete_messages = self
            .messages_to_be_saved
            .iter()
            .filter(|m| self.has_transitive_dependencies(m))
            .cloned()
            .collect::<HashSet<_>>();
        self.messages_to_be_saved
            .retain(|v| !complete_messages.contains(v));
        for message in &complete_messages {
            self.save_broadcast_message(message.clone());
        }

        let mut sorted_messages = Vec::with_capacity(complete_messages.len());
        let mut message_latest_cursors = HashMap::new();

        for message in complete_messages {
            let key = (
                message.message_id(),
                match &message {
                    // Message id alone is not enough to differentiate channel updates.
                    // We need a flag to indicate if the message is an update of node 1.
                    BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                        channel_update.is_update_of_node_1()
                    }
                    _ => false,
                },
            );
            match message_latest_cursors.get(&key) {
                // Ignore the message if we already have a newer message in the list.
                Some(latest_cursor) if latest_cursor > &message.cursor() => continue,
                _ => {
                    message_latest_cursors.insert(key, message.cursor());
                    sorted_messages.extend(self.get_dependent_messages_in_memory(&message));
                    sorted_messages.push(message);
                }
            }
        }

        sorted_messages
    }

    fn get_node_announcement(&self, node_id: &Pubkey) -> Option<NodeAnnouncement> {
        self.store
            .get_latest_node_announcement(node_id)
            .or_else(|| self.get_node_announcement_in_memory(node_id))
    }

    fn get_node_announcement_in_memory(&self, node_id: &Pubkey) -> Option<NodeAnnouncement> {
        self.messages_to_be_saved.iter().find_map(|m| match m {
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement)
                if &node_announcement.node_id == node_id =>
            {
                Some(node_announcement.clone())
            }
            _ => None,
        })
    }

    fn get_channel_annnouncement(&self, outpoint: &OutPoint) -> Option<(u64, ChannelAnnouncement)> {
        self.store
            .get_latest_channel_announcement(outpoint)
            .or_else(|| self.get_channel_annnouncement_in_memory(outpoint))
    }

    fn get_channel_annnouncement_in_memory(
        &self,
        outpoint: &OutPoint,
    ) -> Option<(u64, ChannelAnnouncement)> {
        self.messages_to_be_saved.iter().find_map(|m| match m {
            BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement)
                if &channel_announcement.channel_outpoint == outpoint =>
            {
                Some((*timestamp, channel_announcement.clone()))
            }
            _ => None,
        })
    }

    fn get_dependent_messages_in_memory(
        &self,
        message: &BroadcastMessageWithTimestamp,
    ) -> Vec<BroadcastMessageWithTimestamp> {
        debug!("Getting transitive dependencies for message {:?}", message);
        let mut messages = vec![];
        match message {
            BroadcastMessageWithTimestamp::ChannelAnnouncement(_, channel_announcement) => {
                match self.get_node_announcement_in_memory(&channel_announcement.node1_id) {
                    Some(node_announcement) => {
                        messages.push(BroadcastMessageWithTimestamp::NodeAnnouncement(
                            node_announcement,
                        ));
                    }
                    _ => {}
                };
                match self.get_node_announcement_in_memory(&channel_announcement.node2_id) {
                    None => return vec![],
                    Some(node_announcement) => {
                        messages.push(BroadcastMessageWithTimestamp::NodeAnnouncement(
                            node_announcement,
                        ));
                    }
                }
            }
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                match self.get_channel_annnouncement(&channel_update.channel_outpoint) {
                    Some((timestamp, channel_announcement)) => {
                        let channel_announcement =
                            BroadcastMessageWithTimestamp::ChannelAnnouncement(
                                timestamp,
                                channel_announcement,
                            );
                        messages
                            .extend(self.get_dependent_messages_in_memory(&channel_announcement));
                        messages.push(channel_announcement);
                    }
                    _ => {
                        panic!(
                            "Channel announcement not found for channel update: {:?}",
                            channel_update
                        );
                    }
                }
            }
            BroadcastMessageWithTimestamp::NodeAnnouncement(_) => {}
        }
        messages
    }

    fn has_transitive_dependencies(&self, message: &BroadcastMessageWithTimestamp) -> bool {
        debug!(
            "Verifying transitive dependencies for message {:?}",
            message
        );
        match message {
            BroadcastMessageWithTimestamp::ChannelAnnouncement(_, channel_announcement) => {
                self.get_node_announcement(&channel_announcement.node1_id)
                    .is_some()
                    && self
                        .get_node_announcement(&channel_announcement.node2_id)
                        .is_some()
            }
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                match self.get_channel_annnouncement(&channel_update.channel_outpoint) {
                    Some((timestamp, channel_announcement)) => self.has_transitive_dependencies(
                        &BroadcastMessageWithTimestamp::ChannelAnnouncement(
                            timestamp,
                            channel_announcement,
                        ),
                    ),
                    _ => false,
                }
            }
            BroadcastMessageWithTimestamp::NodeAnnouncement(_) => true,
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
    type Arguments = (Duration, S, ActorRef<CkbChainMessage>);

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (gossip_store_maintenance_interval, store, chain_actor): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_interval(gossip_store_maintenance_interval, || {
            ExtendedGossipMessageStoreMessage::Tick
        });
        Ok(ExtendedGossipMessageStoreState::new(store, chain_actor))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            ExtendedGossipMessageStoreMessage::NewSubscription(cursor, reply) => {
                let id = state.next_id;
                state.next_id += 1;
                let output_port = Arc::new(OutputPort::default());
                let _ = reply.send((id, Arc::clone(&output_port)));
                let store_last_cursor_while_starting = state.last_cursor.clone();
                match &cursor {
                    Some(cursor) if cursor < &store_last_cursor_while_starting => {
                        debug!(
                            "Loading messages from store for subscriber {}: subscription cursor {:?}, store cursor {:?}",
                            id, cursor, store_last_cursor_while_starting
                        );
                        // Since the handling of LoadMessagesFromStore interleaves with the handling of Tick,
                        // we may send the messages in an order that is different from both the dependency order
                        // and the timestamp order. This means that we may send a ChannelUpdate while handling
                        // Tick and later we will send the corresponding ChannelAnnouncement.
                        // So the downstream consumer need to either cache some of the messages and wait for the
                        // dependent messages to arrive or read the messages from the store directly.
                        myself.send_message(
                            ExtendedGossipMessageStoreMessage::LoadMessagesFromStore(
                                id,
                                cursor.clone(),
                            ),
                        )?;
                    }
                    _ => {}
                }
                state.output_ports.insert(
                    id,
                    BroadcastMessageOutput::new(
                        id,
                        store_last_cursor_while_starting,
                        cursor,
                        Arc::clone(&output_port),
                    ),
                );
            }

            ExtendedGossipMessageStoreMessage::UpdateSubscription(id, cursor, reply) => {
                match cursor {
                    Some(cursor) => {
                        state
                            .output_ports
                            .get_mut(&id)
                            .map(|output| output.filter = cursor);
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
                    .filter(|m| m.cursor() <= subscription.store_last_cursor_while_starting)
                    .collect::<Vec<_>>();
                match messages.last() {
                    Some(m) => {
                        myself.send_message(
                            ExtendedGossipMessageStoreMessage::LoadMessagesFromStore(
                                id,
                                m.cursor(),
                            ),
                        )?;
                        subscription
                            .output_port
                            .send(GossipMessageUpdates::new(messages));
                    }
                    None => {
                        // All the messages that are newer than store_last_cursor_while_starting
                        // This means that we have finished initial loading.
                    }
                }
            }

            ExtendedGossipMessageStoreMessage::SaveMessage(message, wait_for_saving, reply) => {
                if let Some(existing_message) =
                    get_existing_newer_broadcast_message(&message, &state.store)
                {
                    let _ = reply.send(Err(Error::InvalidParameter(format!(
                            "An existing broadcast message already saved to store: existing {:?}, new message to store {:?}",
                            existing_message,
                            message
                        ))));
                    return Ok(());
                }

                let (message, _) = match partially_verify_broadcast_message(
                    message.clone(),
                    &state.store,
                    &state.chain_actor,
                )
                .await
                {
                    Err(error) => {
                        error!(
                            "Failed to verify broadcast message {:?}: {:?}",
                            &message, &error
                        );
                        let _ = reply.send(Err(error));
                        return Ok(());
                    }
                    Ok((verified_message, is_complete)) => (verified_message, is_complete),
                };

                if message.timestamp()
                    > now_timestamp_as_millis_u64() + MAX_BROADCAST_MESSAGE_TIMESTAMP_DRIFT_MILLIS
                {
                    error!(
                        "Broadcast message timestamp is too far in the future: {:?}",
                        message
                    );
                    let _ = reply.send(Err(Error::InvalidParameter(format!(
                        "Broadcast message timestamp is too far in the future {:?}",
                        message
                    ))));
                    return Ok(());
                }

                if wait_for_saving {
                    let notifier = state.create_message_saving_notifier(&message);
                    let _ = reply.send(Ok(Some(notifier)));
                } else {
                    let _ = reply.send(Ok(None));
                }

                trace!("ExtendedGossipMessageActor saving message: {:?}", message);
                state.messages_to_be_saved.insert(message);
            }

            ExtendedGossipMessageStoreMessage::Tick => {
                trace!(
                    "Gossip store maintenance ticked: last_cursor = {:?} #subscriptions = {},  #messages_to_be_saved = {}",
                    state.last_cursor,
                    state.output_ports.len(),
                    state.messages_to_be_saved.len(),
                );
                trace!(
                    "Gossip store maintenance ticked: state.messages_to_be_saved {:?}",
                    state.messages_to_be_saved
                );

                let complete_messages = state.prune_messages_to_be_saved();

                // We need to send the lagged complete messages to the subscribers. After doing this,
                // we may remove the messages from the lagged_messages.
                for subscription in state.output_ports.values() {
                    let messages_to_send = match subscription.filter {
                        Some(ref filter) => complete_messages
                            .iter()
                            .filter(|m| &m.cursor() > filter)
                            .cloned()
                            .collect::<Vec<_>>(),
                        None => complete_messages.clone(),
                    };
                    debug!(
                        "ExtendedGossipMessageActor sending lagged complete messages to subscriber: number of messages = {}",
                        messages_to_send.len()
                    );
                    for chunk in messages_to_send.chunks(MAX_NUM_OF_BROADCAST_MESSAGES as usize) {
                        if chunk.is_empty() {
                            break;
                        }
                        subscription
                            .output_port
                            .send(GossipMessageUpdates::new(chunk.to_vec()));
                    }
                }
            }
        }
        Ok(())
    }
}

pub enum ExtendedGossipMessageStoreMessage {
    // A new subscription for gossip message updates. We will send a batch of messages to the subscriber
    // via the returned output port.
    NewSubscription(
        Option<Cursor>,
        RpcReplyPort<(u64, Arc<OutputPort<GossipMessageUpdates>>)>,
    ),
    // Update the subscription with a new cursor. If the outer Option is None, the subscription will be cancelled.
    // If the inner Option is None, the subscription will start from the very latest message in the store.
    UpdateSubscription(u64, Option<Option<Cursor>>, RpcReplyPort<()>),
    // Save a new broadcast message to the store. We will check if the message has any dependencies that are not
    // saved yet. If it has, we will save it to messages_to_be_saved, otherwise we will save it to the store.
    // We may also save the message to lagged_messages if the message is lagged.
    // We may pass a bool parameter to indicate if a output port to wait for the message to be saved should be
    // returned. If there is any error while saving the message, we will send an error message to the output port.
    SaveMessage(
        BroadcastMessage,
        bool,
        RpcReplyPort<Result<Option<GossipMessageSavingNotificationReceiver>, Error>>,
    ),
    // Send broadcast messages after the cursor to the subscriber specified in the u64 id.
    // This is normally called immediately after a new subscription is created. This is the time when
    // we need to send existing messages to the subscriber.
    LoadMessagesFromStore(u64, Cursor),
    // A tick message that is sent periodically to check if there are any messages that are saved out of order.
    // If there are, we will send them to the subscribers.
    // This tick will also advance the last_cursor upon finishing.
    Tick,
}

pub(crate) struct GossipActorState<S> {
    store: ExtendedGossipMessageStore<S>,
    control: ServiceAsyncControl,
    next_request_id: u64,
    myself: ActorRef<GossipActorMessage>,
    chain_actor: ActorRef<CkbChainMessage>,
    // There are some messages missing from our store, and we need to query them from peers.
    // These messages include channel updates and node announcements related to channel announcements,
    // and channel announcements related to channel updates.
    pending_queries: Vec<BroadcastMessageQuery>,
    peer_states: HashMap<PeerId, PeerState>,
}

impl<S> GossipActorState<S>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
{
    fn num_of_finished_syncing_peers(&self) -> usize {
        self.peer_states
            .values()
            .filter(|state| state.sync_status.has_finished_syncing())
            .count()
    }

    fn num_of_active_syncing_peers(&self) -> usize {
        self.peer_states
            .values()
            .filter(|state| state.sync_status.is_active_syncing())
            .count()
    }

    fn num_of_passive_syncing_peers(&self) -> usize {
        self.peer_states
            .values()
            .filter(|state| state.sync_status.is_passive_syncing())
            .count()
    }

    // Passive syncer should be started when there is at least one peer is in the state of passive syncing.
    // Or there is at least one peer has finished syncing.
    fn is_ready_for_passive_syncing(&self) -> bool {
        self.num_of_passive_syncing_peers() > 0 || self.num_of_finished_syncing_peers() > 0
    }

    // Currently we only start new active syncer when there is no successful active syncer finished their job.
    // It is actually sensible to start a new active syncer once in a while.
    fn is_ready_for_active_syncing(&self) -> bool {
        self.num_of_finished_syncing_peers() == 0
    }

    fn new_peers_to_start_active_syncing(&self) -> Vec<PeerId> {
        if !self.is_ready_for_active_syncing() {
            return vec![];
        }

        let num_of_active_syncing_peers = self.num_of_active_syncing_peers();
        if num_of_active_syncing_peers >= MAX_NUM_OF_ACTIVE_SYNCING_PEERS {
            return vec![];
        }

        self.peer_states
            .iter()
            .filter(|(_, state)| state.sync_status.can_start_active_syncing())
            .take(MAX_NUM_OF_ACTIVE_SYNCING_PEERS - num_of_active_syncing_peers)
            .map(|(peer_id, _)| peer_id)
            .cloned()
            .collect()
    }

    fn peers_to_start_passive_syncing(&self) -> Vec<PeerId> {
        if !self.is_ready_for_passive_syncing() {
            return vec![];
        }

        let num_of_passive_syncing_peers = self.num_of_passive_syncing_peers();
        if num_of_passive_syncing_peers >= MIN_NUM_OF_PASSIVE_SYNCING_PEERS {
            return vec![];
        }

        self.peer_states
            .iter()
            .filter(|(_, state)| state.sync_status.can_start_passive_syncing())
            .take(MIN_NUM_OF_PASSIVE_SYNCING_PEERS - num_of_passive_syncing_peers)
            .map(|(peer_id, _)| peer_id)
            .cloned()
            .collect()
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
            .sync_status = PeerSyncStatus::ActiveGet(sync_actor.0);
    }

    async fn start_new_passive_syncer(&mut self, peer_id: &PeerId) {
        match self.send_broadcast_message_filter(peer_id).await {
            Ok(cursor) => {
                self.peer_states
                    .get_mut(peer_id)
                    .expect("get peer state")
                    .sync_status = PeerSyncStatus::PassiveFilter(cursor);
            }
            Err(e) => {
                error!(
                    "Failed to send BroadcastMessagesFilter to peer {:?}: {:?}",
                    peer_id, e
                );
            }
        }
    }

    async fn send_broadcast_message_filter(&mut self, peer_id: &PeerId) -> crate::Result<Cursor> {
        let cursor = self.get_safe_cursor_to_start_syncing();
        let message = GossipMessage::BroadcastMessagesFilter(BroadcastMessagesFilter {
            chain_hash: get_chain_hash(),
            after_cursor: cursor.clone(),
        });
        debug!(
            "Sending BroadcastMessagesFilter to peer {:?}: {:?}",
            &peer_id, &message
        );
        self.send_message_to_peer(peer_id, message).await?;
        Ok(cursor)
    }

    fn is_peer_connected(&self, peer_id: &PeerId) -> bool {
        self.peer_states.contains_key(peer_id)
    }

    fn get_store(&self) -> &S {
        self.store.get_store()
    }

    fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        self.peer_states.get(peer_id).map(|s| s.session)
    }

    fn get_latest_cursor(&self) -> Cursor {
        self.get_store()
            .get_latest_broadcast_message_cursor()
            .unwrap_or_default()
    }

    fn get_safe_cursor_to_start_syncing(&self) -> Cursor {
        self.get_latest_cursor()
            .go_back_for_some_time(MAX_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT)
    }

    async fn try_to_verify_and_save_broadcast_message(&mut self, message: BroadcastMessage) {
        // If there is any messages related to this message that we haven't obtained yet, we will
        // add them to pending_queries, which would be processed later.
        // TODO: It is possible the message here comes from a malicious peer. We should check bookkeep
        // the origin of the message and check if queries constructed here go nowhere.
        let queries = get_dependent_message_queries(&message, self.get_store());
        self.pending_queries.extend(queries);

        let _ = self
            .store
            .actor
            .call(
                |reply| ExtendedGossipMessageStoreMessage::SaveMessage(message, false, reply),
                None,
            )
            .await
            .expect("store actor alive");
    }

    async fn send_message_to_session(
        &self,
        session_id: SessionId,
        message: GossipMessage,
    ) -> crate::Result<()> {
        send_message_to_session(&self.control, session_id, message).await?;
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

fn get_dependent_message_queries<S: GossipMessageStore>(
    message: &BroadcastMessage,
    store: &S,
) -> Vec<BroadcastMessageQuery> {
    let mut queries = Vec::new();
    match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
            let outpoint = &channel_announcement.channel_outpoint;
            if store
                .get_latest_node_announcement_timestamp(&channel_announcement.node1_id)
                .is_none()
            {
                queries.push(BroadcastMessageQuery {
                    flags: BroadcastMessageQueryFlags::NodeAnnouncementNode1,
                    channel_outpoint: outpoint.clone(),
                });
            }
            if store
                .get_latest_node_announcement_timestamp(&channel_announcement.node2_id)
                .is_none()
            {
                queries.push(BroadcastMessageQuery {
                    flags: BroadcastMessageQueryFlags::NodeAnnouncementNode2,
                    channel_outpoint: outpoint.clone(),
                });
            }
        }
        BroadcastMessage::ChannelUpdate(channel_update) => {
            // Check if we need to obtain related channel announcement message.
            let outpoint = &channel_update.channel_outpoint;
            if store
                .get_latest_channel_announcement_timestamp(outpoint)
                .is_none()
            {
                queries.push(BroadcastMessageQuery {
                    flags: BroadcastMessageQueryFlags::ChannelAnnouncement,
                    channel_outpoint: outpoint.clone(),
                });
            }
        }
        BroadcastMessage::NodeAnnouncement(_node_announcement) => {}
    }
    queries
}

async fn get_message_cursor<S: GossipMessageStore>(
    message: BroadcastMessage,
    store: &S,
    chain: &ActorRef<CkbChainMessage>,
) -> Result<Cursor, Error> {
    let m = match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
            let timestamp =
                verify_channel_announcement(&channel_announcement, store, chain).await?;
            BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement)
        }
        BroadcastMessage::ChannelUpdate(channel_update) => {
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update)
        }
        BroadcastMessage::NodeAnnouncement(node_announcement) => {
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement)
        }
    };
    Ok(m.cursor())
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

// Channel updates depends on channel announcements to obtain the node public keys.
// If a channel update is saved before the channel announcement, we can't reliably determine if
// this channel update is valid. So we need to save the channel update to lagged_messages and
// wait for the channel announcement to be saved. The bool value returned indicates if the
// message is fully verified and can be saved to the store.
// In the same vein, channel announcement contains references to node announcements. If a node
// announcement is saved before the channel announcement, we need to temporarily save the channel
// announcement to lagged_messages and wait for the node announcement to be saved.
async fn partially_verify_broadcast_message<S: GossipMessageStore>(
    message: BroadcastMessage,
    store: &S,
    chain: &ActorRef<CkbChainMessage>,
) -> Result<(BroadcastMessageWithTimestamp, bool), Error> {
    match message {
        BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
            let timestamp =
                verify_channel_announcement(&channel_announcement, store, chain).await?;
            let has_node_announcements = store
                .get_latest_node_announcement(&channel_announcement.node1_id)
                .is_some()
                && store
                    .get_latest_node_announcement(&channel_announcement.node2_id)
                    .is_some();
            Ok((
                BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement),
                has_node_announcements,
            ))
        }
        BroadcastMessage::ChannelUpdate(channel_update) => {
            let fully_validated = partially_verify_channel_update(&channel_update, store)?;
            Ok((
                BroadcastMessageWithTimestamp::ChannelUpdate(channel_update),
                fully_validated,
            ))
        }
        BroadcastMessage::NodeAnnouncement(node_announcement) => {
            verify_node_announcement(&node_announcement, store)?;
            Ok((
                BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement),
                true,
            ))
        }
    }
}

async fn verify_channel_announcement<S: GossipMessageStore>(
    channel_announcement: &ChannelAnnouncement,
    store: &S,
    chain: &ActorRef<CkbChainMessage>,
) -> Result<u64, Error> {
    debug!(
        "Verifying channel announcement message: {:?}",
        &channel_announcement
    );
    if let Some((timestamp, announcement)) =
        store.get_latest_channel_announcement(&channel_announcement.channel_outpoint)
    {
        if announcement == *channel_announcement {
            return Ok(timestamp);
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

    debug!(
        "Node signatures in channel announcement message verified: {:?}",
        &channel_announcement
    );

    let (tx, block_hash) = match call_t!(
        chain,
        CkbChainMessage::TraceTx,
        DEFAULT_CHAIN_ACTOR_TIMEOUT,
        TraceTxRequest {
            tx_hash: channel_announcement.channel_outpoint.tx_hash(),
            confirmations: 1,
        }
    ) {
        Ok(TraceTxResponse {
            tx: Some(tx),
            status:
                TxStatus {
                    status: Status::Committed,
                    block_hash: Some(block_hash),
                    ..
                },
        }) => (tx, block_hash),
        err => {
            return Err(Error::InvalidParameter(format!(
                "Channel announcement transaction {:?} not found or not confirmed, result is: {:?}",
                &channel_announcement.channel_outpoint.tx_hash(),
                err
            )));
        }
    };

    debug!("Channel announcement transaction found: {:?}", &tx);

    let pubkey = channel_announcement.ckb_key.serialize();
    let pubkey_hash = &blake2b_256(pubkey.as_slice())[0..20];
    match tx.inner.outputs.first() {
        None => {
            return Err(Error::InvalidParameter(format!(
                "On-chain transaction found but no output: {:?}",
                &channel_announcement
            )));
        }
        Some(output) => {
            if output.lock.args.as_bytes() != pubkey_hash {
                return Err(Error::InvalidParameter(format!(
                    "On-chain transaction found but pubkey hash mismatched: on chain hash {:?}, pub key ({:?}) hash {:?}",
                    &output.lock.args.as_bytes(),
                    hex::encode(pubkey),
                    &pubkey_hash
                )));
            }
            let capacity: u128 = u64::from(output.capacity).into();
            if channel_announcement.udt_type_script.is_none()
                && capacity != channel_announcement.capacity
            {
                return Err(Error::InvalidParameter(format!(
                    "On-chain transaction found but capacity mismatched: on chain capacity {:?}, channel capacity {:?}",
                    &output.capacity, &channel_announcement.capacity
                )));
            }
            capacity
        }
    };

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

    debug!(
        "All signatures in channel announcement message verified: {:?}",
        &channel_announcement
    );

    let timestamp: u64 = match call_t!(
        chain,
        CkbChainMessage::GetBlockTimestamp,
        DEFAULT_CHAIN_ACTOR_TIMEOUT,
        GetBlockTimestampRequest::from_block_hash(block_hash.clone())
    ) {
        Ok(Ok(Some(timestamp))) => timestamp,
        Ok(Ok(None)) => {
            return Err(Error::InternalError(anyhow::anyhow!(
                "Unable to find block {:?} for channel outpoint {:?}",
                &block_hash,
                &channel_announcement.channel_outpoint
            )));
        }
        Ok(Err(err)) => {
            return Err(Error::CkbRpcError(err));
        }
        Err(err) => {
            return Err(Error::InternalError(anyhow::Error::new(err).context(
                format!(
                    "Error while trying to obtain block {:?} for channel outpoint {:?}",
                    block_hash, channel_announcement.channel_outpoint
                ),
            )));
        }
    };

    debug!(
        "Obtained block timestamp for channel: outpoint {:?}, timestamp {}",
        &channel_announcement.channel_outpoint, timestamp
    );

    Ok(timestamp)
}

fn partially_verify_channel_update<S: GossipMessageStore>(
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
            debug!(
                "Verifying channel update message signature: {:?}, pubkey: {:?}, message: {:?}",
                &channel_update, &pubkey, &message
            );
            if !signature.verify(&pubkey, &message) {
                return Err(Error::InvalidParameter(format!(
                    "Channel update message signature verification failed (invalid signature): {:?}",
                    &channel_update
                )));
            }
            Ok(true)
        }
        None => {
            // It is possible that the channel update message is received before the channel announcement message.
            // In this case, we should temporarily store the channel update message and verify it later
            // when the channel announcement message is received.
            return Ok(false);
        }
    }
}

fn verify_node_announcement<S: GossipMessageStore>(
    node_announcement: &NodeAnnouncement,
    store: &S,
) -> Result<(), Error> {
    if let Some(BroadcastMessageWithTimestamp::NodeAnnouncement(announcement)) =
        store.get_broadcast_message_with_cursor(&node_announcement.cursor())
    {
        if announcement == *node_announcement {
            return Ok(());
        } else {
            return Err(Error::InvalidParameter(format!(
                "Node announcement message already exists but mismatched: {:?}, existing: {:?}",
                &node_announcement, &announcement
            )));
        }
    }
    let message = node_announcement.message_to_sign();
    match node_announcement.signature {
        Some(ref signature) if signature.verify(&node_announcement.node_id, &message) => {
            debug!(
                "Node announcement message verified: {:?}",
                &node_announcement
            );
        }
        _ => {
            return Err(Error::InvalidParameter(format!(
                "Node announcement message signature verification failed: {:?}",
                &node_announcement
            )));
        }
    }
    Ok(())
}

impl GossipProtocolHandle {
    pub(crate) async fn new<S>(
        name: Option<String>,
        gossip_network_maintenance_interval: Duration,
        gossip_store_maintenance_interval: Duration,
        store: S,
        chain_actor: ActorRef<CkbChainMessage>,
        supervisor: ActorCell,
    ) -> (Self, ExtendedGossipMessageStore<S>)
    where
        S: GossipMessageStore + Clone + Send + Sync + 'static,
    {
        let (network_control_sender, network_control_receiver) = oneshot::channel();
        let (store_sender, store_receiver) = oneshot::channel();

        let (actor, _handle) = ActorRuntime::spawn_linked_instant(
            name,
            GossipActor::new(),
            (
                network_control_receiver,
                store_sender,
                gossip_network_maintenance_interval,
                gossip_store_maintenance_interval,
                store,
                chain_actor,
            ),
            supervisor,
        )
        .expect("start gossip actor");
        let store = store_receiver.await.expect("receive store");
        (
            Self {
                actor,
                sender: Some(network_control_sender),
            },
            store,
        )
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
        S,
        ActorRef<CkbChainMessage>,
    );

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (rx, tx, network_maintenance_interval, store_maintenance_interval, store, chain_actor): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let store = ExtendedGossipMessageStore::new(
            store_maintenance_interval,
            store,
            chain_actor.clone(),
            myself.get_cell(),
        )
        .await;
        if let Err(_) = tx.send(store.clone()) {
            panic!("failed to send store to the caller");
        }
        let control = timeout(Duration::from_secs(1), rx)
            .await
            .expect("received control timely")
            .expect("receive control");
        debug!("Gossip actor received service control");

        let _ = myself.send_after(Duration::from_millis(500), || {
            GossipActorMessage::TickNetworkMaintenance
        });
        let _ = myself.send_interval(network_maintenance_interval, || {
            GossipActorMessage::TickNetworkMaintenance
        });
        let state = Self::State {
            store,
            control,
            myself,
            chain_actor,
            next_request_id: Default::default(),
            pending_queries: Default::default(),
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
            SupervisionEvent::ActorPanicked(who, _) => {
                error!("{:?} panicked", who);
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
        trace!("Gossip actor received message: {:?}", &message);

        match message {
            GossipActorMessage::PeerConnected(peer_id, pubkey, session) => {
                if state.is_peer_connected(&peer_id) {
                    warn!(
                        "Repeated connection from {:?} for gossip protocol",
                        &peer_id
                    );
                    return Ok(());
                }
                debug!(
                    "Saving gossip peer pubkey and session: peer {:?}, pubkey {:?}, session {:?}",
                    &peer_id, &pubkey, &session.id
                );
                state
                    .peer_states
                    .insert(peer_id.clone(), PeerState::new(session.id));
                state.start_new_active_syncer(&peer_id).await;
            }
            GossipActorMessage::PeerDisconnected(peer_id, session) => {
                debug!(
                    "Peer disconnected: peer {:?}, session {:?}",
                    &peer_id, &session.id
                );
                drop(state.peer_states.remove(&peer_id));
            }
            GossipActorMessage::ProcessBroadcastMessage(message) => {
                state
                    .try_to_verify_and_save_broadcast_message(message.clone())
                    .await;
            }
            GossipActorMessage::TryBroadcastMessages(messages) => {
                debug!("Trying to broadcast message: {:?}", &messages);
                for message in messages {
                    match call!(
                        state.store.actor,
                        ExtendedGossipMessageStoreMessage::SaveMessage,
                        message.clone(),
                        true
                    )
                    .expect("store actor alive")
                    {
                        Ok(Some(mut receiver)) => {
                            debug!(
                                "Broadcast message saving subscribed, waiting for it to be saved: {:?}",
                                &message
                            );
                            let myself = myself.clone();
                            ractor::concurrency::tokio_primatives::spawn(async move {
                                // TODO: We should set a timeout here.
                                match receiver.recv().await {
                                    Ok(Ok(message)) => {
                                        debug!("Broadcast message saved: {:?}", &message);
                                        let _ = myself.send_message(
                                            GossipActorMessage::BroadcastMessageImmediately(
                                                message,
                                            ),
                                        );
                                    }
                                    Ok(Err(error)) => {
                                        error!(
                                            "Failed to save broadcast message (maybe message is invalid) {:?}: {:?}",
                                            &message, &error
                                        );
                                    }
                                    Err(error) => {
                                        error!(
                                            "Failed to save broadcast message {:?}: {:?}",
                                            &message, &error
                                        );
                                    }
                                }
                            });
                        }
                        Ok(None) => {
                            panic!("output port not returned while saving broadcast message");
                        }
                        Err(error) => {
                            error!(
                                "Failed to save broadcast message (calling actor error) {:?}: {:?}",
                                &message, &error
                            );
                        }
                    }
                }
            }
            GossipActorMessage::BroadcastMessageImmediately(message) => {
                for (peer, peer_state) in &state.peer_states {
                    let session = peer_state.session;
                    match &peer_state.filter_processor {
                        Some(filter_processor)
                            if filter_processor.get_filter() < &message.cursor() =>
                        {
                            state
                                .send_message_to_session(
                                    session,
                                    GossipMessage::BroadcastMessagesFilterResult(
                                        message.create_broadcast_messages_filter_result(),
                                    ),
                                )
                                .await?;
                        }
                        _ => {
                            debug!(
                                "Ignoring broadcast message for peer {:?}: {:?} as its filter processor is {:?}",
                                peer, &message, &peer_state.filter_processor
                            );
                        }
                    }
                }
            }

            GossipActorMessage::TickNetworkMaintenance => {
                trace!(
                    "Gossip network maintenance ticked, current state: num of peers: {}, num of finished syncing peers: {}, num of active syncing peers: {}, num of passive syncing peers: {}, num of pending queries: {}, peer states: {:?}",
                    state.peer_states.len(),
                    state.num_of_finished_syncing_peers(),
                    state.num_of_active_syncing_peers(),
                    state.num_of_passive_syncing_peers(),
                    state.pending_queries.len(),
                    &state.peer_states
                );
                for peer in state.new_peers_to_start_active_syncing() {
                    debug!("Starting new active syncer for peer {:?}", &peer);
                    state.start_new_active_syncer(&peer).await;
                }

                for peer in state.peers_to_start_passive_syncing() {
                    debug!("Starting new passive syncer for peer {:?}", &peer);
                    state.start_new_passive_syncer(&peer).await;
                }

                // Query missing messages from peers.
                let pending_queries = std::mem::take(&mut state.pending_queries);
                for chunk in pending_queries.chunks(MAX_NUM_OF_BROADCAST_MESSAGES as usize) {
                    let queries = chunk.to_vec();
                    let id = state.get_and_increment_request_id();
                    for peer_state in state
                        .peer_states
                        .values()
                        .take(NUM_SIMULTANEOUS_GET_REQUESTS)
                    {
                        let message =
                            GossipMessage::QueryBroadcastMessages(QueryBroadcastMessages {
                                id,
                                chain_hash: get_chain_hash(),
                                queries: queries.clone(),
                            });
                        send_message_to_session(&state.control, peer_state.session, message)
                            .await?;
                    }
                }
            }

            GossipActorMessage::ActiveSyncingFinished(peer_id, cursor) => {
                debug!(
                    "Active syncing finished for peer {:?}: {:?}",
                    &peer_id, &cursor
                );
                if let Some(peer_state) = state.peer_states.get_mut(&peer_id) {
                    peer_state.sync_status =
                        PeerSyncStatus::FinishedSyncing(now_timestamp_as_millis_u64(), cursor);
                }
            }

            GossipActorMessage::MaliciousPeerFound(peer_id) => {
                debug!("Malicious peer found: {:?}", &peer_id);
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
                        check_chain_hash(&chain_hash)?;
                        match state.peer_states.get_mut(&peer_id) {
                            Some(peer_state) => match peer_state.filter_processor.as_mut() {
                                Some(filter_processor) => {
                                    debug!(
                                        "Updating filter processor for peer {:?}: from {:?} {:?}",
                                        &peer_id,
                                        filter_processor.get_filter(),
                                        &after_cursor
                                    );
                                    filter_processor.update_filter(&after_cursor);
                                    return Ok(());
                                }
                                _ => {
                                    debug!(
                                        "Creating filter processor for peer {:?}: {:?}",
                                        &peer_id, &after_cursor
                                    );
                                    peer_state.filter_processor = Some(
                                        PeerFilterProcessor::new(
                                            state.store.clone(),
                                            peer_id,
                                            after_cursor.clone(),
                                            myself,
                                        )
                                        .await,
                                    );
                                    peer_state
                                }
                            },
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
                        for message in messages {
                            state
                                .try_to_verify_and_save_broadcast_message(message)
                                .await;
                        }
                    }
                    GossipMessage::GetBroadcastMessages(get_broadcast_messages) => {
                        check_chain_hash(&get_broadcast_messages.chain_hash)?;
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
                            Some(get_broadcast_messages.count as u16),
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
                        check_chain_hash(&chain_hash)?;
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
                                missing_queries: missing_queries,
                            },
                        );
                        if let Err(error) = state.send_message_to_peer(&peer_id, result).await {
                            error!(
                                "Failed to send QueryBroadcastMessagesResult to peer {:?}: {:?}",
                                &peer_id, error
                            );
                        }
                    }
                    GossipMessage::QueryBroadcastMessagesResult(QueryBroadcastMessagesResult {
                        id,
                        messages,
                        missing_queries,
                    }) => {
                        let is_finished = missing_queries.is_empty();
                        for message in messages {
                            state
                                .try_to_verify_and_save_broadcast_message(message)
                                .await;
                        }
                        // TODO: mark requests corresponding to id as finished
                        // TODO: if not finished, send another QueryBroadcastMessages.
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
        if let Err(_) = sender.send(context.control().clone()) {
            panic!("Failed to send service control");
        }
    }

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        info!(
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
        info!(
            "proto id [{}] close on session [{}], address: [{}], type: [{:?}]",
            context.proto_id, context.session.id, &context.session.address, &context.session.ty
        );

        match context.session.remote_pubkey.as_ref() {
            Some(remote_pubkey) => {
                let remote_peer_id = PeerId::from_public_key(&remote_pubkey);
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
