use ckb_hash::blake2b_256;
use ckb_jsonrpc_types::{Status, TxStatus};
use ckb_types::core::TransactionView;
use ckb_types::packed::{Byte32, OutPoint, Script, Transaction};
use ckb_types::prelude::{IntoTransactionView, Pack, Unpack};
use ractor::concurrency::Duration;
use ractor::{
    async_trait as rasync_trait, call, call_t, Actor, ActorCell, ActorProcessingErr, ActorRef,
    RactorErr, RpcReplyPort, SupervisionEvent,
};
use secp256k1::Message;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use std::u64;
use tentacle::multiaddr::{MultiAddr, Protocol};
use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::SessionContext,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    multiaddr::Multiaddr,
    secio::PeerId,
    secio::SecioKeyPair,
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, trace, warn};

use super::channel::{
    AcceptChannelParameter, ChannelActor, ChannelActorMessage, ChannelActorStateStore,
    ChannelCommand, ChannelCommandWithId, ChannelEvent, ChannelInitializationParameter,
    ChannelSubscribers, OpenChannelParameter, ProcessingChannelError, ProcessingChannelResult,
    PublicChannelInfo, DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE,
};
use super::config::AnnouncedNodeName;
use super::fee::{calculate_commitment_tx_fee, default_minimal_ckb_amount};
use super::graph::{NetworkGraph, NetworkGraphStateStore};
use super::graph_syncer::{GraphSyncer, GraphSyncerMessage};
use super::key::blake2b_hash_with_salt;
use super::types::{
    ChannelAnnouncementQuery, ChannelUpdateQuery, EcdsaSignature, FiberBroadcastMessage,
    FiberBroadcastMessageQuery, FiberMessage, FiberQueryInformation, GetBroadcastMessages,
    GetBroadcastMessagesResult, Hash256, NodeAnnouncement, NodeAnnouncementQuery, OpenChannel,
    Privkey, Pubkey, QueryBroadcastMessagesWithinTimeRange,
    QueryBroadcastMessagesWithinTimeRangeResult, QueryChannelsWithinBlockRange,
    QueryChannelsWithinBlockRangeResult,
};
use super::FiberConfig;

use crate::ckb::contracts::{check_udt_script, is_udt_type_auto_accept};
use crate::ckb::{CkbChainMessage, FundingRequest, FundingTx, TraceTxRequest, TraceTxResponse};
use crate::fiber::channel::{
    AddTlcCommand, AddTlcResponse, TxCollaborationCommand, TxUpdateCommand,
};
use crate::fiber::graph::{ChannelInfo, NodeInfo, PaymentSession};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::types::{secp256k1_instance, FiberChannelMessage, OnionPacket, TxSignatures};
use crate::{unwrap_or_return, Error};

pub const FIBER_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

pub const DEFAULT_CHAIN_ACTOR_TIMEOUT: u64 = 60000;

// This is a temporary way to document that we assume the chain actor is always alive.
// We may later relax this assumption. At the moment, if the chain actor fails, we
// should panic with this message, and later we may find all references to this message
// to make sure that we handle the case where the chain actor is not alive.
const ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW: &str =
    "We currently assume that chain actor is always alive, but it failed. This is a known issue.";

const ASSUME_NETWORK_MYSELF_ALIVE: &str = "network actor myself alive";

pub(crate) fn get_chain_hash() -> Hash256 {
    Default::default()
}

#[derive(Debug)]
pub struct OpenChannelResponse {
    pub channel_id: Hash256,
}

#[derive(Debug)]
pub struct AcceptChannelResponse {
    pub old_channel_id: Hash256,
    pub new_channel_id: Hash256,
}

#[derive(Debug)]
pub struct SendPaymentResponse {
    pub payment_hash: Hash256,
}

/// What kind of local information should be broadcasted to the network.
#[derive(Debug)]
pub enum LocalInfoKind {
    NodeAnnouncement,
}

/// The struct here is used both internally and as an API to the outside world.
/// If we want to send a reply to the caller, we need to wrap the message with
/// a RpcReplyPort. Since outsider users have no knowledge of RpcReplyPort, we
/// need to hide it from the API. So in case a reply is needed, we need to put
/// an optional RpcReplyPort in the of the definition of this message.
#[derive(Debug)]
pub enum NetworkActorCommand {
    /// Network commands
    ConnectPeer(Multiaddr),
    DisconnectPeer(PeerId),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendFiberMessage(FiberMessageWithPeerId),
    // Open a channel to a peer.
    OpenChannel(
        OpenChannelCommand,
        RpcReplyPort<Result<OpenChannelResponse, String>>,
    ),
    // Accept a channel to a peer.
    AcceptChannel(
        AcceptChannelCommand,
        RpcReplyPort<Result<AcceptChannelResponse, String>>,
    ),
    // Send a command to a channel.
    ControlFiberChannel(ChannelCommandWithId),
    SendOnionPacket(Vec<u8>, Option<(Hash256, u64)>),
    UpdateChannelFunding(Hash256, Transaction, FundingRequest),
    SignTx(PeerId, Hash256, Transaction, Option<Vec<Vec<u8>>>),
    // Broadcast node/channel information.
    // The vector of PeerId is the list of peers that should receive the message.
    // This is useful when some peers are preferred to receive the message.
    // e.g. the ChannelUpdate message should be received by the counterparty of the channel.
    // This message may be broadcasted to other peers if necessary.
    BroadcastMessage(Vec<PeerId>, FiberBroadcastMessage),
    // Broadcast local information to the network.
    BroadcastLocalInfo(LocalInfoKind),
    SignMessage([u8; 32], RpcReplyPort<EcdsaSignature>),
    // Payment related commands
    SendPayment(
        SendPaymentCommand,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    GetAndProcessChannelsWithinBlockRangeFromPeer(
        (PeerId, u64, u64),
        RpcReplyPort<Result<(), Error>>,
    ),
    GetAndProcessBroadcastMessagesWithinTimeRangeFromPeer(
        (PeerId, u64, u64),
        RpcReplyPort<Result<(), Error>>,
    ),
    MarkSyncingDone,
}

pub async fn sign_network_message(
    network: ActorRef<NetworkActorMessage>,
    message: [u8; 32],
) -> std::result::Result<EcdsaSignature, RactorErr<NetworkActorMessage>> {
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::SignMessage(message, rpc_reply))
    };

    call!(network, message)
}

#[derive(Debug)]
pub struct OpenChannelCommand {
    pub peer_id: PeerId,
    pub funding_amount: u128,
    pub public: bool,
    pub funding_udt_type_script: Option<Script>,
    pub commitment_fee_rate: Option<u64>,
    pub funding_fee_rate: Option<u64>,
    pub tlc_locktime_expiry_delta: Option<u64>,
    pub tlc_min_value: Option<u128>,
    pub tlc_max_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_num_of_accept_tlcs: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentCommand {
    // the identifier of the payment target
    pub target_pubkey: Pubkey,

    // the amount of the payment
    pub amount: u128,

    // The hash to use within the payment's HTLC
    // FIXME: this should be optional when AMP is enabled
    pub payment_hash: Hash256,

    // The CLTV delta from the current height that should be used to set the timelock for the final hop
    pub final_cltv_delta: Option<u64>,

    // the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    // the payment timeout in seconds, if the payment is not completed within this time, it will be cancelled
    pub timeout: Option<u64>,

    // the maximum fee amounts in shannons that the sender is willing to pay
    pub max_fee_amount: Option<u128>,

    // max parts for the payment, only used for multi-part payments
    pub max_parts: Option<u64>,
}

#[derive(Debug)]
pub struct AcceptChannelCommand {
    pub temp_channel_id: Hash256,
    pub funding_amount: u128,
}

impl NetworkActorMessage {
    pub fn new_event(event: NetworkActorEvent) -> Self {
        Self::Event(event)
    }

    pub fn new_command(command: NetworkActorCommand) -> Self {
        Self::Command(command)
    }
}

#[derive(Debug)]
pub enum NetworkServiceEvent {
    ServiceError(ServiceError),
    ServiceEvent(ServiceEvent),
    NetworkStarted(PeerId, MultiAddr, Vec<Multiaddr>),
    PeerConnected(PeerId, Multiaddr),
    PeerDisConnected(PeerId, Multiaddr),
    // An incoming/outgoing channel is created.
    ChannelCreated(PeerId, Hash256),
    // A outgoing channel is pending to be accepted.
    ChannelPendingToBeAccepted(PeerId, Hash256),
    // The channel is ready to use (with funding transaction confirmed
    // and both parties sent ChannelReady messages).
    ChannelReady(PeerId, Hash256),
    ChannelClosed(PeerId, Hash256, Byte32),
    // We should sign a commitment transaction and send it to the other party.
    CommitmentSignaturePending(PeerId, Hash256, u64),
    // We have signed a commitment transaction and sent it to the other party.
    LocalCommitmentSigned(
        PeerId,          /* Peer Id */
        Hash256,         /* Channel Id */
        u64,             /* Commitment number */
        TransactionView, /* Commitment transaction, not valid per se (requires other party's signature) */
        Vec<u8>,         /* Commitment transaction witness */
    ),
    // A RevokeAndAck is received from the peer. Other data relevant to this
    // RevokeAndAck message are also assembled here. The watch tower may use this.
    // TODO: We also need transaction hash from the event `LocalCommitmentSigned` above
    // for the watch tower to watch older transactions being broadcasted.
    RevokeAndAckReceived(
        PeerId,  /* Peer Id */
        Hash256, /* Channel Id */
        u64,     /* Commitment number */
        Privkey, /* Revocation secret */
        Pubkey,  /* Revocation base point */
        Vec<u8>, /* Commitment transaction witness */
        Pubkey,  /* Next commitment point */
    ),
    // The other party has signed a valid commitment transaction,
    // and we successfully assemble the partial signature from other party
    // to create a complete commitment transaction.
    RemoteCommitmentSigned(PeerId, Hash256, u64, TransactionView),
}

/// Events that can be sent to the network actor. Except for NetworkServiceEvent,
/// all events are processed by the network actor.
#[derive(Debug)]
pub enum NetworkActorEvent {
    /// Network eventss to be processed by this actor.
    PeerConnected(PeerId, Pubkey, SessionContext),
    PeerDisconnected(PeerId, SessionContext),
    PeerMessage(PeerId, FiberMessage),

    /// Channel related events.

    /// A new channel is created and the peer id and actor reference is given here.
    /// Note the channel_id here maybe a temporary channel id.
    ChannelCreated(Hash256, PeerId, ActorRef<ChannelActorMessage>),
    /// A channel has been accepted.
    /// The two Hash256 are respectively newly agreed channel id and temp channel id,
    /// The two u128 are respectively local and remote funding amount,
    /// and the script is the lock script of the agreed funding cell.
    ChannelAccepted(
        PeerId,
        Hash256,
        Hash256,
        u128,
        u128,
        Script,
        Option<Script>,
        u64,
        u64,
        u64,
    ),
    /// A channel is ready to use.
    ChannelReady(Hash256, PeerId, OutPoint),
    /// A channel is already closed.
    ClosingTransactionPending(Hash256, PeerId, TransactionView),

    /// Both parties are now able to broadcast a valid funding transaction.
    FundingTransactionPending(Transaction, OutPoint, Hash256),

    /// A funding transaction has been confirmed.
    FundingTransactionConfirmed(OutPoint),

    /// A funding transaction has been confirmed.
    FundingTransactionFailed(OutPoint),

    /// A commitment transaction is signed by us and has sent to the other party.
    LocalCommitmentSigned(PeerId, Hash256, u64, TransactionView, Vec<u8>),

    /// Channel is going to be closed forcely, and the closing transaction is ready to be broadcasted.
    CommitmentTransactionPending(Transaction, Hash256),

    /// A commitment transaction is broacasted successfully.
    CommitmentTransactionConfirmed(Hash256, Hash256),

    /// A commitment transaction is failed to be broacasted.
    CommitmentTransactionFailed(Hash256, Byte32),

    /// A closing transaction has been confirmed.
    ClosingTransactionConfirmed(PeerId, Hash256, Byte32),

    /// A closing transaction has failed (either because of invalid transaction or timeout)
    ClosingTransactionFailed(PeerId, Hash256, Byte32),

    // The graph syncer to the peer has exited with some reason.
    GraphSyncerExited(PeerId, GraphSyncerExitStatus),

    /// Network service events to be sent to outside observers.
    /// These events may be both present at `NetworkActorEvent` and
    /// this branch of `NetworkActorEvent`. This is because some events
    /// (e.g. `ChannelClosed`)require some processing internally,
    /// and they are also interesting to outside observers.
    /// Once we processed these events, we will send them to outside observers.
    NetworkServiceEvent(NetworkServiceEvent),
}

#[derive(Copy, Clone, Debug)]
pub enum GraphSyncerExitStatus {
    Succeeded,
    Failed,
}

#[derive(Debug)]
pub enum NetworkActorMessage {
    Command(NetworkActorCommand),
    Event(NetworkActorEvent),
}

#[derive(Debug)]
pub struct FiberMessageWithPeerId {
    pub peer_id: PeerId,
    pub message: FiberMessage,
}

impl FiberMessageWithPeerId {
    pub fn new(peer_id: PeerId, message: FiberMessage) -> Self {
        Self { peer_id, message }
    }
}

#[derive(Debug)]
pub struct FiberMessageWithSessionId {
    pub session_id: SessionId,
    pub message: FiberMessage,
}

#[derive(Debug)]
pub struct FiberMessageWithChannelId {
    pub channel_id: Hash256,
    pub message: FiberMessage,
}

pub struct NetworkActor<S> {
    // An event emitter to notify ourside observers.
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    chain_actor: ActorRef<CkbChainMessage>,
    store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
}

impl<S> NetworkActor<S>
where
    S: ChannelActorStateStore + NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    pub fn new(
        event_sender: mpsc::Sender<NetworkServiceEvent>,
        chain_actor: ActorRef<CkbChainMessage>,
        store: S,
        my_pubkey: Pubkey,
    ) -> Self {
        Self {
            event_sender,
            chain_actor,
            store: store.clone(),
            network_graph: Arc::new(RwLock::new(NetworkGraph::new(store, my_pubkey))),
        }
    }

    pub async fn on_service_event(&self, event: NetworkServiceEvent) {
        let _ = self.event_sender.send(event).await;
    }

    pub async fn handle_peer_message(
        &self,
        state: &mut NetworkActorState,
        peer_id: PeerId,
        message: FiberMessage,
    ) -> crate::Result<()> {
        match message {
            // We should process OpenChannel message here because there is no channel corresponding
            // to the channel id in the message yet.
            FiberMessage::ChannelInitialization(open_channel) => {
                let temp_channel_id = open_channel.channel_id;
                match state
                    .on_open_channel_msg(peer_id, open_channel.clone())
                    .await
                {
                    Ok(()) => {
                        let auto_accept = if let Some(udt_type_script) =
                            open_channel.funding_udt_type_script
                        {
                            is_udt_type_auto_accept(&udt_type_script, open_channel.funding_amount)
                        } else {
                            state.auto_accept_channel_ckb_funding_amount > 0
                                && open_channel.all_ckb_amount()
                                    >= state.open_channel_auto_accept_min_ckb_funding_amount
                        };
                        if auto_accept {
                            let accept_channel = AcceptChannelCommand {
                                temp_channel_id,
                                funding_amount: state.auto_accept_channel_ckb_funding_amount
                                    as u128,
                            };
                            state
                                .create_inbound_channel(accept_channel, self.store.clone())
                                .await?;
                        }
                    }
                    Err(err) => {
                        error!("Failed to process OpenChannel message: {}", err);
                    }
                }
            }
            FiberMessage::BroadcastMessage(m) => {
                if let Err(e) = self
                    .process_or_stash_broadcasted_message(state, peer_id, m)
                    .await
                {
                    error!("Failed to process broadcasted message: {:?}", e);
                }
            }
            FiberMessage::ChannelNormalOperation(m) => {
                let channel_id = m.get_channel_id();
                state
                    .send_message_to_channel_actor(channel_id, ChannelActorMessage::PeerMessage(m));
            }
            FiberMessage::QueryInformation(q) => match q {
                FiberQueryInformation::GetBroadcastMessages(GetBroadcastMessages {
                    id,
                    queries,
                }) => {
                    let mut messages = Vec::with_capacity(queries.len());
                    for query in queries {
                        let result = self.query_broadcast_message(query).await;
                        match result {
                            Ok(message) => {
                                messages.push(message);
                            }
                            Err(e) => {
                                error!("Failed to query broadcast message: {:?}", e);
                                return Ok(());
                            }
                        }
                    }
                    let reply = GetBroadcastMessagesResult { id, messages };
                    state
                        .send_message_to_peer(
                            &peer_id,
                            FiberMessage::QueryInformation(
                                FiberQueryInformation::GetBroadcastMessagesResult(reply),
                            ),
                        )
                        .await?;
                }
                FiberQueryInformation::GetBroadcastMessagesResult(GetBroadcastMessagesResult {
                    id,
                    messages,
                }) => {
                    debug!("Received GetBroadcastMessagesResult from peer {:?} with id {} and result {:?}", &peer_id, id, &messages);
                    let reply_port = match state.get_reply_port_for_request(&peer_id, id) {
                        Some(reply_port) => reply_port,
                        None => {
                            return Err(Error::InvalidPeerMessage(format!(
                                "No reply port for query broadcast messages with id {} from peer {:?}",
                                id, &peer_id)
                            ));
                        }
                    };
                    for message in messages {
                        if let Err(e) = self
                            .process_broadcasted_message(&state.network, message)
                            .await
                        {
                            let fail_message =
                                format!("Failed to process broadcasted message: {:?}", &e);
                            error!("{}", &fail_message);
                            let _ = reply_port.send(Err(e));
                            return Err(Error::InvalidPeerMessage(fail_message));
                        }
                    }
                    debug!(
                        "Successfully processed all the messages from peer {:?} with id {}",
                        &peer_id, id
                    );
                    let _ = reply_port.send(Ok(()));
                }
                FiberQueryInformation::QueryChannelsWithinBlockRange(
                    QueryChannelsWithinBlockRange {
                        id,
                        chain_hash: _,
                        start_block,
                        end_block,
                    },
                ) => {
                    let channels = self
                        .query_channels_within_block_range(start_block, end_block)
                        .await
                        .into_iter()
                        .map(|c| c.out_point())
                        .collect();
                    let reply = FiberQueryInformation::QueryChannelsWithinBlockRangeResult(
                        QueryChannelsWithinBlockRangeResult { id, channels },
                    );
                    state
                        .send_message_to_peer(&peer_id, FiberMessage::QueryInformation(reply))
                        .await?;
                }
                FiberQueryInformation::QueryChannelsWithinBlockRangeResult(
                    QueryChannelsWithinBlockRangeResult { id, channels },
                ) => {
                    if channels.is_empty() {
                        // No query to the peer needed, early return.
                        match state
                            .broadcast_message_responses
                            .remove(&(peer_id.clone(), id))
                        {
                            Some(reply) => {
                                let _ = reply.send(Ok(()));
                                return Ok(());
                            }
                            _ => {
                                return Err(Error::InvalidPeerMessage(format!(
                                    "No response for query channels with id {} expected from peer {:?}",
                                    id, &peer_id
                                )));
                            }
                        }
                    }
                    debug!("Received QueryChannelsWithinBlockRangeResult from peer {:?} with id {} and channels {:?}", &peer_id, id, &channels);
                    if let Some(new_id) = state.derive_new_request_id(&peer_id, id) {
                        let query = GetBroadcastMessages {
                            id: new_id,
                            queries: channels
                                .into_iter()
                                .map(|channel_outpoint: OutPoint| {
                                    FiberBroadcastMessageQuery::ChannelAnnouncement(
                                        ChannelAnnouncementQuery {
                                            channel_outpoint,
                                            flags: 0,
                                        },
                                    )
                                })
                                .collect(),
                        };
                        debug!("Trying to query peer {:?} channels {:?}", &peer_id, &query);
                        state
                            .send_message_to_peer(
                                &peer_id,
                                FiberMessage::QueryInformation(
                                    FiberQueryInformation::GetBroadcastMessages(query),
                                ),
                            )
                            .await?;
                    } else {
                        return Err(Error::InvalidPeerMessage(format!(
                            "No response for query channels with id {} expected from peer {:?}",
                            id, &peer_id
                        )));
                    }
                }
                FiberQueryInformation::QueryBroadcastMessagesWithinTimeRange(
                    QueryBroadcastMessagesWithinTimeRange {
                        id,
                        chain_hash: _,
                        start_time,
                        end_time,
                    },
                ) => {
                    let queries = self
                        .query_broadcast_messages_within_time_range(start_time, end_time)
                        .await?;
                    let reply = FiberQueryInformation::QueryBroadcastMessagesWithinTimeRangeResult(
                        QueryBroadcastMessagesWithinTimeRangeResult { id, queries },
                    );
                    state
                        .send_message_to_peer(&peer_id, FiberMessage::QueryInformation(reply))
                        .await?;
                }
                FiberQueryInformation::QueryBroadcastMessagesWithinTimeRangeResult(
                    QueryBroadcastMessagesWithinTimeRangeResult { id, queries },
                ) => {
                    if queries.is_empty() {
                        // No query to the peer needed, early return.
                        match state
                            .broadcast_message_responses
                            .remove(&(peer_id.clone(), id))
                        {
                            Some(reply) => {
                                let _ = reply.send(Ok(()));
                                return Ok(());
                            }
                            _ => {
                                return Err(Error::InvalidPeerMessage(format!(
                                    "No response for query broadcast messages with id {} expected from peer {:?}",
                                    id, &peer_id
                                )));
                            }
                        }
                    }

                    if let Some(new_id) = state.derive_new_request_id(&peer_id, id) {
                        let query = GetBroadcastMessages {
                            id: new_id,
                            queries,
                        };
                        state
                            .send_message_to_peer(
                                &peer_id,
                                FiberMessage::QueryInformation(
                                    FiberQueryInformation::GetBroadcastMessages(query),
                                ),
                            )
                            .await?;
                    } else {
                        return Err(Error::InvalidPeerMessage(format!(
                            "No response for query broadcast messages with id {} expected from peer {:?}",
                            id, &peer_id
                        )));
                    }
                }
            },
        };
        Ok(())
    }

    pub async fn query_channels_within_block_range(
        &self,
        start_block: u64,
        end_block: u64,
    ) -> Vec<ChannelInfo> {
        let network_graph = self.network_graph.read().await;
        network_graph
            .get_channels_within_block_range(start_block, end_block)
            .cloned()
            .collect()
    }

    pub async fn query_broadcast_messages_within_time_range(
        &self,
        start_time: u64,
        end_time: u64,
    ) -> Result<Vec<FiberBroadcastMessageQuery>, Error> {
        let start_time = start_time as u64;
        let end_time = end_time as u64;
        let is_within_range = |timestamp: u64| timestamp >= start_time && timestamp < end_time;
        let network_graph = self.network_graph.read().await;

        let mut queries = Vec::new();
        for node_info in network_graph.nodes() {
            let node_announcement = &node_info.anouncement_msg;
            if is_within_range(node_announcement.version) {
                queries.push(FiberBroadcastMessageQuery::NodeAnnouncement(
                    NodeAnnouncementQuery {
                        node_id: node_info.node_id.clone(),
                        flags: 0,
                    },
                ));
            }
        }
        for channel_info in network_graph.channels() {
            if is_within_range(channel_info.channel_annoucement_timestamp()) {
                queries.push(FiberBroadcastMessageQuery::ChannelAnnouncement(
                    ChannelAnnouncementQuery {
                        channel_outpoint: channel_info.out_point(),
                        flags: 0,
                    },
                ));
            }

            if let Some(t) = channel_info.channel_update_one_to_two_timestamp() {
                if is_within_range(t) {
                    queries.push(FiberBroadcastMessageQuery::ChannelUpdate(
                        ChannelUpdateQuery {
                            channel_outpoint: channel_info.out_point(),
                            flags: channel_info.one_to_two_channel_update_flags(),
                        },
                    ));
                }
            }

            if let Some(t) = channel_info.channel_update_two_to_one_timestamp() {
                if is_within_range(t) {
                    queries.push(FiberBroadcastMessageQuery::ChannelUpdate(
                        ChannelUpdateQuery {
                            channel_outpoint: channel_info.out_point(),
                            flags: channel_info.two_to_one_channel_update_flags(),
                        },
                    ));
                }
            }
        }
        Ok(queries)
    }

    pub async fn query_broadcast_message(
        &self,
        query: FiberBroadcastMessageQuery,
    ) -> Result<FiberBroadcastMessage, Error> {
        let network_graph = self.network_graph.read().await;
        match query {
            FiberBroadcastMessageQuery::NodeAnnouncement(NodeAnnouncementQuery {
                node_id,
                flags: _,
            }) => {
                let node_info = network_graph.get_node(node_id.clone());
                match node_info {
                    Some(node_info) => Ok(FiberBroadcastMessage::NodeAnnouncement(
                        node_info.anouncement_msg.clone(),
                    )),
                    None => Err(Error::InvalidParameter(format!(
                        "Node not found: {:?}",
                        &node_id
                    ))),
                }
            }
            FiberBroadcastMessageQuery::ChannelAnnouncement(ChannelAnnouncementQuery {
                channel_outpoint,
                flags: _,
            }) => {
                let channel_info = network_graph.get_channel(&channel_outpoint);
                match channel_info {
                    Some(channel_info) => {
                        let channel_announcement = FiberBroadcastMessage::ChannelAnnouncement(
                            channel_info.announcement_msg.clone(),
                        );
                        Ok(channel_announcement)
                    }
                    None => Err(Error::InvalidParameter(format!(
                        "Channel not found: {:?}",
                        &channel_outpoint
                    ))),
                }
            }
            FiberBroadcastMessageQuery::ChannelUpdate(ChannelUpdateQuery {
                channel_outpoint,
                flags,
            }) => {
                let channel_info = network_graph.get_channel(&channel_outpoint);
                let is_one_to_two = flags & 1 == 0;
                match channel_info {
                    Some(channel_info) => {
                        let update = if is_one_to_two {
                            channel_info
                                .one_to_two
                                .as_ref()
                                .and_then(|u| u.0.last_update_message.clone())
                        } else {
                            channel_info
                                .two_to_one
                                .as_ref()
                                .and_then(|u| u.0.last_update_message.clone())
                        };
                        match update {
                            Some(update) => Ok(FiberBroadcastMessage::ChannelUpdate(update)),
                            None => Err(Error::InvalidParameter(format!(
                                "Channel update not found: {:?}",
                                &channel_outpoint
                            ))),
                        }
                    }
                    None => Err(Error::InvalidParameter(format!(
                        "Channel not found: {:?}",
                        &channel_outpoint
                    ))),
                }
            }
        }
    }

    pub async fn handle_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState,
        event: NetworkActorEvent,
    ) -> crate::Result<()> {
        debug!("Handling event: {:?}", event);
        match event {
            NetworkActorEvent::NetworkServiceEvent(e) => {
                match &e {
                    NetworkServiceEvent::ServiceError(ServiceError::DialerError {
                        address,
                        error,
                    }) => {
                        error!("Dialer error: {:?} -> {:?}", address, error);
                        state.maybe_tell_syncer_peer_disconnected_multiaddr(address)
                    }
                    _ => {}
                }
                self.on_service_event(e).await;
            }
            NetworkActorEvent::PeerConnected(id, pubkey, session) => {
                self.network_graph
                    .write()
                    .await
                    .add_connected_peer(&id, session.address.clone());
                state
                    .on_peer_connected(&id, pubkey, &session, self.store.clone())
                    .await;
                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::NetworkServiceEvent(NetworkServiceEvent::PeerConnected(
                            id,
                            session.address,
                        )),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            NetworkActorEvent::PeerDisconnected(id, session) => {
                self.network_graph.write().await.remove_connected_peer(&id);
                state.on_peer_disconnected(&id);
                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::NetworkServiceEvent(
                            NetworkServiceEvent::PeerDisConnected(id, session.address),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            NetworkActorEvent::ChannelCreated(channel_id, peer_id, actor) => {
                state.on_channel_created(channel_id, &peer_id, actor);
                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::NetworkServiceEvent(
                            NetworkServiceEvent::ChannelCreated(peer_id, channel_id),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            NetworkActorEvent::ChannelAccepted(
                peer_id,
                new,
                old,
                local,
                remote,
                script,
                udt_funding_script,
                local_reserved_ckb_amount,
                remote_reserved_ckb_amount,
                funding_fee_rate,
            ) => {
                assert_ne!(new, old, "new and old channel id must be different");
                if let Some(session) = state.get_peer_session(&peer_id) {
                    state.check_accept_channel_ckb_parameters(
                        local_reserved_ckb_amount,
                        remote_reserved_ckb_amount,
                        funding_fee_rate,
                        &udt_funding_script,
                    )?;
                    if let Some(channel) = state.channels.remove(&old) {
                        debug!("Channel accepted: {:?} -> {:?}", old, new);
                        state.channels.insert(new, channel);
                        if let Some(set) = state.session_channels_map.get_mut(&session) {
                            set.remove(&old);
                            set.insert(new);
                        };

                        debug!("Starting funding channel");
                        // TODO: Here we implies the one who receives AcceptChannel message
                        //  (i.e. the channel initiator) will send TxUpdate message first.
                        myself
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::UpdateChannelFunding(
                                    new,
                                    Default::default(),
                                    FundingRequest {
                                        script,
                                        udt_type_script: udt_funding_script,
                                        local_amount: local as u64,
                                        funding_fee_rate,
                                        remote_amount: remote as u64,
                                        local_reserved_ckb_amount,
                                        remote_reserved_ckb_amount,
                                    },
                                ),
                            ))
                            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                    }
                }
            }
            NetworkActorEvent::ChannelReady(channel_id, peer_id, channel_outpoint) => {
                info!(
                    "Channel ({:?}) to peer {:?} is now ready",
                    channel_id, peer_id
                );

                // FIXME(yukang): need to make sure ChannelReady is sent after the channel is reestablished
                state
                    .outpoint_channel_map
                    .insert(channel_outpoint, channel_id);

                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::NetworkServiceEvent(NetworkServiceEvent::ChannelReady(
                            peer_id, channel_id,
                        )),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            NetworkActorEvent::PeerMessage(peer_id, message) => {
                self.handle_peer_message(state, peer_id, message).await?
            }
            NetworkActorEvent::FundingTransactionPending(transaction, outpoint, channel_id) => {
                state
                    .on_funding_transaction_pending(transaction, outpoint.clone(), channel_id)
                    .await;
            }
            NetworkActorEvent::FundingTransactionConfirmed(outpoint) => {
                state.on_funding_transaction_confirmed(outpoint).await;
            }
            NetworkActorEvent::CommitmentTransactionPending(transaction, channel_id) => {
                state
                    .on_commitment_transaction_pending(transaction, channel_id)
                    .await;
            }
            NetworkActorEvent::CommitmentTransactionConfirmed(tx_hash, channel_id) => {
                state
                    .on_commitment_transaction_confirmed(tx_hash, channel_id)
                    .await;
            }
            NetworkActorEvent::CommitmentTransactionFailed(tx_hash, channel_id) => {
                error!(
                    "Commitment transaction failed for channel {:?}, tx hash: {:?}",
                    channel_id, tx_hash
                );
            }
            NetworkActorEvent::FundingTransactionFailed(outpoint) => {
                error!("Funding transaction failed: {:?}", outpoint);
            }
            NetworkActorEvent::ClosingTransactionPending(channel_id, peer_id, tx) => {
                state
                    .on_closing_transaction_pending(channel_id, peer_id.clone(), tx.clone())
                    .await;
            }
            NetworkActorEvent::ClosingTransactionConfirmed(peer_id, channel_id, tx_hash) => {
                state
                    .on_closing_transaction_confirmed(&peer_id, &channel_id, tx_hash)
                    .await;
            }
            NetworkActorEvent::ClosingTransactionFailed(peer_id, tx_hash, channel_id) => {
                error!(
                    "Closing transaction failed for channel {:?}, tx hash: {:?}, peer id: {:?}",
                    &channel_id, &tx_hash, &peer_id
                );
            }
            NetworkActorEvent::LocalCommitmentSigned(
                peer_id,
                channel_id,
                version,
                tx,
                witnesses,
            ) => {
                // TODO: We should save the witnesses to the channel state.
                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::NetworkServiceEvent(
                            NetworkServiceEvent::LocalCommitmentSigned(
                                peer_id, channel_id, version, tx, witnesses,
                            ),
                        ),
                    ))
                    .expect("myself alive");
            }
            NetworkActorEvent::GraphSyncerExited(peer_id, reason) => {
                debug!(
                    "Graph syncer to peer {:?} has exited with reason {:?}",
                    &peer_id, &reason
                );
                if let NetworkSyncStatus::Running(state) = &mut state.sync_status {
                    if let Some(actor) = state.active_syncers.remove(&peer_id) {
                        actor.get_cell().kill();
                    }
                    debug!("Changing sync succeeded/failed counter");
                    match reason {
                        GraphSyncerExitStatus::Succeeded => state.succeeded = state.succeeded + 1,
                        GraphSyncerExitStatus::Failed => state.failed = state.failed + 1,
                    }
                }
                state.maybe_finish_sync();
            }
        }
        Ok(())
    }

    pub async fn handle_command(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState,
        command: NetworkActorCommand,
    ) -> crate::Result<()> {
        debug!("Handling command: {:?}", command);
        match command {
            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId { peer_id, message }) => {
                state.send_message_to_peer(&peer_id, message).await?;
            }

            NetworkActorCommand::ConnectPeer(addr) => {
                // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                // e.g. whether the peer support some specific feature.
                // TODO: If we are already connected to the peer, skip connecting.
                state
                    .control
                    .dial(addr.clone(), TargetProtocol::All)
                    .await?
                // TODO: note that the dial function does not return error immediately even if dial fails.
                // Tentacle sends an event by calling handle_error function instead, which
                // may receive errors like DialerError.
            }

            NetworkActorCommand::DisconnectPeer(peer_id) => {
                if let Some(session) = state.get_peer_session(&peer_id) {
                    state.control.disconnect(session).await?;
                }
            }

            NetworkActorCommand::OpenChannel(open_channel, reply) => {
                match state
                    .create_outbound_channel(open_channel, self.store.clone())
                    .await
                {
                    Ok((_, channel_id)) => {
                        let _ = reply.send(Ok(OpenChannelResponse { channel_id }));
                    }
                    Err(err) => {
                        error!("Failed to create channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }
            NetworkActorCommand::AcceptChannel(accept_channel, reply) => {
                match state
                    .create_inbound_channel(accept_channel, self.store.clone())
                    .await
                {
                    Ok((_, old_channel_id, new_channel_id)) => {
                        let _ = reply.send(Ok(AcceptChannelResponse {
                            old_channel_id,
                            new_channel_id,
                        }));
                    }
                    Err(err) => {
                        error!("Failed to accept channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }

            NetworkActorCommand::ControlFiberChannel(c) => {
                info!("send command to channel: {:?}", c);
                state
                    .send_command_to_channel(c.channel_id, c.command)
                    .await?
            }

            NetworkActorCommand::SendOnionPacket(packet, previous_tlc) => {
                if let Ok(mut onion_packet) = OnionPacket::deserialize(&packet) {
                    info!("onion packet: {:?}", onion_packet);
                    if let Ok(hop) = onion_packet.shift() {
                        let channel_id = state
                            .outpoint_channel_map
                            .get(&hop.channel_outpoint.unwrap())
                            .expect("channel id should exist");
                        let (send, recv) = oneshot::channel::<Result<AddTlcResponse, String>>();
                        let rpc_reply = RpcReplyPort::from(send);
                        let command = ChannelCommand::AddTlc(
                            AddTlcCommand {
                                amount: hop.amount,
                                preimage: None,
                                payment_hash: Some(hop.payment_hash),
                                expiry: hop.expiry.into(),
                                hash_algorithm: HashAlgorithm::Sha256,
                                onion_packet: onion_packet.serialize(),
                                previous_tlc,
                            },
                            rpc_reply,
                        );
                        state.send_command_to_channel(*channel_id, command).await?;
                        let res = recv.await.expect("recv add tlc response");
                        info!("send onion packet: {:?}", res);
                    }
                } else {
                    info!("onion packet is empty, ignore it");
                }
            }
            NetworkActorCommand::UpdateChannelFunding(channel_id, transaction, request) => {
                let old_tx = transaction.into_view();
                let mut tx = FundingTx::new();
                tx.update_for_self(old_tx)?;
                let tx = match call_t!(
                    self.chain_actor.clone(),
                    CkbChainMessage::Fund,
                    DEFAULT_CHAIN_ACTOR_TIMEOUT,
                    tx,
                    request
                ) {
                    Ok(Ok(tx)) => match tx.into_inner() {
                        Some(tx) => tx,
                        _ => {
                            error!("Obtained empty funding tx");
                            return Ok(());
                        }
                    },
                    Ok(Err(err)) => {
                        // FIXME(yukang): we need to handle this error properly
                        error!("Failed to fund channel: {}", err);
                        return Ok(());
                    }
                    Err(err) => {
                        error!("Failed to call chain actor: {}", err);
                        return Ok(());
                    }
                };
                debug!("Funding transaction updated on our part: {:?}", tx);
                state
                    .send_command_to_channel(
                        channel_id,
                        ChannelCommand::TxCollaborationCommand(TxCollaborationCommand::TxUpdate(
                            TxUpdateCommand {
                                transaction: tx.data(),
                            },
                        )),
                    )
                    .await?
            }
            NetworkActorCommand::SignTx(
                ref peer_id,
                ref channel_id,
                funding_tx,
                partial_witnesses,
            ) => {
                let msg = match partial_witnesses {
                    Some(partial_witnesses) => {
                        debug!(
                            "Received SignTx request with for transaction {:?} and partial witnesses {:?}",
                            &funding_tx,
                            partial_witnesses
                                .iter()
                                .map(hex::encode)
                                .collect::<Vec<_>>()
                        );
                        let funding_tx = funding_tx
                            .into_view()
                            .as_advanced_builder()
                            .set_witnesses(
                                partial_witnesses.into_iter().map(|x| x.pack()).collect(),
                            )
                            .build();

                        let mut funding_tx = call_t!(
                            self.chain_actor,
                            CkbChainMessage::Sign,
                            DEFAULT_CHAIN_ACTOR_TIMEOUT,
                            funding_tx.into()
                        )
                        .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
                        .expect("Signing succeeded");
                        debug!("Funding transaction signed: {:?}", &funding_tx);

                        // Since we have received a valid tx_signatures message, we're now sure that
                        // we can broadcast a valid transaction to the network, i.e. we can wait for
                        // the funding transaction to be confirmed.
                        let funding_tx = funding_tx.take().expect("take tx");
                        let witnesses = funding_tx.witnesses();
                        let outpoint = funding_tx
                            .output_pts_iter()
                            .next()
                            .expect("funding tx output exists");

                        myself
                            .send_message(NetworkActorMessage::new_event(
                                NetworkActorEvent::FundingTransactionPending(
                                    funding_tx.data(),
                                    outpoint,
                                    *channel_id,
                                ),
                            ))
                            .expect("network actor alive");
                        debug!("Fully signed funding tx {:?}", &funding_tx);

                        FiberMessageWithPeerId {
                            peer_id: peer_id.clone(),
                            message: FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::TxSignatures(TxSignatures {
                                    channel_id: *channel_id,
                                    witnesses: witnesses.into_iter().map(|x| x.unpack()).collect(),
                                    tx_hash: funding_tx.hash().into(),
                                }),
                            ),
                        }
                    }
                    None => {
                        debug!(
                            "Received SignTx request with for transaction {:?} without partial witnesses, so start signing it now",
                            &funding_tx,
                        );
                        let mut funding_tx = call_t!(
                            self.chain_actor,
                            CkbChainMessage::Sign,
                            DEFAULT_CHAIN_ACTOR_TIMEOUT,
                            funding_tx.into()
                        )
                        .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)?;
                        debug!("Funding transaction signed: {:?}", &funding_tx);
                        let funding_tx = funding_tx.take().expect("take tx");
                        let witnesses = funding_tx.witnesses();

                        debug!("Partially signed funding tx {:?}", &funding_tx);
                        FiberMessageWithPeerId {
                            peer_id: peer_id.clone(),
                            message: FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::TxSignatures(TxSignatures {
                                    channel_id: *channel_id,
                                    witnesses: witnesses.into_iter().map(|x| x.unpack()).collect(),
                                    tx_hash: funding_tx.hash().into(),
                                }),
                            ),
                        }
                    }
                };
                debug!(
                    "Handled tx_signatures, peer: {:?}, messge to send: {:?}",
                    &peer_id, &msg
                );
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(msg),
                    ))
                    .expect("network actor alive");
            }
            NetworkActorCommand::BroadcastMessage(peers, message) => {
                // Send message to peers in the list anyway.
                debug!("Broadcasting message {:?} to peers {:?}", &message, &peers);
                for peer_id in &peers {
                    if let Err(e) = state
                        .send_message_to_peer(
                            peer_id,
                            FiberMessage::BroadcastMessage(message.clone()),
                        )
                        .await
                    {
                        error!(
                            "Failed to broadcast message {:?} to peer {:?}: {:?}",
                            &message, peer_id, e
                        );
                    }
                }

                const MAX_BROADCAST_SESSIONS: usize = 5;
                let peer_ids =
                    state.get_n_peer_peer_ids(MAX_BROADCAST_SESSIONS, peers.into_iter().collect());
                debug!("Broadcasting message random selected peers {:?}", &peer_ids);
                // The order matters here because should_message_be_broadcasted
                // will change the state, and we don't want to change the state
                // if there is not peer to broadcast the message.
                if !peer_ids.is_empty() && state.should_message_be_broadcasted(&message) {
                    debug!(
                        "Broadcasting unseen message {:?} to peers {:?}",
                        &message, &peer_ids
                    );
                    for peer_id in peer_ids {
                        if let Err(e) = state
                            .send_message_to_peer(
                                &peer_id,
                                FiberMessage::BroadcastMessage(message.clone()),
                            )
                            .await
                        {
                            error!(
                                "Failed to broadcast message {:?} to peer {:?}: {:?}",
                                &message, &peer_id, e
                            );
                        }
                    }
                }
            }
            NetworkActorCommand::SignMessage(message, reply) => {
                let signature = state.private_key.sign(message);
                let _ = reply.send(signature);
            }
            NetworkActorCommand::SendPayment(payment_request, reply) => {
                let payment_hash = payment_request.payment_hash;
                let res = self.on_send_payment(myself, payment_request).await;
                info!("send_payment res: {:?}", res);
                let _ = reply.send(Ok(SendPaymentResponse { payment_hash }));
            }
            NetworkActorCommand::BroadcastLocalInfo(kind) => match kind {
                LocalInfoKind::NodeAnnouncement => {
                    if let Some(message) = state.get_node_announcement_message() {
                        myself
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::BroadcastMessage(
                                    vec![],
                                    FiberBroadcastMessage::NodeAnnouncement(message),
                                ),
                            ))
                            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                    }
                }
            },
            NetworkActorCommand::MarkSyncingDone => {
                state.sync_status = NetworkSyncStatus::Done;
                let mut broadcasted_message_queue = vec![];
                std::mem::swap(
                    &mut state.broadcasted_message_queue,
                    &mut broadcasted_message_queue,
                );
                for message in broadcasted_message_queue.drain(..) {
                    let (_peer_id, message) = message;
                    if let Err(e) = self
                        .process_broadcasted_message(&state.network, message)
                        .await
                    {
                        error!("Failed to process broadcasted message: {:?}", e);
                    }
                }
            }
            NetworkActorCommand::GetAndProcessChannelsWithinBlockRangeFromPeer(request, reply) => {
                // TODO: We need to send a reply to the caller if enough time passed,
                // but we still do not get a reply from the peer.
                let (peer_id, start_block, end_block) = request;
                let id = state.create_request_id_for_reply_port(&peer_id, reply);
                let message = FiberMessage::QueryInformation(
                    FiberQueryInformation::QueryChannelsWithinBlockRange(
                        QueryChannelsWithinBlockRange {
                            id,
                            chain_hash: get_chain_hash(),
                            start_block,
                            end_block,
                        },
                    ),
                );
                state.send_message_to_peer(&peer_id, message).await?;
            }
            NetworkActorCommand::GetAndProcessBroadcastMessagesWithinTimeRangeFromPeer(
                request,
                reply,
            ) => {
                // TODO: We need to send a reply to the caller if enough time passed,
                // but we still do not get a reply from the peer.
                let (peer_id, start_time, end_time) = request;
                let id = state.create_request_id_for_reply_port(&peer_id, reply);
                let message = FiberMessage::QueryInformation(
                    FiberQueryInformation::QueryBroadcastMessagesWithinTimeRange(
                        QueryBroadcastMessagesWithinTimeRange {
                            id,
                            chain_hash: get_chain_hash(),
                            start_time,
                            end_time,
                        },
                    ),
                );
                state.send_message_to_peer(&peer_id, message).await?;
            }
        };
        Ok(())
    }

    async fn process_or_stash_broadcasted_message(
        &self,
        state: &mut NetworkActorState,
        peer_id: PeerId,
        message: FiberBroadcastMessage,
    ) -> Result<(), Error> {
        if state.sync_status.is_syncing() {
            debug!(
                "Saving broadcasted message to queue as we are syncing: {:?}",
                &message
            );
            state.broadcasted_message_queue.push((peer_id, message));
            return Ok(());
        }
        // Rebroadcast the message to other peers if necessary.
        state
            .network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::BroadcastMessage(vec![], message.clone()),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        self.process_broadcasted_message(&state.network, message)
            .await
    }

    async fn process_broadcasted_message(
        &self,
        network: &ActorRef<NetworkActorMessage>,
        message: FiberBroadcastMessage,
    ) -> Result<(), Error> {
        match message {
            FiberBroadcastMessage::NodeAnnouncement(ref node_announcement) => {
                let message = node_announcement.message_to_sign();
                if !self
                    .network_graph
                    .read()
                    .await
                    .check_chain_hash(node_announcement.chain_hash)
                {
                    return Err(Error::InvalidParameter(format!(
                        "Node announcement chain hash mismatched: {:?}",
                        &node_announcement
                    )));
                }
                match node_announcement.signature {
                    Some(ref signature)
                        if signature.verify(&node_announcement.node_id, &message) =>
                    {
                        debug!(
                            "Node announcement message verified: {:?}",
                            &node_announcement
                        );

                        // Add the node to the network graph.
                        let node_info = NodeInfo {
                            node_id: node_announcement.node_id,
                            timestamp: std::time::UNIX_EPOCH.elapsed().unwrap().as_millis() as u64,
                            anouncement_msg: node_announcement.clone(),
                        };
                        self.network_graph.write().await.add_node(node_info);

                        // TODO: bookkeeping how many nodes we have connected to. Stop connnecting once we surpass a threshold.
                        for addr in &node_announcement.addresses {
                            network.send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::ConnectPeer(addr.clone()),
                            ))?;
                        }
                        Ok(())
                    }
                    _ => {
                        return Err(Error::InvalidParameter(format!(
                            "Node announcement message signature verification failed: {:?}",
                            &node_announcement
                        )));
                    }
                }
            }

            FiberBroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                debug!(
                    "Received channel announcement message: {:?}",
                    &channel_announcement
                );
                let message = channel_announcement.message_to_sign();
                if channel_announcement.node_1_id == channel_announcement.node_2_id {
                    return Err(Error::InvalidParameter(format!(
                        "Channel announcement node had a channel with itself: {:?}",
                        &channel_announcement
                    )));
                }
                if !self
                    .network_graph
                    .read()
                    .await
                    .check_chain_hash(channel_announcement.chain_hash)
                {
                    return Err(Error::InvalidParameter(format!(
                        "Channel announcement chain hash mismatched: {:?}",
                        &channel_announcement
                    )));
                }
                let (node_1_signature, node_2_signature, ckb_signature) = match (
                    channel_announcement.node_1_signature,
                    channel_announcement.node_2_signature,
                    channel_announcement.ckb_signature,
                ) {
                    (Some(node_1_signature), Some(node_2_signature), Some(ckb_signature)) => {
                        (node_1_signature, node_2_signature, ckb_signature)
                    }
                    _ => {
                        return Err(Error::InvalidParameter(format!(
                            "Channel announcement message signature verification failed, some signatures are missing: {:?}",
                            &channel_announcement
                        )));
                    }
                };

                if !node_1_signature.verify(&channel_announcement.node_1_id, &message) {
                    return Err(Error::InvalidParameter(format!(
                        "Channel announcement message signature verification failed for node 1: {:?}, message: {:?}, signature: {:?}, pubkey: {:?}",
                        &channel_announcement,
                        &message,
                        &node_1_signature,
                        &channel_announcement.node_1_id
                    )));
                }

                if !node_2_signature.verify(&channel_announcement.node_2_id, &message) {
                    return Err(Error::InvalidParameter(format!(
                        "Channel announcement message signature verification failed for node 2: {:?}, message: {:?}, signature: {:?}, pubkey: {:?}",
                        &channel_announcement,
                        &message,
                        &node_2_signature,
                        &channel_announcement.node_2_id
                    )));
                }

                let (tx, block_number, tx_index): (_, u64, _) = match call_t!(
                    self.chain_actor,
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
                                block_number: Some(block_number),
                                ..
                            },
                    }) => (
                        tx,
                        block_number.into(),
                        // tx index is not returned on older ckb version, using dummy tx index instead
                        0u32,
                    ),
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
                let pubkey_hash = blake2b_256(pubkey.as_slice());
                match tx.inner.outputs.get(0) {
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
                        if channel_announcement.udt_type_script.is_some()
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
                    &ckb_signature,
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

                // Add the channel to the network graph.
                let channel_info = ChannelInfo {
                    funding_tx_block_number: block_number.into(),
                    funding_tx_index: tx_index,
                    announcement_msg: channel_announcement.clone(),
                    one_to_two: None, // wait for channel update message
                    two_to_one: None,
                    timestamp: std::time::UNIX_EPOCH.elapsed().unwrap().as_millis() as u64,
                };
                self.network_graph.write().await.add_channel(channel_info);
                Ok(())
            }

            FiberBroadcastMessage::ChannelUpdate(ref channel_update) => {
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
                let mut network_graph = self.network_graph.write().await;
                let channel = network_graph.get_channel(&channel_update.channel_outpoint);
                if let Some(channel) = channel {
                    let pubkey = if channel_update.message_flags & 1 == 0 {
                        channel.node1()
                    } else {
                        channel.node2()
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
                    debug!(
                        "Channel update message signature verified: {:?}",
                        &channel_update
                    );
                    let res = network_graph.process_channel_update(channel_update.clone());
                    if res.is_err() {
                        return Err(Error::InvalidParameter(format!(
                            "Channel update message processing failed: {:?} result: {:?}",
                            &channel_update, res
                        )));
                    }
                } else {
                    return Err(Error::InvalidParameter(format!(
                        "Failed to process channel update because channel not found: {:?}",
                        &channel_update
                    )));
                }
                Ok(())
            }
        }
    }

    async fn on_send_payment(
        &self,
        my_self: ActorRef<NetworkActorMessage>,
        payment_request: SendPaymentCommand,
    ) -> Result<Hash256, Error> {
        let graph = self.network_graph.read().await;
        // initialize the payment session in db and begin the payment process in a statemachine to
        // handle the payment process
        info!("send payment: {:?}", payment_request);
        let payment_session = PaymentSession::new(payment_request.clone(), 3);

        let onion_path = graph.build_route(payment_request)?;
        assert!(!onion_path.is_empty());

        info!("onion_infos: {:?}", onion_path);
        let onion_packet = OnionPacket::new(onion_path).serialize();

        let res = my_self.send_message(NetworkActorMessage::Command(
            NetworkActorCommand::SendOnionPacket(onion_packet.clone(), None),
        ));
        info!("result: {:?}", res);
        Ok(payment_session.payment_hash())
    }
}

struct NetworkSyncState {
    // The block number we are syncing from.
    starting_height: u64,
    // The block number we are syncing up to.
    // This is normally the tip block number when we startup. We will only actively sync
    // channel announcement up to this number (other info will be broadcasted by peers).
    ending_height: u64,
    // The timestamp we started syncing.
    starting_time: u64,
    // All the pinned peers that we are going to sync with.
    pinned_syncing_peers: Vec<(PeerId, Multiaddr)>,
    active_syncers: HashMap<PeerId, ActorRef<GraphSyncerMessage>>,
    // Number of peers with whom we succeeded to sync.
    succeeded: usize,
    // Number of peers with whom we failed to sync.
    failed: usize,
}

impl NetworkSyncState {
    // Note that this function may actually change the state, this is because,
    // when the sync to all peers failed, we actually want to start a new syncer,
    // and we want to track this syncer.
    async fn maybe_create_graph_syncer(
        &mut self,
        peer_id: &PeerId,
        network: ActorRef<NetworkActorMessage>,
    ) -> Option<ActorRef<GraphSyncerMessage>> {
        // There are two possibility for the following condition to be true:
        // 1) we don't have any pinned syncing peers.
        // 2) we have some pinned syncing peers, and all of them failed to sync.
        // In the first case, both self.pinned_syncing_peers.len() is always 0,
        // and self.failed is alway greater or equal 0, so the condition is always true.
        // In the second case, if self.failed is larger than the length of pinned_syncing_peers,
        // then all of pinned sync peers failed to sync. This is because
        // we will always try to sync with all the pinned syncing peers first.
        let should_create = if self.failed >= self.pinned_syncing_peers.len() {
            // TODO: we may want more than one successful syncing.
            if self.succeeded != 0 {
                false
            } else {
                debug!("Adding peer to dynamic syncing peers list: peer {:?}, succeeded syncing {}, failed syncing {}, pinned syncing peers {}", peer_id, self.succeeded, self.failed, self.pinned_syncing_peers.len());
                true
            }
        } else {
            self.pinned_syncing_peers
                .iter()
                .any(|(id, _)| id == peer_id)
        };

        if should_create {
            let graph_syncer = Actor::spawn_linked(
                Some(format!("Graph syncer to {}", peer_id)),
                GraphSyncer::new(
                    network.clone(),
                    peer_id.clone(),
                    self.starting_height,
                    self.ending_height,
                    self.starting_time,
                ),
                (),
                network.get_cell(),
            )
            .await
            .expect("Failed to start graph syncer actor")
            .0;
            self.active_syncers
                .insert(peer_id.clone(), graph_syncer.clone());
            Some(graph_syncer)
        } else {
            None
        }
    }

    fn get_graph_syncer(&self, peer_id: &PeerId) -> Option<&ActorRef<GraphSyncerMessage>> {
        self.active_syncers.get(peer_id)
    }
}

enum NetworkSyncStatus {
    Running(NetworkSyncState),
    Done,
}

impl NetworkSyncStatus {
    fn new(
        starting_height: u64,
        ending_height: u64,
        starting_time: u64,
        syncing_peers: Vec<(PeerId, Multiaddr)>,
    ) -> Self {
        let state = NetworkSyncState {
            starting_height,
            ending_height,
            starting_time,
            pinned_syncing_peers: syncing_peers,
            active_syncers: Default::default(),
            succeeded: 0,
            failed: 0,
        };
        NetworkSyncStatus::Running(state)
    }

    fn is_syncing(&self) -> bool {
        match self {
            NetworkSyncStatus::Running(_) => true,
            NetworkSyncStatus::Done => false,
        }
    }
}

pub struct NetworkActorState {
    // The name of the node to be announced to the network, may be empty.
    node_name: Option<AnnouncedNodeName>,
    peer_id: PeerId,
    announced_addrs: Vec<Multiaddr>,
    // We need to keep private key here in order to sign node announcement messages.
    private_key: Privkey,
    // This is the entropy used to generate various random values.
    // Must be kept secret.
    // TODO: Maybe we should abstract this into a separate trait.
    entropy: [u8; 32],
    network: ActorRef<NetworkActorMessage>,
    // This immutable attribute is placed here because we need to create it in
    // the pre_start function.
    control: ServiceAsyncControl,
    peer_session_map: HashMap<PeerId, SessionId>,
    // This map is used to store the public key of the peer.
    peer_pubkey_map: HashMap<PeerId, Pubkey>,
    session_channels_map: HashMap<SessionId, HashSet<Hash256>>,
    channels: HashMap<Hash256, ActorRef<ChannelActorMessage>>,
    // Outpoint to channel id mapping.
    outpoint_channel_map: HashMap<OutPoint, Hash256>,
    // Channels in this hashmap are pending for acceptance. The user needs to
    // issue an AcceptChannelCommand with the amount of funding to accept the channel.
    to_be_accepted_channels: HashMap<Hash256, (PeerId, OpenChannel)>,
    // Channels in this hashmap are pending for funding transaction confirmation.
    pending_channels: HashMap<OutPoint, Hash256>,
    // Used to broadcast and query network info.
    chain_actor: ActorRef<CkbChainMessage>,
    // If the other party funding more than this amount, we will automatically accept the channel.
    open_channel_auto_accept_min_ckb_funding_amount: u64,
    // Tha default amount of CKB to be funded when auto accepting a channel.
    auto_accept_channel_ckb_funding_amount: u64,
    // The default locktime expiry delta to forward tlcs.
    tlc_locktime_expiry_delta: u64,
    // The default tlc min and max value of tlcs to be accepted.
    tlc_min_value: u128,
    tlc_max_value: u128,
    // The default tlc fee proportional millionths to be used when auto accepting a channel.
    tlc_fee_proportional_millionths: u128,
    // A hashset to store the list of all broadcasted messages.
    // This is used to avoid re-broadcasting the same message over and over again
    // TODO: some more intelligent way to manage broadcasting.
    // 1) The hashset is not a constant size, so we may need to remove old messages.
    // We can use bloom filter to efficiently check if a message is already broadcasted.
    // But there is a possibility of false positives. In that case, we can use different
    // hash functions to make different nodes have different false positives.
    // 2) The broadcast message NodeAnnouncement and ChannelUpdate have a timestamp field.
    // We can use this field to determine if a message is too old to be broadcasted.
    // We didn't check the timestamp field here but all nodes should only save the latest
    // message of the same type.
    broadcasted_messages: HashSet<Hash256>,
    channel_subscribers: ChannelSubscribers,
    // Request id for the next request.
    next_request_id: u64,
    // The response of these broadcast messages will be sent to the corresponding channel.
    broadcast_message_responses: HashMap<(PeerId, u64), RpcReplyPort<Result<(), Error>>>,
    original_requests: HashMap<(PeerId, u64), u64>,
    // This field holds the information about our syncing status.
    sync_status: NetworkSyncStatus,
    // A queue of messages that are received while we are syncing network messages.
    // Need to be processed after the sync is done.
    broadcasted_message_queue: Vec<(PeerId, FiberBroadcastMessage)>,
}

static CHANNEL_ACTOR_NAME_PREFIX: AtomicU64 = AtomicU64::new(0u64);

// ractor requires that the actor name is unique, so we add a prefix to the actor name.
fn generate_channel_actor_name(local_peer_id: &PeerId, remote_peer_id: &PeerId) -> String {
    format!(
        "Channel-{} {} <-> {}",
        CHANNEL_ACTOR_NAME_PREFIX.fetch_add(1, Ordering::AcqRel),
        local_peer_id,
        remote_peer_id
    )
}

impl NetworkActorState {
    pub fn get_node_announcement_message(&self) -> Option<NodeAnnouncement> {
        let alias = self.node_name?;
        let addresses = self.announced_addrs.clone();
        Some(NodeAnnouncement::new(alias, addresses, &self.private_key))
    }

    pub fn should_message_be_broadcasted(&mut self, message: &FiberBroadcastMessage) -> bool {
        self.broadcasted_messages.insert(message.id())
    }

    pub fn get_private_key(&self) -> Privkey {
        self.private_key
    }

    pub fn get_public_key(&self) -> Pubkey {
        self.get_private_key().pubkey()
    }

    pub fn generate_channel_seed(&mut self) -> [u8; 32] {
        let channel_user_id = self.channels.len();
        let seed = channel_user_id
            .to_be_bytes()
            .into_iter()
            .chain(self.entropy.iter().cloned())
            .collect::<Vec<u8>>();
        let result = blake2b_hash_with_salt(&seed, b"FIBER_CHANNEL_SEED");
        self.entropy = blake2b_hash_with_salt(&result, b"FIBER_NETWORK_ENTROPY_UPDATE");
        result
    }

    // Create a channel which will receive the response from the peer.
    // The channel will be closed after the response is received.
    pub fn create_request_id_for_reply_port(
        &mut self,
        peer_id: &PeerId,
        reply_port: RpcReplyPort<Result<(), Error>>,
    ) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.broadcast_message_responses
            .insert((peer_id.clone(), id), reply_port);
        id
    }

    pub fn get_reply_port_for_request(
        &mut self,
        peer_id: &PeerId,
        request_id: u64,
    ) -> Option<RpcReplyPort<Result<(), Error>>> {
        let original_id = self
            .original_requests
            .remove(&(peer_id.clone(), request_id))?;

        self.broadcast_message_responses
            .remove(&(peer_id.clone(), original_id))
    }

    fn derive_new_request_id(&mut self, peer_id: &PeerId, old_id: u64) -> Option<u64> {
        if self
            .broadcast_message_responses
            .contains_key(&(peer_id.clone(), old_id))
        {
            let id = self.next_request_id;
            self.next_request_id += 1;
            self.original_requests.insert((peer_id.clone(), id), old_id);
            Some(id)
        } else {
            None
        }
    }

    pub async fn create_outbound_channel<S: ChannelActorStateStore + Sync + Send + 'static>(
        &mut self,
        open_channel: OpenChannelCommand,
        store: S,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256), ProcessingChannelError> {
        let network = self.network.clone();
        let OpenChannelCommand {
            peer_id,
            funding_amount,
            public,
            funding_udt_type_script,
            commitment_fee_rate,
            funding_fee_rate,
            tlc_locktime_expiry_delta,
            tlc_min_value,
            tlc_max_value,
            tlc_fee_proportional_millionths,
            max_tlc_value_in_flight,
            max_num_of_accept_tlcs,
        } = open_channel;
        let remote_pubkey =
            self.get_peer_pubkey(&peer_id)
                .ok_or(ProcessingChannelError::InvalidParameter(format!(
                    "Peer {:?} pubkey not found",
                    &peer_id
                )))?;
        if let Some(udt_type_script) = funding_udt_type_script.as_ref() {
            if !check_udt_script(udt_type_script) {
                return Err(ProcessingChannelError::InvalidParameter(
                    "Invalid UDT type script".to_string(),
                ));
            }
        }
        // NOTE: here we only check the amount is valid, we will also check more in the `pre_start` from channel creation
        let (_funding_amount, _reserved_ckb_amount) =
            self.get_funding_and_reserved_amount(funding_amount, &funding_udt_type_script)?;
        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            Some(generate_channel_actor_name(&self.peer_id, &peer_id)),
            ChannelActor::new(
                self.get_public_key(),
                remote_pubkey,
                network.clone(),
                store,
                self.channel_subscribers.clone(),
            ),
            ChannelInitializationParameter::OpenChannel(OpenChannelParameter {
                funding_amount,
                seed,
                public_channel_info: public.then_some(PublicChannelInfo::new(
                    tlc_locktime_expiry_delta.unwrap_or(self.tlc_locktime_expiry_delta),
                    tlc_min_value.unwrap_or(self.tlc_min_value),
                    tlc_max_value.unwrap_or(self.tlc_max_value),
                    tlc_fee_proportional_millionths.unwrap_or(self.tlc_fee_proportional_millionths),
                )),
                funding_udt_type_script,
                channel_id_sender: tx,
                commitment_fee_rate,
                funding_fee_rate,
                max_tlc_value_in_flight,
                max_num_of_accept_tlcs,
            }),
            network.clone().get_cell(),
        )
        .await?
        .0;
        let temp_channel_id = rx.await.expect("msg received");
        Ok((channel, temp_channel_id))
    }

    pub async fn create_inbound_channel<S: ChannelActorStateStore + Sync + Send + 'static>(
        &mut self,
        accept_channel: AcceptChannelCommand,
        store: S,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256, Hash256), ProcessingChannelError> {
        let AcceptChannelCommand {
            temp_channel_id,
            funding_amount,
        } = accept_channel;

        let (peer_id, open_channel) = self
            .to_be_accepted_channels
            .remove(&temp_channel_id)
            .ok_or(ProcessingChannelError::InvalidParameter(format!(
                "No channel with temp id {:?} found",
                &temp_channel_id
            )))?;

        let remote_pubkey =
            self.get_peer_pubkey(&peer_id)
                .ok_or(ProcessingChannelError::InvalidParameter(format!(
                    "Peer {:?} pubkey not found",
                    &peer_id
                )))?;

        let (funding_amount, reserved_ckb_amount) = self.get_funding_and_reserved_amount(
            funding_amount,
            &open_channel.funding_udt_type_script,
        )?;

        let network = self.network.clone();
        let id = open_channel.channel_id;
        if let Some(channel) = self.channels.get(&id) {
            warn!("A channel of id {:?} is already created, returning it", &id);
            return Ok((channel.clone(), temp_channel_id, id));
        }

        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            None,
            ChannelActor::new(
                self.get_public_key(),
                remote_pubkey,
                network.clone(),
                store,
                self.channel_subscribers.clone(),
            ),
            ChannelInitializationParameter::AcceptChannel(AcceptChannelParameter {
                funding_amount,
                reserved_ckb_amount,
                public_channel_info: Some(PublicChannelInfo::new(
                    self.tlc_locktime_expiry_delta,
                    self.tlc_min_value,
                    self.tlc_max_value,
                    self.tlc_fee_proportional_millionths,
                )),
                seed,
                open_channel,
                channel_id_sender: Some(tx),
            }),
            network.clone().get_cell(),
        )
        .await?
        .0;
        let new_id = rx.await.expect("msg received");
        Ok((channel, temp_channel_id, new_id))
    }

    async fn broadcast_tx_with_callback<F>(&self, transaction: TransactionView, callback: F)
    where
        F: Send + 'static + FnOnce(Result<TraceTxResponse, RactorErr<CkbChainMessage>>),
    {
        debug!("Trying to broadcast transaction {:?}", &transaction);
        let chain = self.chain_actor.clone();
        call_t!(
            &chain,
            CkbChainMessage::SendTx,
            DEFAULT_CHAIN_ACTOR_TIMEOUT,
            transaction.clone()
        )
        .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
        .expect("valid tx to broadcast");

        let tx_hash = transaction.hash();
        info!("Transactoin sent to the network: {}", tx_hash);

        // TODO: make number of confirmation to transaction configurable.
        const NUM_CONFIRMATIONS: u64 = 4;
        let request = TraceTxRequest {
            tx_hash: tx_hash.clone(),
            confirmations: NUM_CONFIRMATIONS,
        };

        // Spawn a new task to avoid blocking current actor message processing.
        ractor::concurrency::tokio_primatives::spawn(async move {
            debug!("Tracing transaction status {:?}", &request.tx_hash);
            let result = call_t!(
                chain,
                CkbChainMessage::TraceTx,
                DEFAULT_CHAIN_ACTOR_TIMEOUT,
                request.clone()
            );
            callback(result);
        });
    }

    fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        self.peer_session_map.get(peer_id).cloned()
    }

    pub fn get_n_peer_peer_ids(&self, n: usize, excluding: HashSet<PeerId>) -> Vec<PeerId> {
        self.peer_session_map
            .keys()
            .skip_while(|x| excluding.contains(x))
            .take(n)
            .cloned()
            .collect()
    }

    pub fn get_n_peer_sessions(&self, n: usize) -> Vec<SessionId> {
        self.peer_session_map.values().take(n).cloned().collect()
    }

    fn get_peer_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.peer_pubkey_map.get(peer_id).cloned()
    }

    fn get_funding_and_reserved_amount(
        &self,
        funding_amount: u128,
        udt_type_script: &Option<Script>,
    ) -> Result<(u128, u64), ProcessingChannelError> {
        let reserved_ckb_amount = default_minimal_ckb_amount(udt_type_script.is_some());
        if udt_type_script.is_none() && funding_amount < reserved_ckb_amount.into() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "The value of the channel should be greater than the reserve amount: {}",
                reserved_ckb_amount
            )));
        }
        let funding_amount = if udt_type_script.is_some() {
            funding_amount
        } else {
            funding_amount - reserved_ckb_amount as u128
        };
        Ok((funding_amount, reserved_ckb_amount))
    }

    fn check_accept_channel_ckb_parameters(
        &self,
        remote_reserved_ckb_amount: u64,
        local_reserved_ckb_amount: u64,
        funding_fee_rate: u64,
        udt_type_script: &Option<Script>,
    ) -> crate::Result<()> {
        let reserved_ckb_amount = default_minimal_ckb_amount(udt_type_script.is_some());
        if remote_reserved_ckb_amount < reserved_ckb_amount
            || local_reserved_ckb_amount < reserved_ckb_amount
        {
            return Err(Error::InvalidParameter(format!(
                "Reserved CKB amount is less than the minimal amount: {}",
                reserved_ckb_amount
            )));
        }

        if funding_fee_rate < DEFAULT_FEE_RATE {
            return Err(Error::InvalidParameter(format!(
                "Funding fee rate is less than {}",
                DEFAULT_FEE_RATE
            )));
        }
        Ok(())
    }

    fn check_open_ckb_parameters(
        &self,
        open_channel: &OpenChannel,
    ) -> Result<(), ProcessingChannelError> {
        let reserved_ckb_amount = open_channel.reserved_ckb_amount;
        let udt_type_script = &open_channel.funding_udt_type_script;

        let minimal_reserved_ckb_amount = default_minimal_ckb_amount(udt_type_script.is_some());
        if reserved_ckb_amount < minimal_reserved_ckb_amount {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Remote reserved CKB amount {} is less than the minimal amount: {}",
                reserved_ckb_amount, minimal_reserved_ckb_amount,
            )));
        }

        if open_channel.funding_fee_rate < DEFAULT_FEE_RATE {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Funding fee rate is less than {}",
                DEFAULT_FEE_RATE,
            )));
        }

        if open_channel.commitment_fee_rate < DEFAULT_COMMITMENT_FEE_RATE {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee rate is less than {}",
                DEFAULT_COMMITMENT_FEE_RATE,
            )));
        }

        let commitment_fee = calculate_commitment_tx_fee(
            open_channel.commitment_fee_rate,
            &open_channel.funding_udt_type_script,
        );

        let expected_minimal_reserved_ckb_amount = commitment_fee * 2;
        debug!(
            "expected_minimal_reserved_ckb_amount: {}, reserved_ckb_amount: {}",
            expected_minimal_reserved_ckb_amount, reserved_ckb_amount
        );
        if reserved_ckb_amount < expected_minimal_reserved_ckb_amount {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee rate is: {}, expect more CKB amount as reserved ckb amount expected to larger than {}, \
                or you can set a lower commitment fee rate",
                open_channel.commitment_fee_rate, expected_minimal_reserved_ckb_amount
            )));
        }

        Ok(())
    }

    async fn send_message_to_session(
        &self,
        session_id: SessionId,
        message: FiberMessage,
    ) -> crate::Result<()> {
        self.control
            .send_message_to(session_id, FIBER_PROTOCOL_ID, message.to_molecule_bytes())
            .await?;
        Ok(())
    }

    async fn send_message_to_peer(
        &self,
        peer_id: &PeerId,
        message: FiberMessage,
    ) -> crate::Result<()> {
        match self.get_peer_session(peer_id) {
            Some(session) => self.send_message_to_session(session, message).await,
            None => Err(Error::PeerNotFound(peer_id.clone())),
        }
    }

    async fn send_command_to_channel(
        &self,
        channel_id: Hash256,
        command: ChannelCommand,
    ) -> crate::Result<()> {
        match self.channels.get(&channel_id) {
            Some(actor) => {
                actor.send_message(ChannelActorMessage::Command(command))?;
                Ok(())
            }
            None => Err(Error::ChannelNotFound(channel_id)),
        }
    }

    async fn on_peer_connected<
        S: ChannelActorStateStore + NetworkGraphStateStore + Clone + Send + Sync + 'static,
    >(
        &mut self,
        remote_peer_id: &PeerId,
        remote_pubkey: Pubkey,
        session: &SessionContext,
        store: S,
    ) {
        self.peer_session_map
            .insert(remote_peer_id.clone(), session.id);
        self.peer_pubkey_map
            .insert(remote_peer_id.clone(), remote_pubkey);

        if let Some(message) = self.get_node_announcement_message() {
            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::BroadcastMessage(
                        vec![remote_peer_id.clone()],
                        FiberBroadcastMessage::NodeAnnouncement(message),
                    ),
                ))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        }

        for channel_id in store.get_active_channel_ids_by_peer(&remote_peer_id) {
            debug!("Reestablishing channel {:x}", &channel_id);
            if let Ok((_channel, _)) = Actor::spawn_linked(
                None,
                ChannelActor::new(
                    self.get_public_key(),
                    remote_pubkey,
                    self.network.clone(),
                    store.clone(),
                    self.channel_subscribers.clone(),
                ),
                ChannelInitializationParameter::ReestablishChannel(channel_id),
                self.network.get_cell(),
            )
            .await
            {
                // network actor will receive ChannelCreated event after channel is created
                // and will add the channel to the session_channels_map, so we don't need to do it here
                info!("channel {:x} reestablished successfully", &channel_id);
            } else {
                error!("Failed to reestablish channel {:x}", &channel_id);
            }
        }
        self.maybe_sync_network_graph(remote_peer_id).await;
    }

    fn on_peer_disconnected(&mut self, id: &PeerId) {
        if let Some(session) = self.peer_session_map.remove(id) {
            if let Some(channel_ids) = self.session_channels_map.remove(&session) {
                for channel_id in channel_ids {
                    if let Some(channel) = self.channels.remove(&channel_id) {
                        let _ = channel.send_message(ChannelActorMessage::Event(
                            ChannelEvent::PeerDisconnected,
                        ));
                    }
                }
            }
        }
        self.maybe_tell_syncer_peer_disconnected(id);
    }

    async fn maybe_sync_network_graph(&mut self, peer_id: &PeerId) {
        if let NetworkSyncStatus::Running(state) = &mut self.sync_status {
            if let Some(_) = state
                .maybe_create_graph_syncer(peer_id, self.network.clone())
                .await
            {
                debug!("Created graph syncer to peer {:?}", peer_id);
            }
        }
    }

    fn maybe_tell_syncer_peer_disconnected(&self, peer_id: &PeerId) {
        if let NetworkSyncStatus::Running(ref state) = self.sync_status {
            if let Some(syncer) = state.get_graph_syncer(peer_id) {
                let _ = syncer.send_message(GraphSyncerMessage::PeerDisConnected);
            }
        }
    }

    fn maybe_tell_syncer_peer_disconnected_multiaddr(&self, multiaddr: &Multiaddr) {
        if let NetworkSyncStatus::Running(ref state) = self.sync_status {
            if let Some(peer_id) = state
                .pinned_syncing_peers
                .iter()
                .find(|(_p, a)| a == multiaddr)
                .map(|x| &x.0)
            {
                self.maybe_tell_syncer_peer_disconnected(peer_id);
            }
        }
    }

    fn maybe_finish_sync(&mut self) {
        if let NetworkSyncStatus::Running(state) = &self.sync_status {
            // TODO: It is better to sync with a few more peers to make sure we have the latest data.
            // But we may only be connected just one node.
            if state.succeeded >= 1 {
                debug!(
                    "All peers finished syncing, starting time {:?}, finishing time {:?}",
                    state.starting_time,
                    SystemTime::now()
                );
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::MarkSyncingDone,
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }
    }

    fn on_channel_created(
        &mut self,
        id: Hash256,
        peer_id: &PeerId,
        actor: ActorRef<ChannelActorMessage>,
    ) {
        if let Some(session) = self.get_peer_session(peer_id) {
            self.channels.insert(id, actor);
            self.session_channels_map
                .entry(session)
                .or_default()
                .insert(id);
        }
    }

    async fn on_closing_transaction_pending(
        &mut self,
        channel_id: Hash256,
        peer_id: PeerId,
        transaction: TransactionView,
    ) {
        let tx_hash: Byte32 = transaction.hash();
        info!(
            "Channel ({:?}) to peer {:?} is closed. Broadcasting closing transaction ({:?}) now.",
            &channel_id, &peer_id, &tx_hash
        );
        let network: ActorRef<NetworkActorMessage> = self.network.clone();
        self.broadcast_tx_with_callback(transaction, move |result| {
            let message = match result {
                Ok(TraceTxResponse {
                    status:
                        TxStatus {
                            status: Status::Committed,
                            ..
                        },
                    ..
                }) => {
                    info!("Cloisng transaction {:?} confirmed", &tx_hash);
                    NetworkActorEvent::ClosingTransactionConfirmed(peer_id, channel_id, tx_hash)
                }
                Ok(status) => {
                    error!(
                        "Closing transaction {:?} failed to be confirmed with final status {:?}",
                        &tx_hash, &status
                    );
                    NetworkActorEvent::ClosingTransactionFailed(peer_id, channel_id, tx_hash)
                }
                Err(err) => {
                    error!("Failed to trace transaction {:?}: {:?}", &tx_hash, &err);
                    NetworkActorEvent::ClosingTransactionFailed(peer_id, channel_id, tx_hash)
                }
            };
            network
                .send_message(NetworkActorMessage::new_event(message))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        })
        .await;
    }

    async fn on_closing_transaction_confirmed(
        &mut self,
        peer_id: &PeerId,
        channel_id: &Hash256,
        tx_hash: Byte32,
    ) {
        self.channels.remove(&channel_id);
        if let Some(session) = self.get_peer_session(&peer_id) {
            if let Some(set) = self.session_channels_map.get_mut(&session) {
                set.remove(&channel_id);
            }
        }
        self.send_message_to_channel_actor(
            *channel_id,
            ChannelActorMessage::Event(ChannelEvent::ClosingTransactionConfirmed),
        );
        // Notify outside observers.
        self.network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(NetworkServiceEvent::ChannelClosed(
                    peer_id.clone(),
                    *channel_id,
                    tx_hash,
                )),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
    }

    pub async fn on_open_channel_msg(
        &mut self,
        peer_id: PeerId,
        open_channel: OpenChannel,
    ) -> ProcessingChannelResult {
        self.check_open_ckb_parameters(&open_channel)?;

        if let Some(udt_type_script) = &open_channel.funding_udt_type_script {
            if !check_udt_script(udt_type_script) {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Invalid UDT type script: {:?}",
                    udt_type_script
                )));
            }
        }

        let id = open_channel.channel_id;
        if let Some(channel) = self.to_be_accepted_channels.get(&id) {
            warn!(
                "A channel from {:?} of id {:?} is already awaiting to be accepted: {:?}",
                &peer_id, &id, channel
            );
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "A channel from {:?} of id {:?} is already awaiting to be accepted",
                &peer_id, &id,
            )));
        }
        debug!(
            "Channel from {:?} of id {:?} is now awaiting to be accepted: {:?}",
            &peer_id, &id, &open_channel
        );
        self.to_be_accepted_channels
            .insert(id, (peer_id.clone(), open_channel));
        // Notify outside observers.
        self.network
            .clone()
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(
                    NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, id),
                ),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        Ok(())
    }

    async fn on_funding_transaction_pending(
        &mut self,
        transaction: Transaction,
        outpoint: OutPoint,
        channel_id: Hash256,
    ) {
        // Just a sanity check to ensure that no two channels are associated with the same outpoint.
        if let Some(old) = self.pending_channels.remove(&outpoint) {
            if old != channel_id {
                panic!("Trying to associate a new channel id {:?} with the same outpoint {:?} when old channel id is {:?}. Rejecting.", channel_id, outpoint, old);
            }
        }
        self.pending_channels.insert(outpoint.clone(), channel_id);
        // TODO: try to broadcast the transaction to the network.
        let transaction = transaction.into_view();
        let tx_hash: Byte32 = transaction.hash();
        debug!(
            "Funding transaction (outpoint {:?}) for channel {:?} is now ready. Broadcast it {:?} now.",
            &outpoint, &channel_id, &tx_hash
        );
        let network = self.network.clone();
        self.broadcast_tx_with_callback(transaction, move |result| {
            debug!("Funding transaction broadcast result: {:?}", &result);
            let message = match result {
                Ok(TraceTxResponse {
                    status:
                        TxStatus {
                            status: Status::Committed,
                            ..
                        },
                    ..
                }) => {
                    info!("Funding transaction {:?} confirmed", &tx_hash);
                    NetworkActorEvent::FundingTransactionConfirmed(outpoint)
                }
                Ok(status) => {
                    error!(
                        "Funding transaction {:?} failed to be confirmed with final status {:?}",
                        &tx_hash, &status
                    );
                    NetworkActorEvent::FundingTransactionFailed(outpoint)
                }
                Err(err) => {
                    error!("Failed to trace transaction {:?}: {:?}", &tx_hash, &err);
                    NetworkActorEvent::FundingTransactionFailed(outpoint)
                }
            };

            // Notify outside observers.
            network
                .send_message(NetworkActorMessage::new_event(message))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        })
        .await;
    }

    async fn on_commitment_transaction_pending(
        &mut self,
        transaction: Transaction,
        channel_id: Hash256,
    ) {
        let transaction = transaction.into_view();
        let tx_hash: Byte32 = transaction.hash();
        debug!(
            "Commitment transaction for channel {:?} is now ready. Broadcast it {:?} now.",
            &channel_id, &tx_hash
        );

        let network = self.network.clone();
        self.broadcast_tx_with_callback(transaction, move |result| {
            let message = match result {
                Ok(TraceTxResponse {
                    status:
                        TxStatus {
                            status: Status::Committed,
                            ..
                        },
                    ..
                }) => {
                    info!("Commitment transaction {:?} confirmed", tx_hash,);
                    NetworkActorEvent::CommitmentTransactionConfirmed(tx_hash.into(), channel_id)
                }
                Ok(status) => {
                    error!(
                        "Commitment transaction {:?} failed to be confirmed with final status {:?}",
                        &tx_hash, &status
                    );
                    NetworkActorEvent::CommitmentTransactionFailed(channel_id, tx_hash)
                }
                Err(err) => {
                    error!(
                        "Failed to trace commitment transaction {:?}: {:?}",
                        &tx_hash, &err
                    );
                    NetworkActorEvent::CommitmentTransactionFailed(channel_id, tx_hash)
                }
            };
            network
                .send_message(NetworkActorMessage::new_event(message))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        })
        .await;
    }

    async fn on_funding_transaction_confirmed(&mut self, outpoint: OutPoint) {
        debug!("Funding transaction is confirmed: {:?}", &outpoint);
        let channel_id = match self.pending_channels.remove(&outpoint) {
            Some(channel_id) => channel_id,
            None => {
                warn!(
                    "Funding transaction confirmed for outpoint {:?} but no channel found",
                    &outpoint
                );
                return;
            }
        };
        self.send_message_to_channel_actor(
            channel_id,
            ChannelActorMessage::Event(ChannelEvent::FundingTransactionConfirmed),
        );
    }

    async fn on_commitment_transaction_confirmed(&mut self, tx_hash: Hash256, channel_id: Hash256) {
        debug!("Commitment transaction is confirmed: {:?}", tx_hash);
        self.send_message_to_channel_actor(
            channel_id,
            ChannelActorMessage::Event(ChannelEvent::CommitmentTransactionConfirmed),
        );
    }

    fn send_message_to_channel_actor(&self, channel_id: Hash256, message: ChannelActorMessage) {
        match self.channels.get(&channel_id) {
            None => {
                error!(
                    "Failed to send message to channel actor: channel {:?} not found",
                    &channel_id
                );
            }
            Some(actor) => {
                actor.send_message(message).expect("channel actor alive");
            }
        }
    }
}

pub struct NetworkActorStartArguments {
    pub config: FiberConfig,
    pub tracker: TaskTracker,
    pub channel_subscribers: ChannelSubscribers,
}

#[rasync_trait]
impl<S> Actor for NetworkActor<S>
where
    S: ChannelActorStateStore + NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    type Msg = NetworkActorMessage;
    type State = NetworkActorState;
    type Arguments = NetworkActorStartArguments;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let NetworkActorStartArguments {
            config,
            tracker,
            channel_subscribers,
        } = args;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("SystemTime::now() should after UNIX_EPOCH");
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let private_key = <[u8; 32]>::try_from(kp.as_ref())
            .expect("valid length for key")
            .try_into()
            .expect("valid secret key");
        let entropy = blake2b_hash_with_salt(
            [kp.as_ref(), now.as_nanos().to_le_bytes().as_ref()]
                .concat()
                .as_slice(),
            b"FIBER_NETWORK_ENTROPY",
        );
        let secio_kp = SecioKeyPair::from(kp);
        let secio_pk = secio_kp.public_key();
        let handle = Handle::new(myself.clone());
        let mut service = ServiceBuilder::default()
            .insert_protocol(handle.clone().create_meta(FIBER_PROTOCOL_ID))
            .handshake_type(secio_kp.into())
            .build(handle);
        let mut listening_addr = service
            .listen(
                MultiAddr::from_str(config.listening_addr())
                    .expect("valid tentacle listening address"),
            )
            .await
            .expect("listen tentacle");

        trace!("debug secio_pk: {:?}", secio_pk);
        let my_peer_id: PeerId = PeerId::from(secio_pk);
        listening_addr.push(Protocol::P2P(Cow::Owned(my_peer_id.clone().into_bytes())));
        let mut announced_addrs = Vec::with_capacity(config.announced_addrs.len() + 1);
        if config.announce_listening_addr() {
            announced_addrs.push(listening_addr.clone());
        }
        for listen_addr in &config.announced_addrs {
            let mut multiaddr =
                MultiAddr::from_str(listen_addr.as_str()).expect("valid announced listen addr");
            multiaddr.push(Protocol::P2P(Cow::Owned(my_peer_id.clone().into_bytes())));
            announced_addrs.push(multiaddr);
        }
        info!(
            "Started listening tentacle on {:?}, peer id {:?}, announced addresses {:?}",
            &listening_addr, &my_peer_id, &announced_addrs
        );

        let control = service.control().to_owned();

        myself
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(NetworkServiceEvent::NetworkStarted(
                    my_peer_id.clone(),
                    listening_addr.clone(),
                    announced_addrs.clone(),
                )),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);

        tracker.spawn(async move {
            service.run().await;
            debug!("Tentacle service shutdown");
        });

        let graph = self.network_graph.read().await;
        let peers_to_sync_network_graph = graph
            .get_peers_to_sync_network_graph()
            .into_iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        let height = graph.get_best_height();
        let last_update = graph.get_last_update_timestamp();
        debug!(
            "Trying to sync network graph with peers {:?} with height {} and last update {:?}",
            &peers_to_sync_network_graph, &height, &last_update
        );

        let chain_actor = self.chain_actor.clone();
        let current_block_number = call!(chain_actor, CkbChainMessage::GetCurrentBlockNumber, ())
            .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
            .expect("Get current block number from chain");
        let state = NetworkActorState {
            node_name: config.announced_node_name,
            peer_id: my_peer_id,
            announced_addrs,
            private_key,
            entropy,
            network: myself.clone(),
            control,
            peer_session_map: Default::default(),
            peer_pubkey_map: Default::default(),
            session_channels_map: Default::default(),
            channels: Default::default(),
            outpoint_channel_map: Default::default(),
            to_be_accepted_channels: Default::default(),
            pending_channels: Default::default(),
            chain_actor,
            open_channel_auto_accept_min_ckb_funding_amount: config
                .open_channel_auto_accept_min_ckb_funding_amount(),
            auto_accept_channel_ckb_funding_amount: config.auto_accept_channel_ckb_funding_amount(),
            tlc_locktime_expiry_delta: config.tlc_locktime_expiry_delta(),
            tlc_min_value: config.tlc_min_value(),
            tlc_max_value: config.tlc_max_value(),
            tlc_fee_proportional_millionths: config.tlc_fee_proportional_millionths(),
            broadcasted_messages: Default::default(),
            channel_subscribers,
            next_request_id: Default::default(),
            broadcast_message_responses: Default::default(),
            original_requests: Default::default(),
            sync_status: NetworkSyncStatus::new(
                height,
                current_block_number,
                last_update,
                peers_to_sync_network_graph,
            ),
            broadcasted_message_queue: Default::default(),
        };

        // load the connected peers from the network graph
        let peers = graph.get_connected_peers();
        // TODO: we need to bootstrap the network if no peers are connected.
        if peers.is_empty() {
            warn!("No connected peers found in the network graph");
        }
        for (_peer_id, addr) in peers {
            myself.send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ConnectPeer(addr.clone()),
            ))?;
        }

        if config.auto_announce_node() {
            // We have no easy way to know when the connections to peers are established
            // in tentacle, so we just wait for a while.
            myself.send_after(Duration::from_secs(1), || {
                NetworkActorMessage::new_command(NetworkActorCommand::BroadcastLocalInfo(
                    LocalInfoKind::NodeAnnouncement,
                ))
            });
        }

        let announce_node_interval_seconds = config.announce_node_interval_seconds();
        if announce_node_interval_seconds > 0 {
            myself.send_interval(Duration::from_secs(announce_node_interval_seconds), || {
                NetworkActorMessage::new_command(NetworkActorCommand::BroadcastLocalInfo(
                    LocalInfoKind::NodeAnnouncement,
                ))
            });
        }

        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            NetworkActorMessage::Event(event) => {
                if let Err(err) = self.handle_event(myself, state, event).await {
                    error!("Failed to handle ckb network event: {}", err);
                }
            }
            NetworkActorMessage::Command(command) => {
                if let Err(err) = self.handle_command(myself, state, command).await {
                    error!("Failed to handle ckb network command: {}", err);
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let Err(err) = state.control.close().await {
            error!("Failed to close tentacle service: {}", err);
        }
        debug!("Network service for {:?} shutdown", state.peer_id);
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorTerminated(who, _, _) => {
                debug!("Actor {:?} terminated", who);
            }
            SupervisionEvent::ActorPanicked(who, _) => {
                error!("Actor {:?} panicked", who);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Handle {
    actor: ActorRef<NetworkActorMessage>,
}

impl Handle {
    pub fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        Self { actor }
    }

    fn send_actor_message(&self, message: NetworkActorMessage) {
        // If we are closing the whole network service, we may have already stopped the network actor.
        // In that case the send_message will fail.
        // Ideally, we should close tentacle network service first, then stop the network actor.
        // But ractor provides only api for `post_stop` instead of `pre_stop`.
        let _ = self.actor.send_message(message);
    }

    fn emit_event(&self, event: NetworkServiceEvent) {
        self.send_actor_message(NetworkActorMessage::Event(
            NetworkActorEvent::NetworkServiceEvent(event),
        ));
    }

    fn create_meta(self, id: ProtocolId) -> ProtocolMeta {
        MetaBuilder::new()
            .id(id)
            .service_handle(move || {
                let handle = Box::new(self);
                ProtocolHandle::Callback(handle)
            })
            .build()
    }
}

#[async_trait]
impl ServiceProtocol for Handle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        let session = context.session;
        info!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id, session.id, session.address, session.ty, version
        );

        if let Some(remote_pubkey) = context.session.remote_pubkey.clone() {
            let remote_peer_id = PeerId::from_public_key(&remote_pubkey);
            self.send_actor_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PeerConnected(
                    remote_peer_id,
                    remote_pubkey.into(),
                    context.session.clone(),
                ),
            ));
        } else {
            warn!("Peer connected without remote pubkey {:?}", context.session);
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );

        match context.session.remote_pubkey.as_ref() {
            Some(pubkey) => {
                let peer_id = PeerId::from_public_key(pubkey);
                self.send_actor_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::PeerDisconnected(peer_id, context.session.clone()),
                ));
            }
            None => {
                unreachable!("Received message without remote pubkey");
            }
        }
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        let msg = unwrap_or_return!(FiberMessage::from_molecule_slice(&data), "parse message");
        match context.session.remote_pubkey.as_ref() {
            Some(pubkey) => {
                let peer_id = PeerId::from_public_key(pubkey);
                self.send_actor_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::PeerMessage(peer_id, msg),
                ));
            }
            None => {
                unreachable!("Received message without remote pubkey");
            }
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[async_trait]
impl ServiceHandle for Handle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        self.emit_event(NetworkServiceEvent::ServiceError(error));
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        self.emit_event(NetworkServiceEvent::ServiceEvent(event));
    }
}

pub(crate) fn emit_service_event(
    network: &ActorRef<NetworkActorMessage>,
    event: NetworkServiceEvent,
) {
    network
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::NetworkServiceEvent(event),
        ))
        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
}

pub async fn start_network<
    S: ChannelActorStateStore + NetworkGraphStateStore + Clone + Send + Sync + 'static,
>(
    config: FiberConfig,
    chain_actor: ActorRef<CkbChainMessage>,
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    tracker: TaskTracker,
    root_actor: ActorCell,
    store: S,
    channel_subscribers: ChannelSubscribers,
) -> ActorRef<NetworkActorMessage> {
    let secio_kp: SecioKeyPair = config
        .read_or_generate_secret_key()
        .expect("read or generate secret key")
        .into();
    let my_pubkey = secio_kp.public_key();
    let my_peer_id = PeerId::from_public_key(&my_pubkey);

    let (actor, _handle) = Actor::spawn_linked(
        Some(format!("Network {}", my_peer_id)),
        NetworkActor::new(event_sender, chain_actor, store, my_pubkey.into()),
        NetworkActorStartArguments {
            config,
            tracker,
            channel_subscribers,
        },
        root_actor,
    )
    .await
    .expect("Failed to start network actor");

    actor
}
