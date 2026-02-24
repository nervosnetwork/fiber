use ckb_hash::blake2b_256;
use ckb_types::core::tx_pool::TxStatus;
use ckb_types::core::{EpochNumberWithFraction, TransactionView};
use ckb_types::packed::{Byte32, OutPoint, Script, Transaction};
use ckb_types::prelude::{Builder, Entity, IntoTransactionView, Pack, Unpack};
use ckb_types::H256;
use either::Either;
use getrandom::getrandom;
use once_cell::sync::OnceCell;
use ractor::concurrency::Duration;
use ractor::{
    call_t, Actor, ActorCell, ActorProcessingErr, ActorRef, RpcReplyPort, SupervisionEvent,
};
use rand::seq::{IteratorRandom, SliceRandom};
use secp256k1::SECP256K1;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use strum::AsRefStr;
use tentacle::multiaddr::{MultiAddr, Protocol};
use tentacle::service::SessionType;
#[cfg(not(target_arch = "wasm32"))]
use tentacle::utils::TransportType;
use tentacle::utils::{extract_peer_id, is_reachable, multiaddr_to_socketaddr};
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
use tokio_util::codec::length_delimited;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, trace, warn};

use super::channel::{
    get_funding_and_reserved_amount, AcceptChannelParameter, ChannelActor, ChannelActorMessage,
    ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, ChannelEvent,
    ChannelInitializationParameter, ChannelOpenRecord, ChannelOpenRecordStore,
    ChannelOpeningStatus, ChannelState, ChannelTlcInfo, CloseFlags, OpenChannelParameter,
    PrevTlcInfo, ProcessingChannelError, ProcessingChannelResult, PublicChannelInfo,
    RemoveTlcCommand, RevocationData, SettlementData, StopReason, TLCId,
    DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
};
use super::config::AnnouncedNodeName;
use crate::ckb::client::CkbChainClient;
use ckb_sdk::rpc::ckb_indexer::{Order, ScriptType, SearchKey, SearchMode};

use super::features::FeatureVector;
use super::gossip::{GossipActorMessage, GossipMessageStore, GossipMessageUpdates};
use super::graph::{NetworkGraph, NetworkGraphStateStore, OwnedChannelUpdateEvent, RouterHop};
use super::key::blake2b_hash_with_salt;
use super::types::{
    BroadcastMessageWithTimestamp, EcdsaSignature, FiberMessage, ForwardTlcResult, GossipMessage,
    Hash256, Init, NodeAnnouncement, OpenChannel, Privkey, Pubkey, RemoveTlcFulfill,
    RemoveTlcReason, TlcErr, TlcErrorCode,
};
use super::{
    FiberConfig, InFlightCkbTxActor, InFlightCkbTxActorArguments, InFlightCkbTxKind,
    ASSUME_NETWORK_ACTOR_ALIVE,
};
use crate::ckb::config::UdtCfgInfos;
use crate::ckb::contracts::{
    check_udt_script, get_udt_info, get_udt_whitelist, is_udt_type_auto_accept,
};
use crate::ckb::{CkbChainMessage, FundingError, FundingRequest, FundingTx, GetShutdownTxResponse};
use crate::fiber::channel::{
    tlc_expiry_delay, AddTlcCommand, AddTlcResponse, ChannelActorState, ChannelEphemeralConfig,
    ChannelInitializationOperation, RetryableTlcOperation, ShutdownCommand, TxCollaborationCommand,
    TxUpdateCommand,
};
use crate::fiber::channel::{
    AwaitingTxSignaturesFlags, ShuttingDownFlags, MAX_TLC_NUMBER_IN_FLIGHT,
};
use crate::fiber::config::{DEFAULT_COMMITMENT_DELAY_EPOCHS, MIN_TLC_EXPIRY_DELTA};
use crate::fiber::fee::{check_open_channel_parameters, check_tlc_delta_with_epochs};
use crate::fiber::gossip::{GossipConfig, GossipService, SubscribableGossipMessageStore};
#[cfg(any(debug_assertions, test, feature = "bench"))]
use crate::fiber::payment::SessionRoute;
use crate::fiber::payment::{
    PaymentActor, PaymentActorArguments, PaymentActorMessage, PaymentCustomRecords, PaymentStatus,
    SendPaymentCommand, SendPaymentDataBuilder, SendPaymentWithRouterCommand, TrampolineContext,
};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::types::{
    FiberChannelMessage, PeeledPaymentOnionPacket, TlcErrPacket, TrampolineHopPayload,
    TrampolineOnionPacket, TxAbort, TxSignatures,
};
use crate::fiber::SettleTlcSetCommand;
use crate::invoice::{
    CkbInvoice, CkbInvoiceStatus, InvoiceError, InvoiceStore, PreimageStore, SettleInvoiceError,
};
use crate::utils::{actor::ActorHandleLogGuard, payment::is_invoice_fulfilled};
use crate::{now_timestamp_as_millis_u64, unwrap_or_return, Error};

pub const FIBER_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

pub const GOSSIP_PROTOCOL_ID: ProtocolId = ProtocolId::new(43);

pub const DEFAULT_CHAIN_ACTOR_TIMEOUT: u64 = 300000;

// TODO: make it configurable
pub const CKB_TX_TRACING_CONFIRMATIONS: u64 = 4;

pub const DEFAULT_PAYMENT_TRY_LIMIT: u32 = 5;
pub const DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT: u32 = 3;

const ACTOR_HANDLE_WARN_THRESHOLD_MS: u64 = 15_000;

// (128 + 2) KB, 2 KB for custom records
pub const MAX_SERVICE_PROTOCOAL_DATA_SIZE: usize = 1024 * (128 + 2);
pub const MAX_CUSTOM_RECORDS_SIZE: usize = 2 * 1024; // 2 KB

// This is a temporary way to document that we assume the chain actor is always alive.
// We may later relax this assumption. At the moment, if the chain actor fails, we
// should panic with this message, and later we may find all references to this message
// to make sure that we handle the case where the chain actor is not alive.
const ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW: &str =
    "We currently assume that chain actor is always alive, but it failed. This is a known issue.";

pub(crate) const ASSUME_NETWORK_MYSELF_ALIVE: &str = "network actor myself alive";

const ASSUME_GOSSIP_ACTOR_ALIVE: &str = "gossip actor must be alive";

// The duration for which we will try to maintain the number of peers in connection.
const MAINTAINING_CONNECTIONS_INTERVAL: Duration = Duration::from_secs(1200);

// The duration for which we will check all channels.
#[cfg(debug_assertions)]
// use a short interval for debugging build
const CHECK_CHANNELS_INTERVAL: Duration = Duration::from_secs(3);
#[cfg(not(debug_assertions))]
const CHECK_CHANNELS_INTERVAL: Duration = Duration::from_secs(60);

const CHECK_CHANNELS_SHUTDOWN_INTERVAL: Duration = Duration::from_secs(300);

// The duration for which we will check peer init messages.
const CHECK_PEER_INIT_INTERVAL: Duration = Duration::from_secs(20);

// While creating a network graph from the gossip messages, we will load current gossip messages
// in the store and process them. We will load all current messages and get the latest cursor.
// The problem is that we can't guarantee that the messages are in order, that is to say it is
// possible that messages with smaller cursor may arrive at the store from the time we create
// the graph. So we have to subscribe to gossip messages with a cursor slightly smaller than
// current latest cursor. This parameter is the difference between the cursor we use to subscribe
// and the latest cursor.
const MAX_GRAPH_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT: Duration =
    Duration::from_secs(60 * 60 * 2);

static CHAIN_HASH_INSTANCE: OnceCell<Hash256> = OnceCell::new();

pub fn init_chain_hash(chain_hash: Hash256) {
    CHAIN_HASH_INSTANCE
        .set(chain_hash)
        .expect("init_chain_hash should only be called once");
}

pub fn get_chain_hash() -> Hash256 {
    CHAIN_HASH_INSTANCE.get().cloned().unwrap_or_default()
}

pub(crate) fn check_chain_hash(chain_hash: &Hash256) -> Result<(), Error> {
    if chain_hash == &get_chain_hash() {
        Ok(())
    } else {
        Err(Error::InvalidChainHash(*chain_hash, get_chain_hash()))
    }
}

#[derive(Debug)]
pub enum PeerDisconnectReason {
    /// User request disconnection.
    Requested,
    /// Init message timeout.
    InitMessageTimeout,
    /// Chain hash mismatch.
    ChainHashMismatch,
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

/// A channel that has been received from a remote peer but not yet accepted locally.
/// These are held in `to_be_accepted_channels` waiting for a manual `accept_channel` call.
#[derive(Debug, Clone)]
pub struct PendingAcceptChannel {
    /// The temporary channel ID assigned by the initiator.
    pub channel_id: Hash256,
    /// The peer ID of the channel initiator.
    pub peer_id: PeerId,
    /// The amount of CKB or UDT the initiator is contributing to the channel.
    pub funding_amount: u128,
    /// UDT type script, if this is a UDT channel.
    pub udt_type_script: Option<Script>,
    /// Timestamp (milliseconds since UNIX epoch) when this channel request was received.
    pub created_at: u64,
}

#[derive(Debug)]
pub struct SendPaymentResponse {
    pub payment_hash: Hash256,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub failed_error: Option<String>,
    pub custom_records: Option<PaymentCustomRecords>,
    pub fee: u128,
    #[cfg(any(debug_assertions, test, feature = "bench"))]
    pub routers: Vec<SessionRoute>,
}

/// What kind of local information should be broadcasted to the network.
#[derive(Debug)]
pub enum LocalInfoKind {
    NodeAnnouncement,
}

#[derive(Debug, Clone)]
pub struct NodeInfoResponse {
    pub node_name: Option<AnnouncedNodeName>,
    pub node_id: Pubkey,
    pub addresses: Vec<MultiAddr>,
    pub features: FeatureVector,
    pub chain_hash: Hash256,
    pub open_channel_auto_accept_min_ckb_funding_amount: u64,
    pub auto_accept_channel_ckb_funding_amount: u64,
    pub tlc_expiry_delta: u64,
    pub tlc_min_value: u128,
    pub tlc_fee_proportional_millionths: u128,
    pub channel_count: u32,
    pub pending_channel_count: u32,
    pub peers_count: u32,
    pub udt_cfg_infos: UdtCfgInfos,
}

/// The information about a peer connected to the node.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PeerInfo {
    /// The identity public key of the peer.
    pub pubkey: Pubkey,

    /// The peer ID of the peer
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,

    /// The multi-address associated with the connecting peer.
    /// Note: this is only the address which used for connecting to the peer, not all addresses of the peer.
    /// The `graph_nodes` in Graph rpc module will return all addresses of the peer.
    pub address: MultiAddr,
}

#[derive(Debug, Clone)]
pub struct SendOnionPacketCommand {
    pub peeled_onion_packet: PeeledPaymentOnionPacket,
    // We are currently forwarding a previous tlc. The previous tlc's channel id, tlc id
    // and the fee paid are included here.
    pub previous_tlc: Option<PrevTlcInfo>,
    pub payment_hash: Hash256,
    pub attempt_id: Option<u64>,
}

/// The struct here is used both internally and as an API to the outside world.
/// If we want to send a reply to the caller, we need to wrap the message with
/// a RpcReplyPort. Since outsider users have no knowledge of RpcReplyPort, we
/// need to hide it from the API. So in case a reply is needed, we need to put
/// an optional RpcReplyPort in the of the definition of this message.
#[derive(Debug, AsRefStr)]
pub enum NetworkActorCommand {
    /// Network commands
    // Connect to a peer, and optionally also save the peer to the peer store.
    ConnectPeer(Multiaddr),
    DisconnectPeer(PeerId, PeerDisconnectReason),
    // Save the address of a peer to the peer store, the address here must be a valid
    // multiaddr with the peer id.
    SavePeerAddress(Multiaddr),
    // We need to maintain a certain number of peers connections to keep the network running.
    MaintainConnections,
    // Check all channels and see if we need to force close any of them or settle down tlc with preimage.
    CheckChannels,
    // Timeout a hold tlc
    TimeoutHoldTlc(Hash256, Hash256, u64),
    // Settle tlc set by given a list of `(channel_id, tlc_id)`
    SettleTlcSet(Hash256, Vec<(Hash256, u64)>),
    // Settle hold tlc set saved for a payment hash
    SettleHoldTlcSet(Hash256),
    // Check peer send us Init message in an expected time, otherwise disconnect with the peer.
    CheckPeerInit(PeerId, SessionId),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendFiberMessage(FiberMessageWithPeerId),
    // Open a channel to a peer.
    OpenChannel(
        OpenChannelCommand,
        RpcReplyPort<Result<OpenChannelResponse, String>>,
    ),
    // Abandon a channel, channel_id maybe temp_channel_id or normal channel_id
    AbandonChannel(Hash256, RpcReplyPort<Result<(), String>>),
    // Accept a channel to a peer.
    AcceptChannel(
        AcceptChannelCommand,
        RpcReplyPort<Result<AcceptChannelResponse, String>>,
    ),
    // Send a command to a channel.
    ControlFiberChannel(ChannelCommandWithId),
    // Send an onion packet to the next hop. The `PeeledPaymentOnionPacket::current` contains
    // the hop data for the current node.
    SendPaymentOnionPacket(SendOnionPacketCommand, RpcReplyPort<Result<(), TlcErr>>),
    UpdateChannelFunding(Hash256, Transaction, FundingRequest),
    VerifyFundingTx {
        local_tx: Transaction,
        remote_tx: Transaction,
        funding_cell_lock_script: Script,
        reply: RpcReplyPort<Result<(), FundingError>>,
    },
    SignFundingTx(PeerId, Hash256, Transaction, Option<Vec<Vec<u8>>>),
    NotifyFundingTx(Transaction),
    CheckChannelsShutdown,
    CheckChannelShutdown(Hash256),
    RemoteForceShutdownChannel(Hash256, Option<GetShutdownTxResponse>),
    // Broadcast our BroadcastMessage to the network.
    BroadcastMessages(Vec<BroadcastMessageWithTimestamp>),
    // Broadcast local information to the network.
    BroadcastLocalInfo(LocalInfoKind),
    // Payment related commands
    SendPayment(
        SendPaymentCommand,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    // Send payment with router
    SendPaymentWithRouter(
        SendPaymentWithRouterCommand,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    // Get Payment Session for query payment status and errors
    GetPayment(Hash256, RpcReplyPort<Result<SendPaymentResponse, String>>),
    // Build a payment router with the given hops
    BuildPaymentRouter(
        BuildRouterCommand,
        RpcReplyPort<Result<PaymentRouter, String>>,
    ),
    // Get the count of inflight payments
    GetInflightPaymentCount(RpcReplyPort<Result<u32, String>>),

    AddInvoice(
        CkbInvoice,
        Option<Hash256>,
        RpcReplyPort<Result<(), InvoiceError>>,
    ),

    SettleInvoice(
        Hash256,
        Hash256,
        RpcReplyPort<Result<(), SettleInvoiceError>>,
    ),

    NodeInfo((), RpcReplyPort<Result<NodeInfoResponse, String>>),
    ListPeers((), RpcReplyPort<Result<Vec<PeerInfo>, String>>),
    // Get all inbound channel requests that are waiting for `accept_channel`
    GetPendingAcceptChannels(RpcReplyPort<Result<Vec<PendingAcceptChannel>, String>>),

    #[cfg(any(debug_assertions, feature = "bench"))]
    UpdateFeatures(FeatureVector),
}

pub fn sign_network_message(private_key: &Privkey, message: [u8; 32]) -> EcdsaSignature {
    debug!(
        "Signing message with node private key: message {:?}, public key {:?}",
        message,
        private_key.pubkey()
    );
    private_key.sign(message)
}

#[derive(Debug)]
pub struct OpenChannelCommand {
    pub peer_id: PeerId,
    pub funding_amount: u128,
    pub public: bool,
    pub one_way: bool,
    pub shutdown_script: Option<Script>,
    pub funding_udt_type_script: Option<Script>,
    pub commitment_fee_rate: Option<u64>,
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,
    pub funding_fee_rate: Option<u64>,
    pub tlc_expiry_delta: Option<u64>,
    pub tlc_min_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_tlc_number_in_flight: Option<u64>,
}

/// A hop requirement need to meet when building router, do not including the source node,
/// the last hop is the target node.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopRequire {
    /// The public key of the node
    pub(crate) pubkey: Pubkey,
    /// The outpoint for the channel, which means use channel with `channel_outpoint` to reach this node
    #[serde_as(as = "Option<EntityHex>")]
    pub(crate) channel_outpoint: Option<OutPoint>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildRouterCommand {
    /// the amount of the payment, the unit is Shannons for non UDT payment
    pub amount: Option<u128>,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub hops_info: Vec<HopRequire>,
    pub final_tlc_expiry_delta: Option<u64>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentRouter {
    pub router_hops: Vec<RouterHop>,
}

#[derive(Debug)]
pub struct AcceptChannelCommand {
    pub temp_channel_id: Hash256,
    pub funding_amount: u128,
    pub shutdown_script: Option<Script>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_tlc_number_in_flight: Option<u64>,
    pub min_tlc_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub tlc_expiry_delta: Option<u64>,
}

impl NetworkActorMessage {
    pub fn new_event(event: NetworkActorEvent) -> Self {
        Self::Event(event)
    }

    pub fn new_command(command: NetworkActorCommand) -> Self {
        Self::Command(command)
    }

    pub fn new_notification(service_event: NetworkServiceEvent) -> Self {
        Self::Notification(service_event)
    }
}

#[cfg(any(debug_assertions, feature = "bench"))]
#[derive(Clone, Debug)]
pub enum DebugEvent {
    // A AddTlc peer message processed with failure
    AddTlcFailed(PeerId, Hash256, TlcErr),
    // Common event with string
    Common(String),
}

#[macro_export]
macro_rules! debug_event {
    ($network:expr, $debug_event:expr) => {
        #[cfg(any(debug_assertions, feature = "bench"))]
        $network
            .send_message(NetworkActorMessage::new_notification(
                $crate::fiber::network::NetworkServiceEvent::DebugEvent(
                    $crate::fiber::network::DebugEvent::Common($debug_event.to_string()),
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    };
}

#[derive(Clone, Debug, AsRefStr)]
pub enum NetworkServiceEvent {
    NetworkStarted(PeerId, Vec<MultiAddr>, Vec<Multiaddr>),
    NetworkStopped(PeerId),
    PeerConnected(PeerId, Multiaddr),
    PeerDisConnected(PeerId, Multiaddr),
    // An incoming/outgoing channel is created.
    ChannelCreated(PeerId, Hash256),
    // An incoming channel is pending to be accepted.
    ChannelPendingToBeAccepted(PeerId, Hash256),
    // A funding tx is completed. The watch tower may use this to monitor the channel.
    RemoteTxComplete(
        PeerId,
        Hash256,
        Option<Script>,
        Privkey,
        Pubkey,
        Pubkey,
        Pubkey,
        SettlementData,
    ),
    // The channel is ready to use (with funding transaction confirmed
    // and both parties sent ChannelReady messages).
    ChannelReady(PeerId, Hash256, OutPoint),
    ChannelClosed(PeerId, Hash256, Byte32),
    ChannelAbandon(Hash256),
    ChannelFundingAborted(Hash256),
    // A RevokeAndAck is received from the peer. Other data relevant to this
    // RevokeAndAck message are also assembled here. The watch tower may use this.
    RevokeAndAckReceived(
        PeerId,  /* Peer Id */
        Hash256, /* Channel Id */
        RevocationData,
        SettlementData,
    ),
    // The other party has signed a valid commitment transaction,
    // and we successfully assemble the partial signature from other party
    // to create a complete commitment transaction and a settlement transaction.
    RemoteCommitmentSigned(PeerId, Hash256, TransactionView, SettlementData),
    // We have signed a valid commitment transaction, and the other party may use
    // the signature we sent to them to create a complete commitment transaction
    LocalCommitmentSigned(Hash256, SettlementData),
    // Preimage is created for the payment hash, the first Hash256 is the payment hash,
    // and the second Hash256 is the preimage.
    PreimageCreated(Hash256, Hash256),
    // Preimage is removed for the payment hash.
    PreimageRemoved(Hash256),
    // Some other debug event for assertion.
    #[cfg(any(debug_assertions, feature = "bench"))]
    DebugEvent(DebugEvent),
}

/// Events that can be sent to the network actor. Except for NetworkServiceEvent,
/// all events are processed by the network actor.
#[derive(Debug, AsRefStr)]
pub enum NetworkActorEvent {
    /// Network events to be processed by this actor.
    PeerConnected(PeerId, Pubkey, SessionContext),
    PeerDisconnected(PeerId, SessionContext),
    FiberMessage(PeerId, FiberMessage),

    // Some gossip messages have been updated in the gossip message store.
    // Normally we need to propagate these messages to the network graph.
    GossipMessageUpdates(GossipMessageUpdates),

    /// Channel related events.
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
    /// A channel is going to be closed, waiting the closing transaction to be broadcasted and confirmed.
    ClosingTransactionPending(Hash256, PeerId, TransactionView, bool),

    /// Both parties are now able to broadcast a valid funding transaction.
    FundingTransactionPending(Transaction, OutPoint, Hash256),

    /// A funding transaction has been confirmed. The transaction was included in the
    /// block with the given transaction index, and the timestamp in the block header.
    FundingTransactionConfirmed(OutPoint, H256, u32, u64),

    /// A funding transaction has failed.
    FundingTransactionFailed(OutPoint),

    /// A closing transaction has been confirmed (peer_id, channel_id, tx_hash, force, close_by_us).
    ClosingTransactionConfirmed(PeerId, Hash256, Byte32, bool, bool),

    /// A closing transaction has failed (either because of invalid transaction or timeout)
    ClosingTransactionFailed(PeerId, Hash256, Byte32),

    // A tlc remove message is received. (payment_hash, attempt_id, remove_tlc)
    TlcRemoveReceived(Hash256, Option<u64>, RemoveTlcReason),

    // A payment need to retry
    RetrySendPayment(Hash256, Option<u64>),

    // AddTlc result from peer (payment_hash, attempt_id, add_tlc_result, (previous_channel_id, previous_tlc_id))
    AddTlcResult(
        Hash256,
        Option<u64>,
        Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
        Option<PrevTlcInfo>,
    ),

    // An owned channel is updated.
    OwnedChannelUpdateEvent(OwnedChannelUpdateEvent),

    // A channel actor stopped event.
    ChannelActorStopped(Hash256, StopReason),

    // A payment actor stopped event.
    PaymentActorStopped(Hash256),

    // Channel settlement check completed - channel is fully settled on-chain.
    ChannelSettlementCompleted(Hash256),
}

#[derive(Debug)]
pub enum NetworkActorMessage {
    Command(NetworkActorCommand),
    Event(NetworkActorEvent),
    Notification(NetworkServiceEvent),
}

impl Display for NetworkActorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(command) => write!(f, "Command.{}", command.as_ref()),
            Self::Event(event) => write!(f, "Event.{}", event.as_ref()),
            Self::Notification(event) => write!(f, "Notification.{}", event.as_ref()),
        }
    }
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
pub struct GossipMessageWithPeerId {
    pub peer_id: PeerId,
    pub message: GossipMessage,
}

impl GossipMessageWithPeerId {
    pub fn new(peer_id: PeerId, message: GossipMessage) -> Self {
        Self { peer_id, message }
    }
}

pub struct NetworkActor<S, C> {
    // An event emitter to notify outside observers.
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    chain_actor: ActorRef<CkbChainMessage>,
    store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    chain_client: C,
}

impl<S, C> NetworkActor<S, C>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + ChannelOpenRecordStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    pub fn new(
        event_sender: mpsc::Sender<NetworkServiceEvent>,
        chain_actor: ActorRef<CkbChainMessage>,
        store: S,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
        chain_client: C,
    ) -> Self {
        Self {
            event_sender,
            chain_actor,
            store: store.clone(),
            network_graph,
            chain_client,
        }
    }

    pub async fn handle_peer_message(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        peer_id: PeerId,
        message: FiberMessage,
    ) -> crate::Result<()> {
        match message {
            FiberMessage::Init(init_message) => {
                state.on_init_msg(myself, peer_id, init_message).await?;
            }
            // We should process OpenChannel message here because there is no channel corresponding
            // to the channel id in the message yet.
            FiberMessage::ChannelInitialization(open_channel) => {
                state.check_feature_compatibility(&peer_id)?;
                let temp_channel_id = open_channel.channel_id;
                let peer_id_for_logging = peer_id.clone();
                match state
                    .on_open_channel_msg(peer_id, open_channel.clone())
                    .await
                {
                    Ok(()) => {
                        let auto_accept = if let Some(udt_type_script) =
                            open_channel.funding_udt_type_script.as_ref()
                        {
                            is_udt_type_auto_accept(udt_type_script, open_channel.funding_amount)
                        } else {
                            state.auto_accept_channel_ckb_funding_amount > 0
                                && open_channel.funding_amount
                                    >= state.open_channel_auto_accept_min_ckb_funding_amount as u128
                        };
                        if auto_accept {
                            let accept_channel = AcceptChannelCommand {
                                temp_channel_id,
                                funding_amount: if open_channel.funding_udt_type_script.is_some() {
                                    0
                                } else {
                                    state.auto_accept_channel_ckb_funding_amount as u128
                                },
                                shutdown_script: None,
                                max_tlc_number_in_flight: None,
                                max_tlc_value_in_flight: None,
                                min_tlc_value: None,
                                tlc_fee_proportional_millionths: None,
                                tlc_expiry_delta: None,
                            };
                            state.create_inbound_channel(accept_channel).await?;
                        } else {
                            // Log warning when auto-accept fails
                            state.log_receiver_auto_accept_failure(
                                &peer_id_for_logging,
                                &open_channel,
                                temp_channel_id,
                            );
                            debug_event!(myself, "ChannelAutoAcceptFailed");
                        }
                    }
                    Err(err) => {
                        error!("Failed to process OpenChannel message: {}", err);
                    }
                }
            }
            FiberMessage::ChannelNormalOperation(msg) => {
                state.check_feature_compatibility(&peer_id)?;
                let channel_id = msg.get_channel_id();
                let found = state
                    .peer_session_map
                    .get(&peer_id)
                    .and_then(|peer| state.session_channels_map.get(&peer.session_id))
                    .is_some_and(|channels| channels.contains(&channel_id));

                if !found {
                    error!(
                            "Received a channel message for a channel that is not created with peer: {:?}",
                            channel_id
                        );
                    return Err(Error::ChannelNotFound(channel_id));
                }
                state
                    .send_message_to_channel_actor(
                        channel_id,
                        Some(&peer_id),
                        ChannelActorMessage::PeerMessage(msg),
                    )
                    .await;
            }
        };
        Ok(())
    }

    pub async fn handle_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        event: NetworkActorEvent,
    ) -> crate::Result<()> {
        match event {
            NetworkActorEvent::PeerConnected(id, pubkey, session) => {
                state.on_peer_connected(&id, pubkey, &session).await;
                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::PeerConnected(id, session.address),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            NetworkActorEvent::PeerDisconnected(id, session) => {
                state.on_peer_disconnected(&id);
                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::PeerDisConnected(id, session.address),
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
                    if let Some(channel) = state.channels.remove(&old) {
                        debug!("Channel accepted: {:?} -> {:?}", old, new);
                        state.channels.insert(new, channel);
                        if let Some(set) = state.session_channels_map.get_mut(&session) {
                            set.remove(&old);
                            set.insert(new);
                        };

                        // Update the opening record: rename from temp ID to final ID and advance status.
                        if let Some(mut record) = state.store.get_channel_open_record(&old) {
                            state.store.delete_channel_open_record(&old);
                            record.channel_id = new;
                            record.update_status(ChannelOpeningStatus::FundingTxBuilding);
                            state.store.insert_channel_open_record(record);
                        }

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
                                        local_amount: local,
                                        funding_fee_rate,
                                        remote_amount: remote,
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

                // Mark the opening record as ChannelReady (terminal success state).
                if let Some(mut record) = state.store.get_channel_open_record(&channel_id) {
                    record.update_status(ChannelOpeningStatus::ChannelReady);
                    state.store.insert_channel_open_record(record);
                }

                // FIXME(yukang): need to make sure ChannelReady is sent after the channel is reestablished
                state
                    .outpoint_channel_map
                    .insert(channel_outpoint.clone(), channel_id);

                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::ChannelReady(
                            peer_id.clone(),
                            channel_id,
                            channel_outpoint.clone(),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);

                // Retry payment attempts whose first hop uses this channel
                for attempt in self
                    .store
                    .get_pending_attempts_by_channel_outpoint(&channel_outpoint)
                {
                    debug!(
                        "Retrying payment attempt {:?} for channel {:?} reestablished",
                        attempt.payment_hash, channel_outpoint
                    );
                    if let Err(err) = myself.send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::RetrySendPayment(attempt.payment_hash, Some(attempt.id)),
                    )) {
                        debug!(
                            "Failed to register payment retry for {:?}: {:?}",
                            attempt.payment_hash, err
                        );
                    }
                }

                debug_event!(
                    myself,
                    format!(
                        "Channel is now ready with channel_id {:?} to peer {:?}",
                        channel_id, peer_id
                    )
                );
            }
            NetworkActorEvent::FiberMessage(peer_id, message) => {
                self.handle_peer_message(myself, state, peer_id, message)
                    .await?
            }
            NetworkActorEvent::FundingTransactionPending(transaction, outpoint, channel_id) => {
                // Advance the opening record to FundingTxBroadcasted.
                if let Some(mut record) = state.store.get_channel_open_record(&channel_id) {
                    record.update_status(ChannelOpeningStatus::FundingTxBroadcasted);
                    state.store.insert_channel_open_record(record);
                }
                state
                    .on_funding_transaction_pending(channel_id, transaction, outpoint)
                    .await;
            }
            NetworkActorEvent::FundingTransactionConfirmed(
                outpoint,
                block_hash,
                tx_index,
                timestamp,
            ) => {
                state
                    .on_funding_transaction_confirmed(outpoint, block_hash, tx_index, timestamp)
                    .await;
            }
            NetworkActorEvent::FundingTransactionFailed(outpoint) => {
                error!("Funding transaction failed: {:?}", outpoint);
                state.abort_funding(Either::Right(outpoint)).await;
            }
            NetworkActorEvent::ClosingTransactionPending(channel_id, peer_id, tx, force) => {
                state
                    .on_closing_transaction_pending(channel_id, peer_id.clone(), tx.clone(), force)
                    .await;
            }
            NetworkActorEvent::ClosingTransactionConfirmed(
                peer_id,
                channel_id,
                tx_hash,
                force,
                close_by_us,
            ) => {
                state
                    .on_closing_transaction_confirmed(
                        &peer_id,
                        &channel_id,
                        tx_hash,
                        force,
                        close_by_us,
                    )
                    .await;
            }
            NetworkActorEvent::ClosingTransactionFailed(peer_id, tx_hash, channel_id) => {
                error!(
                    "Closing transaction failed for channel {:?}, tx hash: {:?}, peer id: {:?}",
                    &channel_id, &tx_hash, &peer_id
                );
            }
            NetworkActorEvent::TlcRemoveReceived(payment_hash, attempt_id, remove_tlc_reason) => {
                // When a node is restarted, RemoveTLC will also be resent if necessary
                self.on_remove_tlc_event(
                    myself.clone(),
                    state,
                    payment_hash,
                    attempt_id,
                    remove_tlc_reason,
                )
                .await;
                #[cfg(debug_assertions)]
                {
                    if let Some(payment_session) = self.store.get_payment_session(payment_hash) {
                        debug_event!(
                            myself,
                            format!(
                                "after on_remove_tlc_event session_status: {:?}",
                                payment_session.status
                            )
                        );
                    }
                }
            }
            NetworkActorEvent::RetrySendPayment(payment_hash, attempt_id) => {
                self.resume_payment_actor_and_send_command(
                    myself,
                    state,
                    payment_hash,
                    PaymentActorMessage::RetrySendPayment(attempt_id),
                )
                .await;
            }
            NetworkActorEvent::AddTlcResult(
                payment_hash,
                attempt_id,
                add_tlc_result,
                previous_tlc,
            ) => {
                self.on_add_tlc_result_event(
                    myself,
                    state,
                    payment_hash,
                    attempt_id,
                    add_tlc_result,
                    previous_tlc,
                )
                .await;
            }
            NetworkActorEvent::GossipMessageUpdates(gossip_message_updates) => {
                let mut graph = self.network_graph.write().await;
                graph.update_for_messages(gossip_message_updates.messages);
                debug_event!(myself, "Received gossip message updates");
            }
            NetworkActorEvent::OwnedChannelUpdateEvent(owned_channel_update_event) => {
                let mut graph = self.network_graph.write().await;
                debug!(
                    "Received owned channel update event: {:?}",
                    owned_channel_update_event
                );
                let is_down =
                    matches!(owned_channel_update_event, OwnedChannelUpdateEvent::Down(_));
                graph.process_owned_channel_update_event(owned_channel_update_event);
                if is_down {
                    debug!("Owned channel is down");
                }
            }
            NetworkActorEvent::ChannelActorStopped(channel_id, reason) => {
                // If the channel failed before reaching ChannelReady, mark the opening record as Failed.
                if let Some(mut record) = state.store.get_channel_open_record(&channel_id) {
                    if record.status != ChannelOpeningStatus::ChannelReady {
                        let failure_detail = match reason {
                            StopReason::Abandon => "Channel was abandoned".to_string(),
                            StopReason::AbortFunding => "Funding transaction aborted".to_string(),
                            StopReason::PeerDisConnected => {
                                "Peer disconnected during channel opening".to_string()
                            }
                            StopReason::Closed => {
                                "Channel closed before becoming ready".to_string()
                            }
                        };
                        record.fail(failure_detail);
                        state.store.insert_channel_open_record(record);
                    }
                }
                state.on_channel_actor_stopped(channel_id, reason).await;
            }
            NetworkActorEvent::PaymentActorStopped(payment_hash) => {
                state.on_payment_actor_stopped(payment_hash).await;
            }
            NetworkActorEvent::ChannelSettlementCompleted(channel_id) => {
                // Update channel state to remove WAITING_ONCHAIN_SETTLEMENT flag
                if let Some(mut actor_state) = self.store.get_channel_actor_state(&channel_id) {
                    if let ChannelState::Closed(mut flags) = actor_state.state {
                        flags.remove(CloseFlags::WAITING_ONCHAIN_SETTLEMENT);
                        actor_state.state = ChannelState::Closed(flags);
                        self.store.insert_channel_actor_state(actor_state);
                        info!("Channel {channel_id:?} on-chain settlement completed");
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn handle_command(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        command: NetworkActorCommand,
    ) -> crate::Result<()> {
        match command {
            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId { peer_id, message }) => {
                state.send_fiber_message_to_peer(&peer_id, message).await?;
            }
            NetworkActorCommand::ConnectPeer(addr) => {
                // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                // e.g. whether the peer support some specific feature.
                if let Some(peer_id) = extract_peer_id(&addr) {
                    if state.is_connected(&peer_id) {
                        debug!("Peer {:?} already connected, ignoring...", peer_id);
                        return Ok(());
                    }
                    if state.peer_id == peer_id {
                        debug!("Trying to connect to self {:?}, ignoring...", addr);
                        return Ok(());
                    }

                    state
                        .control
                        .dial(addr.clone(), TargetProtocol::All)
                        .await?
                } else {
                    error!("Failed to extract peer id from address: {:?}", addr);
                    return Ok(());
                }

                // TODO: note that the dial function does not return error immediately even if dial fails.
                // Tentacle sends an event by calling handle_error function instead, which
                // may receive errors like DialerError.
            }
            NetworkActorCommand::DisconnectPeer(peer_id, reason) => {
                if let Some(session) = state.get_peer_session(&peer_id) {
                    debug!(
                        "Disconnecting peer {:?} session w {:?}ith reason {:?}",
                        &peer_id, &session, &reason
                    );
                    state.control.disconnect(session).await?;
                }
            }
            NetworkActorCommand::SavePeerAddress(addr) => match extract_peer_id(&addr) {
                Some(peer) => {
                    debug!("Saved peer id {:?} with address {:?}", &peer, &addr);
                    state.save_peer_address(peer, addr);
                }
                None => {
                    error!("Failed to save address to peer store: unable to extract peer id from address {:?}", &addr);
                }
            },
            NetworkActorCommand::MaintainConnections => {
                debug!("Trying to connect to peers with mutual channels");

                for (peer_id, channel_id, channel_state) in self.store.get_channel_states(None) {
                    if state.is_connected(&peer_id) {
                        continue;
                    }
                    let addresses = state.get_peer_addresses(&peer_id);

                    debug!(
                        "Reconnecting channel {:x} peers {:?} in state {:?} with addresses {:?}",
                        &channel_id, &peer_id, &channel_state, &addresses
                    );

                    if let Some(addr) = addresses.iter().choose(&mut rand::thread_rng()) {
                        myself
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::ConnectPeer(addr.to_owned()),
                            ))
                            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                    }
                }

                let mut inbound_peer_sessions = state.inbound_peer_sessions();
                let num_inbound_peers = inbound_peer_sessions.len();
                let num_outbound_peers = state.num_of_outbound_peers();

                debug!("Maintaining network connections ticked: current num inbound peers {}, current num outbound peers {}", num_inbound_peers, num_outbound_peers);

                if num_inbound_peers > state.max_inbound_peers {
                    debug!(
                                "Already connected to {} inbound peers, only wants {} peers, disconnecting some",
                                num_inbound_peers, state.max_inbound_peers
                            );
                    inbound_peer_sessions.retain(|k| !state.session_channels_map.contains_key(k));
                    let sessions_to_disconnect = if inbound_peer_sessions.len()
                        < num_inbound_peers - state.max_inbound_peers
                    {
                        warn!(
                                    "Wants to disconnect more {} inbound peers, but all peers except {:?} have channels, will not disconnect any peer with channels",
                                    num_inbound_peers - state.max_inbound_peers, &inbound_peer_sessions
                                );
                        &inbound_peer_sessions[..]
                    } else {
                        &inbound_peer_sessions[..num_inbound_peers - state.max_inbound_peers]
                    };
                    debug!(
                        "Disconnecting inbound peer sessions {:?}",
                        sessions_to_disconnect
                    );
                    for session in sessions_to_disconnect {
                        if let Err(err) = state.control.disconnect(*session).await {
                            error!("Failed to disconnect session: {}", err);
                        }
                    }
                }

                if num_outbound_peers >= state.min_outbound_peers {
                    debug!(
                                "Already connected to {} outbound peers, wants a minimal of {} peers, skipping connecting to more peers",
                                num_outbound_peers, state.min_outbound_peers
                            );
                    return Ok(());
                }

                let peers_to_connect = {
                    let graph = self.network_graph.read().await;
                    let n_peers_to_connect = state.min_outbound_peers - num_outbound_peers;
                    let n_graph_nodes = graph.num_of_nodes();
                    let n_saved_peers = state.state_to_be_persisted.num_of_saved_nodes();
                    let n_all_saved_peers = n_graph_nodes + n_saved_peers;
                    if n_all_saved_peers == 0 {
                        return Ok(());
                    }
                    let n_saved_peers_to_connect =
                        n_peers_to_connect * n_saved_peers / n_all_saved_peers;
                    let n_graph_nodes_to_connect = n_peers_to_connect - n_saved_peers_to_connect;

                    let saved_peers_to_connect = state
                        .state_to_be_persisted
                        .sample_n_peers_to_connect(n_saved_peers_to_connect);
                    trace!(
                        "Randomly selected peers from saved addresses to connect: {:?}",
                        &saved_peers_to_connect
                    );
                    let graph_nodes_to_connect =
                        graph.sample_n_peers_to_connect(n_graph_nodes_to_connect);
                    trace!(
                        "Randomly selected peers from network graph to connect: {:?}",
                        &graph_nodes_to_connect
                    );
                    saved_peers_to_connect
                        .into_iter()
                        .chain(graph_nodes_to_connect.into_iter())
                };

                let mut rng = rand::thread_rng();
                for (peer_id, addresses) in peers_to_connect {
                    debug!("Peer to connect: {:?}, {:?}", peer_id, addresses);
                    if let Some(session) = state.get_peer_session(&peer_id) {
                        debug!(
                                    "Randomly selected peer {:?} already connected with session id {:?}, skipping connection",
                                    peer_id, session
                                );
                        continue;
                    }

                    // Randomly pick one address to connect
                    if let Some(addr) = addresses.choose(&mut rng) {
                        state
                            .network
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::ConnectPeer(addr.clone()),
                            ))
                            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                    }
                }
            }
            NetworkActorCommand::CheckPeerInit(peer_id, session_id) => {
                // Check if the peer has sent Init message.
                if let Some(session) = state.peer_session_map.get(&peer_id) {
                    // If Peer reconnect, the session_id will changed, and a new CheckPeerInit command will be issued.
                    // In that case we just skip check here.
                    if session.session_id == session_id && session.features.is_none() {
                        state
                            .network
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::DisconnectPeer(
                                    peer_id.clone(),
                                    PeerDisconnectReason::InitMessageTimeout,
                                ),
                            ))
                            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                    }
                }
            }
            NetworkActorCommand::CheckChannelsShutdown => {
                for (_peer_id, channel_id, channel_state) in self.store.get_channel_states(None) {
                    if matches!(
                        channel_state,
                        ChannelState::ChannelReady | ChannelState::ShuttingDown(..)
                    ) {
                        if let Some(actor_state) = self.store.get_channel_actor_state(&channel_id) {
                            let funding_lock_script = state
                                .get_cached_channel_funding_lock_script(channel_id, &actor_state);
                            // Spawn async task for concurrent RPC call
                            let chain_client = self.chain_client.clone();
                            let myself_clone = myself.clone();
                            crate::tasks::spawn(async move {
                                Self::check_channel_shutdown(
                                    chain_client,
                                    myself_clone,
                                    channel_id,
                                    funding_lock_script,
                                )
                                .await;
                            });
                        }
                    } else if matches!(
                        channel_state,
                        ChannelState::Closed(flags)
                            if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
                    ) {
                        if let Some(actor_state) = self.store.get_channel_actor_state(&channel_id) {
                            // Spawn async task for concurrent RPC call
                            let chain_client = self.chain_client.clone();
                            let myself_clone = myself.clone();
                            crate::tasks::spawn(async move {
                                Self::check_channel_shutdown_settlement(
                                    chain_client,
                                    myself_clone,
                                    actor_state,
                                )
                                .await;
                            });
                        }
                    }
                }
            }
            NetworkActorCommand::CheckChannels => {
                let now = now_timestamp_as_millis_u64();

                // peer has active channels but down
                let mut with_channel_down_peers = HashSet::new();
                let mut ready_channels_count = 0;
                let mut shuttingdown_channels_count = 0;
                for (peer_id, channel_id, channel_state) in self.store.get_channel_states(None) {
                    if matches!(channel_state, ChannelState::ChannelReady) {
                        if let Some(actor_state) = self.store.get_channel_actor_state(&channel_id) {
                            ready_channels_count += 1;
                            if actor_state.reestablishing {
                                continue;
                            }

                            if !state.peer_session_map.contains_key(&peer_id) {
                                with_channel_down_peers.insert(peer_id);
                            }

                            for tlc in actor_state.tlc_state.get_committed_received_tlcs() {
                                // skip if tlc amount is not fulfilled invoice
                                // this may happened if payment is mpp
                                if let Some(invoice) = self.store.get_invoice(&tlc.payment_hash) {
                                    if !is_invoice_fulfilled(&invoice, std::iter::once(tlc)) {
                                        continue;
                                    }
                                }

                                let Some(payment_preimage) =
                                    self.store.get_preimage(&tlc.payment_hash)
                                else {
                                    continue;
                                };
                                debug!(
                                    "Found payment preimage for channel {:?} tlc {:?}",
                                    channel_id,
                                    tlc.id()
                                );
                                if self
                                    .store
                                    .get_invoice_status(&tlc.payment_hash)
                                    .is_some_and(|s| {
                                        !matches!(
                                            s,
                                            CkbInvoiceStatus::Open | CkbInvoiceStatus::Received
                                        )
                                    })
                                {
                                    continue;
                                }

                                let (send, _recv) = oneshot::channel();
                                let rpc_reply = RpcReplyPort::from(send);

                                if let Err(err) = state
                                    .send_command_to_channel(
                                        channel_id,
                                        ChannelCommand::RemoveTlc(
                                            RemoveTlcCommand {
                                                id: tlc.id(),
                                                reason: RemoveTlcReason::RemoveTlcFulfill(
                                                    RemoveTlcFulfill { payment_preimage },
                                                ),
                                            },
                                            rpc_reply,
                                        ),
                                    )
                                    .await
                                {
                                    error!(
                                        "Failed to remove tlc {:?} with preimage for channel {:?}: {}",
                                        tlc.id(),
                                        channel_id,
                                        err
                                    );
                                }
                            }

                            let delay_epoch = EpochNumberWithFraction::from_full_value(
                                actor_state.commitment_delay_epoch,
                            );
                            let epoch_delay_milliseconds = tlc_expiry_delay(&delay_epoch);
                            // for received tlcs, check whether the tlc is expired, if so we send RemoveTlc message
                            // to previous hop, even if later hop send backup RemoveTlc message to us later,
                            // it will be ignored.
                            let expect_expiry = now
                                + epoch_delay_milliseconds
                                + CHECK_CHANNELS_INTERVAL.as_millis() as u64;
                            let expired_tlcs = actor_state
                                .tlc_state
                                .get_committed_received_tlcs()
                                .filter(|tlc| {
                                    tlc.forwarding_tlc.is_none() && tlc.expiry < expect_expiry
                                })
                                .collect::<Vec<_>>();
                            for tlc in expired_tlcs {
                                info!(
                                    "Removing expired tlc {:?} for channel {:?}",
                                    tlc.id(),
                                    channel_id
                                );
                                let (send, _recv) = oneshot::channel();
                                let rpc_reply = RpcReplyPort::from(send);
                                if let Err(err) = state
                                    .send_command_to_channel(
                                        channel_id,
                                        ChannelCommand::RemoveTlc(
                                            RemoveTlcCommand {
                                                id: tlc.id(),
                                                reason: RemoveTlcReason::RemoveTlcFail(
                                                    TlcErrPacket::new(
                                                        TlcErr::new(TlcErrorCode::ExpiryTooSoon),
                                                        &tlc.shared_secret,
                                                    ),
                                                ),
                                            },
                                            rpc_reply,
                                        ),
                                    )
                                    .await
                                {
                                    error!(
                                        "Failed to remove expired tlc {:?} for channel {:?}: {}",
                                        tlc.id(),
                                        channel_id,
                                        err
                                    );
                                }
                            }

                            // check whether the next hop have already sent us the RemoveTlc message
                            // for the offered expired tlc, if not we will force close the channel
                            let expect_expiry = now + epoch_delay_milliseconds;
                            if actor_state
                                .tlc_state
                                .get_expired_offered_tlcs(expect_expiry)
                                .next()
                                .is_some()
                            {
                                info!(
                                    "Force closing channel {:?} due to expired offered tlc",
                                    channel_id
                                );
                                let (send, _recv) = oneshot::channel();
                                let rpc_reply = RpcReplyPort::from(send);
                                if let Err(err) = state
                                    .send_command_to_channel(
                                        channel_id,
                                        ChannelCommand::Shutdown(
                                            ShutdownCommand {
                                                close_script: None,
                                                fee_rate: None,
                                                force: true,
                                            },
                                            rpc_reply,
                                        ),
                                    )
                                    .await
                                {
                                    error!(
                                        "Failed to force close channel {:?}: {}",
                                        channel_id, err
                                    );
                                }
                            }
                        }
                    } else if matches!(channel_state, ChannelState::ShuttingDown(..)) {
                        shuttingdown_channels_count += 1;
                    } else if matches!(
                        channel_state,
                        ChannelState::Closed(flags)
                            if flags.intersects(
                                CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::UNCOOPERATIVE_REMOTE
                            )
                    ) {
                        if let Some(actor_state) = self.store.get_channel_actor_state(&channel_id) {
                            let delay_epoch = EpochNumberWithFraction::from_full_value(
                                actor_state.commitment_delay_epoch,
                            );
                            let epoch_delay_milliseconds = tlc_expiry_delay(&delay_epoch);
                            let expect_expiry = now + epoch_delay_milliseconds;
                            for tlc in actor_state
                                .tlc_state
                                .get_expired_offered_tlcs(expect_expiry)
                            {
                                if let Some((forwarding_channel_id, forwarding_tlc_id)) =
                                    tlc.forwarding_tlc
                                {
                                    if self.store.is_tlc_settled(&channel_id, &tlc.payment_hash) {
                                        let (send, _recv) = oneshot::channel();
                                        let rpc_reply = RpcReplyPort::from(send);
                                        if let Err(err) = state
                                            .send_command_to_channel(
                                                forwarding_channel_id,
                                                ChannelCommand::RemoveTlc(
                                                    RemoveTlcCommand {
                                                        id: forwarding_tlc_id,
                                                        reason: RemoveTlcReason::RemoveTlcFail(
                                                            TlcErrPacket::new(
                                                                TlcErr::new(
                                                                    TlcErrorCode::ExpiryTooSoon,
                                                                ),
                                                                &tlc.shared_secret,
                                                            ),
                                                        ),
                                                    },
                                                    rpc_reply,
                                                ),
                                            )
                                            .await
                                        {
                                            error!(
                                                "Failed to remove settled tlc {:?} for channel {:?}: {}",
                                                forwarding_tlc_id, forwarding_channel_id, err
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // update metrics
                #[cfg(all(feature = "metrics", not(target_arch = "wasm32")))]
                {
                    // channels
                    metrics::gauge!(crate::metrics::DOWN_WITH_CHANNEL_PEER_COUNT)
                        .set(with_channel_down_peers.len() as u32);
                    metrics::gauge!(crate::metrics::TOTAL_CHANNEL_COUNT)
                        .set((ready_channels_count + shuttingdown_channels_count) as u32);
                    metrics::gauge!(crate::metrics::READY_CHANNEL_COUNT)
                        .set(ready_channels_count as u32);
                    metrics::gauge!(crate::metrics::SHUTTING_DOWN_CHANNEL_COUNT)
                        .set(shuttingdown_channels_count as u32);
                    metrics::gauge!(crate::metrics::INFLIGHT_PAYMENTS_COUNT)
                        .set(state.inflight_payments.len() as u32);

                    // peers
                    let total_peers = state.peer_session_map.len();
                    let outbound_peers = state
                        .peer_session_map
                        .iter()
                        .filter(|(_id, peer)| peer.session_type.is_outbound())
                        .count();
                    let inbound_peers = total_peers - outbound_peers;
                    metrics::gauge!(crate::metrics::TOTAL_PEER_COUNT).set(total_peers as u32);
                    metrics::gauge!(crate::metrics::INBOUND_PEER_COUNT).set(inbound_peers as u32);
                    metrics::gauge!(crate::metrics::OUTBOUND_PEER_COUNT).set(outbound_peers as u32);
                }
                debug!(
                    "Check channels: {} ready channels {} shutting down channels, found {} peers down with channels",
                    ready_channels_count, shuttingdown_channels_count,
                    with_channel_down_peers.len()
                );

                // Due to channel offline or network issues, remove hold tlc maybe failed,
                // we retry timeout these tlcs.
                let current_time = now_timestamp_as_millis_u64();
                for (payment_hash, hold_tlcs) in self.store.get_node_hold_tlcs() {
                    // timeout hold tlc
                    let already_timeout = hold_tlcs
                        .iter()
                        .any(|hold_tlc| current_time >= hold_tlc.hold_expire_at);
                    if already_timeout {
                        debug!("Timeout {payment_hash} hold tlcs {}", hold_tlcs.len());
                        for hold_tlc in hold_tlcs {
                            myself
                                .send_message(NetworkActorMessage::new_command(
                                    NetworkActorCommand::TimeoutHoldTlc(
                                        payment_hash,
                                        hold_tlc.channel_id,
                                        hold_tlc.tlc_id,
                                    ),
                                ))
                                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                        }
                    }
                }
            }
            NetworkActorCommand::SettleHoldTlcSet(payment_hash) => {
                self.settle_hold_tlc_set(state, payment_hash).await;
            }
            NetworkActorCommand::SettleTlcSet(payment_hash, channel_tlc_ids) => {
                self.settle_tlc_set(state, payment_hash, channel_tlc_ids, false)
                    .await;
            }
            NetworkActorCommand::TimeoutHoldTlc(payment_hash, channel_id, tlc_id) => {
                self.timeout_hold_tlc(state, payment_hash, channel_id, tlc_id)
                    .await;
            }
            NetworkActorCommand::OpenChannel(open_channel, reply) => {
                let network_graph = self.network_graph.clone();
                match state
                    .create_outbound_channel(open_channel, network_graph)
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
                match state.create_inbound_channel(accept_channel).await {
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
            NetworkActorCommand::AbandonChannel(channel_id, reply) => {
                match state.abandon_channel(channel_id).await {
                    Ok(_) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(err) => {
                        error!("Failed to abandon channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }
            NetworkActorCommand::ControlFiberChannel(c) => {
                state
                    .send_command_to_channel(c.channel_id, c.command)
                    .await?
            }
            NetworkActorCommand::SendPaymentOnionPacket(command, reply) => {
                match self
                    .handle_send_onion_packet_command(state, command.clone())
                    .await
                {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(err) => {
                        self.on_add_tlc_result_event(
                            myself,
                            state,
                            command.payment_hash,
                            command.attempt_id,
                            Err((
                                ProcessingChannelError::TlcForwardingError(err.clone()),
                                err.clone(),
                            )),
                            command.previous_tlc,
                        )
                        .await;
                        let _ = reply.send(Err(err));
                    }
                }
            }
            NetworkActorCommand::UpdateChannelFunding(channel_id, transaction, request) => {
                let old_tx = transaction.into_view();
                let mut tx = FundingTx::new();
                tx.update_for_self(old_tx);
                let tx = match self.fund(tx, request).await {
                    Ok(tx) => match tx.into_inner() {
                        Some(tx) => tx,
                        _ => {
                            error!("Obtained empty funding tx");
                            return Ok(());
                        }
                    },
                    Err(err) => {
                        error!("Failed to fund channel: {}", err);
                        if !err.is_temporary() {
                            state.abort_funding(Either::Left(channel_id)).await;
                        }
                        return Ok(());
                    }
                };
                if tracing::enabled!(target: "fnn::fiber::network::funding", tracing::Level::DEBUG)
                {
                    let tx_json: ckb_jsonrpc_types::Transaction = tx.data().into();
                    let tx_json = serde_json::to_string(&tx_json).unwrap_or_default();
                    debug!(target: "fnn::fiber::network::funding", "Funding transaction updated on our part: {}", tx_json);
                }
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
            NetworkActorCommand::VerifyFundingTx {
                local_tx,
                remote_tx,
                funding_cell_lock_script,
                reply,
            } => {
                let _ = self
                    .chain_actor
                    .send_message(CkbChainMessage::VerifyFundingTx {
                        local_tx,
                        remote_tx,
                        reply,
                        funding_cell_lock_script,
                    });
            }
            NetworkActorCommand::NotifyFundingTx(tx) => {
                let _ = self
                    .chain_actor
                    .send_message(CkbChainMessage::AddFundingTx(tx.into()));
            }
            NetworkActorCommand::SignFundingTx(
                ref peer_id,
                ref channel_id,
                funding_tx,
                partial_witnesses,
            ) => {
                let tx_hash: Hash256 = funding_tx.calc_tx_hash().into();

                // Check if we have partial witnesses before moving them
                let has_partial_witnesses = partial_witnesses.is_some();

                // Prepare funding transaction with partial witnesses if provided
                let funding_tx = match partial_witnesses {
                    Some(partial_witnesses) => {
                        debug!(
                            "Received SignFudningTx request with for transaction {:?} and partial witnesses {:?}",
                            &funding_tx,
                            partial_witnesses
                                .iter()
                                .map(hex::encode)
                                .collect::<Vec<_>>()
                        );
                        funding_tx
                            .into_view()
                            .as_advanced_builder()
                            .set_witnesses(
                                partial_witnesses.into_iter().map(|x| x.pack()).collect(),
                            )
                            .build()
                    }
                    None => {
                        debug!(
                            "Received SignFundingTx request with for transaction {:?} without partial witnesses, so start signing it now",
                            &funding_tx,
                        );
                        funding_tx.into_view()
                    }
                };

                // Sign the funding transaction
                let mut signed_funding_tx = match call_t!(
                    self.chain_actor,
                    CkbChainMessage::Sign,
                    DEFAULT_CHAIN_ACTOR_TIMEOUT,
                    funding_tx.into()
                )
                .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
                {
                    Ok(funding_tx) => funding_tx,
                    Err(err) => {
                        error!("Failed to sign funding transaction: {}", err);
                        // Send TxAbort message to peer
                        let abort_msg = FiberMessageWithPeerId {
                            peer_id: peer_id.clone(),
                            message: FiberMessage::ChannelNormalOperation(
                                FiberChannelMessage::TxAbort(TxAbort {
                                    channel_id: *channel_id,
                                    message: format!("Failed to sign funding transaction: {}", err)
                                        .as_bytes()
                                        .to_vec(),
                                }),
                            ),
                        };
                        myself
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::SendFiberMessage(abort_msg),
                            ))
                            .expect("network actor alive");
                        // Abort funding and close the channel
                        state.abort_funding(Either::Left(*channel_id)).await;
                        return Ok(());
                    }
                };
                debug!("Funding transaction signed: {:?}", &signed_funding_tx);

                // Extract signed transaction and witnesses
                let funding_tx = signed_funding_tx.take().expect("take tx");
                let witnesses = funding_tx.witnesses();

                // If we received partial witnesses, the transaction is fully signed
                // and we should notify that it's pending confirmation
                if has_partial_witnesses {
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
                } else {
                    debug!("Partially signed funding tx {:?}", &funding_tx);
                }

                // Create the message to send to peer
                let msg = FiberMessageWithPeerId {
                    peer_id: peer_id.clone(),
                    message: FiberMessage::ChannelNormalOperation(
                        FiberChannelMessage::TxSignatures(TxSignatures {
                            channel_id: *channel_id,
                            witnesses: witnesses.into_iter().map(|x| x.unpack()).collect(),
                        }),
                    ),
                };

                // Before sending the signatures to the peer, start tracing the tx
                // It should be the first time to trace the tx
                state
                    .trace_tx(tx_hash, InFlightCkbTxKind::Funding(*channel_id))
                    .await?;

                // Notify channel actor to save the signatures
                if let Err(err) = state
                    .send_command_to_channel(
                        *channel_id,
                        ChannelCommand::FundingTxSigned(funding_tx.data()),
                    )
                    .await
                {
                    error!(
                        "Failed to update signed funding tx {:?}: {}",
                        channel_id, err
                    );
                }

                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(msg),
                    ))
                    .expect("network actor alive");
            }
            NetworkActorCommand::CheckChannelShutdown(channel_id) => {
                if let Some(channel_state) = self.store.get_channel_actor_state(&channel_id) {
                    let funding_lock_script =
                        state.get_cached_channel_funding_lock_script(channel_id, &channel_state);
                    // Spawn async task for concurrent RPC call
                    let chain_client = self.chain_client.clone();
                    let myself_clone = myself.clone();
                    crate::tasks::spawn(async move {
                        Self::check_channel_shutdown(
                            chain_client,
                            myself_clone,
                            channel_id,
                            funding_lock_script,
                        )
                        .await;
                    });
                } else {
                    tracing::debug!(
                        "stop check channel shutdown, can't find {channel_id:?} actor state"
                    );
                }
            }
            NetworkActorCommand::RemoteForceShutdownChannel(channel_id, response) => {
                if let Some(shutdown_tx_response) = response {
                    self.handle_remote_channel_shutdown(myself, channel_id, shutdown_tx_response)
                        .await;
                }
            }
            NetworkActorCommand::BroadcastMessages(message) => {
                if let Some(ref gossip_actor) = state.gossip_actor {
                    gossip_actor
                        .send_message(GossipActorMessage::TryBroadcastMessages(message))
                        .expect(ASSUME_GOSSIP_ACTOR_ALIVE);
                } else {
                    debug!("Gossip actor is not available, skipping broadcast message");
                }
            }
            NetworkActorCommand::SendPayment(payment_request, reply) => {
                let payment_request = match payment_request.build_send_payment_data() {
                    Ok(payment) => payment,
                    Err(err) => {
                        error!("Failed to build payment from command: {:?}", err);
                        let _ = reply.send(Err(err.to_string()));
                        return Ok(());
                    }
                };

                let _ = self
                    .start_payment_actor(
                        myself,
                        state,
                        payment_request.payment_hash,
                        PaymentActorMessage::SendPayment(payment_request, reply),
                    )
                    .await;
            }
            NetworkActorCommand::SendPaymentWithRouter(payment_request, reply) => {
                let source = self.network_graph.read().await.get_source_pubkey();
                let payment_request = match payment_request.build_send_payment_data(source) {
                    Ok(payment) => payment,
                    Err(err) => {
                        error!("Failed to build payment from command: {:?}", err);
                        let _ = reply.send(Err(err.to_string()));
                        return Ok(());
                    }
                };
                let _ = self
                    .start_payment_actor(
                        myself,
                        state,
                        payment_request.payment_hash,
                        PaymentActorMessage::SendPayment(payment_request, reply),
                    )
                    .await;
            }
            NetworkActorCommand::BuildPaymentRouter(build_payment_router, reply) => {
                match self.on_build_payment_router(build_payment_router).await {
                    Ok(router) => {
                        let _ = reply.send(Ok(router));
                    }
                    Err(e) => {
                        error!("Failed to build payment router: {:?}", e);
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            NetworkActorCommand::GetPayment(payment_hash, reply) => {
                match self.on_get_payment(&payment_hash) {
                    Ok(payment) => {
                        let _ = reply.send(Ok(payment));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            NetworkActorCommand::BroadcastLocalInfo(kind) => match kind {
                LocalInfoKind::NodeAnnouncement => {
                    let message = state.get_or_create_new_node_announcement_message();
                    myself
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::BroadcastMessages(vec![
                                BroadcastMessageWithTimestamp::NodeAnnouncement(message),
                            ]),
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
            },
            NetworkActorCommand::NodeInfo(_, rpc) => {
                let response = NodeInfoResponse {
                    node_name: state.node_name,
                    node_id: state.get_public_key(),
                    features: state.features.clone(),
                    addresses: state.announced_addrs.clone(),
                    chain_hash: get_chain_hash(),
                    open_channel_auto_accept_min_ckb_funding_amount: state
                        .open_channel_auto_accept_min_ckb_funding_amount,
                    auto_accept_channel_ckb_funding_amount: state
                        .auto_accept_channel_ckb_funding_amount,
                    tlc_expiry_delta: state.tlc_expiry_delta,
                    tlc_min_value: state.tlc_min_value,
                    tlc_fee_proportional_millionths: state.tlc_fee_proportional_millionths,
                    channel_count: state.channels.len() as u32,
                    pending_channel_count: state.pending_channels.len() as u32,
                    peers_count: state.peer_session_map.len() as u32,
                    udt_cfg_infos: get_udt_whitelist(),
                };
                let _ = rpc.send(Ok(response));
            }
            NetworkActorCommand::GetInflightPaymentCount(reply) => {
                let _ = reply.send(Ok(state.inflight_payments.len() as u32));
            }
            NetworkActorCommand::ListPeers(_, rpc) => {
                let peers = state
                    .peer_session_map
                    .iter()
                    .map(|(peer_id, peer)| PeerInfo {
                        peer_id: peer_id.clone(),
                        pubkey: peer.pubkey,
                        address: peer.address.clone(),
                    })
                    .collect::<Vec<_>>();
                let _ = rpc.send(Ok(peers));
            }

            NetworkActorCommand::GetPendingAcceptChannels(rpc) => {
                let pending = state
                    .to_be_accepted_channels
                    .map
                    .iter()
                    .map(
                        |(channel_id, (peer_id, open_channel))| PendingAcceptChannel {
                            channel_id: *channel_id,
                            peer_id: peer_id.clone(),
                            funding_amount: open_channel.funding_amount,
                            udt_type_script: open_channel.funding_udt_type_script.clone(),
                            created_at: state
                                .store
                                .get_channel_open_record(channel_id)
                                .map(|r| r.created_at)
                                .unwrap_or_else(crate::now_timestamp_as_millis_u64),
                        },
                    )
                    .collect::<Vec<_>>();
                let _ = rpc.send(Ok(pending));
            }

            NetworkActorCommand::SettleInvoice(hash, preimage, reply) => {
                let _ = reply.send(self.settle_invoice(&myself, hash, preimage));
            }
            NetworkActorCommand::AddInvoice(invoice, preimage, reply) => {
                let _ = reply.send(self.add_invoice(invoice, preimage));
            }

            #[cfg(any(debug_assertions, feature = "bench"))]
            NetworkActorCommand::UpdateFeatures(features) => {
                state.features = features;
                state.last_node_announcement_message = None;
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::BroadcastLocalInfo(LocalInfoKind::NodeAnnouncement),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        };
        Ok(())
    }

    async fn timeout_hold_tlc(
        &self,
        state: &mut NetworkActorState<S, C>,
        payment_hash: Hash256,
        channel_id: Hash256,
        tlc_id: u64,
    ) {
        if self.store.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received) {
            // When invoice is marked as received, we ignore the hold TLC timeout and only
            // remove the TLC when it actually expires. Expired TLCs are removed in the
            // CheckChannels routine (see NetworkActorCommand::CheckChannels handler).
            return;
        }

        let channel_actor_state = self.store.get_channel_actor_state(&channel_id);
        let tlc = channel_actor_state
            .as_ref()
            .and_then(|state| state.tlc_state.get(&TLCId::Received(tlc_id)));
        let Some(tlc) = tlc else {
            trace!(
                "Timeout tlc {:?} (payment hash {:?}) for channel {:?}: tlc is settled or not found, just unhold it",
                tlc_id, payment_hash, channel_id
            );
            // remove hold tlc from store
            self.store
                .remove_payment_hold_tlc(&payment_hash, &channel_id, tlc_id);
            return;
        };

        debug!(
            "Removing timeout hold tlc: payment_hash={:?} channel_id={:?} tlc_id={:?}",
            payment_hash, channel_id, tlc_id
        );

        let (send, _recv) = oneshot::channel();
        let rpc_reply = RpcReplyPort::from(send);
        match state
            .send_command_to_channel(
                channel_id,
                ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: tlc.id(),
                        reason: RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                            TlcErr::new(TlcErrorCode::HoldTlcTimeout),
                            &tlc.shared_secret,
                        )),
                    },
                    rpc_reply,
                ),
            )
            .await
        {
            Ok(_) => {
                debug!(
                    "Succeeded to remove tlc {:?} for channel {:?}",
                    tlc.id(),
                    channel_id,
                );
                // remove hold tlc from store
                self.store
                    .remove_payment_hold_tlc(&payment_hash, &channel_id, tlc_id);
            }
            Err(err) => {
                debug!(
                    "Failed to remove tlc {:?} for channel {:?}: {}, will retry on next check",
                    tlc.id(),
                    channel_id,
                    err
                );
            }
        }
    }

    async fn settle_hold_tlc_set(
        &self,
        state: &mut NetworkActorState<S, C>,
        payment_hash: Hash256,
    ) {
        let channel_tlc_ids = self
            .store
            .get_payment_hold_tlcs(payment_hash)
            .iter()
            .map(|hold_tlc| (hold_tlc.channel_id, hold_tlc.tlc_id))
            .collect();
        self.settle_tlc_set(state, payment_hash, channel_tlc_ids, true)
            .await
    }

    async fn settle_tlc_set(
        &self,
        state: &mut NetworkActorState<S, C>,
        payment_hash: Hash256,
        channel_tlc_ids: Vec<(Hash256, u64)>,
        is_hold_tlc_set: bool,
    ) {
        let settle_command = SettleTlcSetCommand::new(payment_hash, channel_tlc_ids, &self.store);
        for tlc_settlement in settle_command.run() {
            let (send, _recv) = oneshot::channel();
            let rpc_reply = RpcReplyPort::from(send);
            match state
                .send_command_to_channel(
                    tlc_settlement.channel_id(),
                    ChannelCommand::RemoveTlc(
                        tlc_settlement.remove_tlc_command().clone(),
                        rpc_reply,
                    ),
                )
                .await
            {
                Ok(_) => {
                    if is_hold_tlc_set {
                        self.store.remove_payment_hold_tlc(
                            &payment_hash,
                            &tlc_settlement.channel_id(),
                            tlc_settlement.tlc_id(),
                        );
                    }
                }
                Err(err) => {
                    error!(
                        "Failed to remove tlc {:?} for channel {:?}: {}",
                        tlc_settlement.tlc_id(),
                        tlc_settlement.channel_id(),
                        err
                    );
                }
            }
        }
    }

    /// Async version of check_channel_shutdown that runs in spawned task.
    /// Checks if the channel funding cell has been spent (indicating remote force close).
    async fn check_channel_shutdown(
        chain_client: C,
        myself: ActorRef<NetworkActorMessage>,
        channel_id: Hash256,
        funding_lock_script: Script,
    ) {
        match chain_client.get_shutdown_tx(funding_lock_script).await {
            Ok(shutdown_tx) => {
                let _ = myself.send_message(NetworkActorMessage::Command(
                    NetworkActorCommand::RemoteForceShutdownChannel(channel_id, shutdown_tx),
                ));
            }
            Err(err) => {
                tracing::error!("Failed to check shutdown tx for channel {channel_id:?}: {err:?}");
            }
        }
    }

    /// Async version of check_channel_shutdown_settlement that runs in spawned task.
    /// Checks if the commitment transaction outputs have been spent (indicating settlement complete).
    async fn check_channel_shutdown_settlement(
        chain_client: C,
        myself: ActorRef<NetworkActorMessage>,
        state: ChannelActorState,
    ) {
        let channel_id = state.get_id();
        let ChannelState::Closed(flags) = state.state else {
            return;
        };
        if !flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT) {
            return;
        }

        let Some(tx_hash) = state.shutdown_transaction_hash.clone() else {
            debug!(
                "stop check channel settlement, {:?} missing shutdown tx hash",
                channel_id
            );
            return;
        };

        let tx_response = match chain_client.get_transaction(tx_hash.clone()).await {
            Ok(response) => response,
            Err(err) => {
                error!(
                    "Failed to load commitment tx {:?} during settlement check: {:?}",
                    tx_hash, err
                );
                return;
            }
        };

        let Some(tx) = tx_response.transaction else {
            debug!(
                "Commitment tx {:?} not available when checking settlement",
                tx_hash
            );
            return;
        };

        let Some(output) = tx.outputs().get(0) else {
            warn!(
                "Commitment tx {:?} has no outputs when checking settlement",
                tx_hash
            );
            return;
        };

        let lock = output.lock();
        let lock_args = lock.args().raw_data();
        if lock_args.len() < 36 {
            warn!(
                "Commitment tx {:?} lock args too short: {:?}",
                tx_hash, lock_args
            );
            return;
        }
        let prefix_lock = lock
            .as_builder()
            .args(lock_args[0..36].to_vec().pack())
            .build();

        let search_key = SearchKey {
            script: prefix_lock.into(),
            script_type: ScriptType::Lock,
            script_search_mode: Some(SearchMode::Prefix),
            with_data: Some(false),
            filter: None,
            group_by_transaction: None,
        };

        match chain_client
            .get_cells(search_key, Order::Desc, 1, None)
            .await
        {
            Ok(response) => {
                let response = crate::ckb::GetCellsResponse::from(response);
                if response.objects.is_empty() {
                    // Notify actor that settlement is complete
                    let _ = myself.send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelSettlementCompleted(channel_id),
                    ));
                }
            }
            Err(err) => {
                error!(
                    "Failed to check commitment cells for {:?}: {:?}",
                    channel_id, err
                );
            }
        }
    }

    // Check shutdown tx of a channel, shutdown channel if channel is force closed by remote
    async fn handle_remote_channel_shutdown(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        channel_id: Hash256,
        response: GetShutdownTxResponse,
    ) {
        let Some(state) = self.store.get_channel_actor_state(&channel_id) else {
            tracing::debug!("skip check channel shutdown, can't find {channel_id:?} actor state");
            return;
        };

        if !matches!(
            state.state,
            ChannelState::ChannelReady | ChannelState::ShuttingDown(..)
        ) {
            return;
        }

        if let GetShutdownTxResponse {
            transaction: Some(tx),
            tx_status: TxStatus::Committed(..),
        } = response
        {
            // we only check remote sent force close transaction here
            if tx.outputs().len() == 1 {
                if let Some(output) = tx.outputs().get(0) {
                    // Check if channel is force closed by counter party
                    let lock_args =
                        &blake2b_256(state.get_commitment_lock_script_xonly(true))[0..20];
                    if &output.lock().args().raw_data()[0..20] == lock_args {
                        let channel_id = state.get_id();
                        let peer_id = state.get_remote_peer_id();
                        let tx_hash = tx.hash();
                        tracing::debug!("channel {channel_id:?} is shutdown by remote");
                        myself
                            .send_message(NetworkActorMessage::Event(
                                NetworkActorEvent::ClosingTransactionConfirmed(
                                    peer_id, channel_id, tx_hash, true, false,
                                ),
                            ))
                            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    }
                }
            }
        }
    }

    pub fn add_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let payment_hash = invoice.payment_hash();
        if self.store.get_invoice(payment_hash).is_some() {
            return Err(InvoiceError::InvoiceAlreadyExists);
        }
        self.store.insert_invoice(invoice, preimage)
    }

    pub fn settle_invoice(
        &self,
        myself: &ActorRef<NetworkActorMessage>,
        payment_hash: Hash256,
        payment_preimage: Hash256,
    ) -> Result<(), SettleInvoiceError> {
        let invoice = self
            .store
            .get_invoice(&payment_hash)
            .ok_or(SettleInvoiceError::InvoiceNotFound)?;

        let hash_algorithm = invoice.hash_algorithm().copied().unwrap_or_default();
        let hash = hash_algorithm.hash(payment_preimage);
        if hash.as_slice() != payment_hash.as_ref() {
            return Err(SettleInvoiceError::HashMismatch);
        }

        // Allow only settling Received invoice. When the invoice is Received, it's safe to notify
        // that the preimage can be revealed.
        match self.store.get_invoice_status(&payment_hash) {
            Some(CkbInvoiceStatus::Received) => {}
            Some(CkbInvoiceStatus::Open) => {
                if invoice.is_expired() {
                    return Err(SettleInvoiceError::InvoiceAlreadyExpired);
                }
                return Err(SettleInvoiceError::InvoiceStillOpen);
            }
            Some(CkbInvoiceStatus::Cancelled) => {
                return Err(SettleInvoiceError::InvoiceAlreadyCancelled);
            }
            Some(CkbInvoiceStatus::Expired) => {
                return Err(SettleInvoiceError::InvoiceAlreadyExpired);
            }
            Some(CkbInvoiceStatus::Paid) => return Err(SettleInvoiceError::InvoiceAlreadyPaid),
            None => return Err(SettleInvoiceError::InvoiceNotFound),
        }

        self.store.insert_preimage(payment_hash, payment_preimage);
        // Notify watchtower about the preimage so it can settle TLCs on-chain if needed
        // (e.g., after force close).
        myself
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::PreimageCreated(payment_hash, payment_preimage),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        // We will send network actor a message to settle the invoice immediately if possible.
        let _ = myself.send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::SettleHoldTlcSet(payment_hash),
        ));

        Ok(())
    }

    async fn handle_send_onion_packet_command(
        &self,
        state: &mut NetworkActorState<S, C>,
        command: SendOnionPacketCommand,
    ) -> Result<(), TlcErr> {
        trace!("Entering handle_send_onion_packet_command");
        let SendOnionPacketCommand {
            peeled_onion_packet,
            previous_tlc,
            payment_hash,
            attempt_id,
        } = command;

        // Trampoline forwarding: the onion for this node is the last hop, but contains an
        // encrypted payload telling us the real final recipient and parameters.
        if let Some(trampoline_bytes) = peeled_onion_packet.current.trampoline_onion() {
            return self
                .forward_trampoline_packet(
                    state,
                    &trampoline_bytes,
                    previous_tlc,
                    payment_hash,
                    peeled_onion_packet.current.amount,
                )
                .await;
        }

        let info = peeled_onion_packet.current.clone();
        let shared_secret = peeled_onion_packet.shared_secret;
        let channel_outpoint = OutPoint::new(info.funding_tx_hash.into(), 0);
        let channel_id = match state.outpoint_channel_map.get(&channel_outpoint) {
            Some(channel_id) => channel_id,
            None => {
                error!(
                    "Channel id not found in outpoint_channel_map with {:?}, are we connected to the peer?",
                     channel_outpoint
                );
                let tlc_err = TlcErr::new_channel_fail(
                    TlcErrorCode::UnknownNextPeer,
                    state.get_public_key(),
                    channel_outpoint.clone(),
                    None,
                );
                return Err(tlc_err);
            }
        };

        let (send, _recv) = oneshot::channel::<Result<AddTlcResponse, TlcErr>>();
        // explicitly don't wait for the response, we will handle the result in AddTlcResult
        let rpc_reply = RpcReplyPort::from(send);
        let command = ChannelCommand::AddTlc(
            AddTlcCommand {
                amount: info.amount,
                payment_hash,
                attempt_id,
                expiry: info.expiry,
                hash_algorithm: info.hash_algorithm,
                onion_packet: peeled_onion_packet.next.clone(),
                shared_secret,
                is_trampoline_hop: false,
                previous_tlc,
            },
            rpc_reply,
        );
        trace!(
            "Sending AddTlcCommand to {}, command {:?}",
            *channel_id,
            command
        );
        // we have already checked the channel_id is valid,
        match state.send_command_to_channel(*channel_id, command).await {
            Ok(_) => {
                return Ok(());
            }
            Err(err) => {
                error!(
                    "Failed to send onion packet to channel: {:?} with err: {:?}",
                    channel_id, err
                );
                let tlc_error = self.get_tlc_error(state, &err, &channel_outpoint);
                return Err(tlc_error);
            }
        }
    }

    async fn forward_trampoline_packet(
        &self,
        state: &mut NetworkActorState<S, C>,
        trampoline_bytes: &[u8],
        previous_tlc: Option<PrevTlcInfo>,
        payment_hash: Hash256,
        incoming_amount: u128,
    ) -> Result<(), TlcErr> {
        if !state.features.supports_trampoline_routing() {
            error!(
                "Trampoline forwarding rejected: local node does not support trampoline routing"
            );
            return Err(TlcErr::new_node_fail(
                TlcErrorCode::RequiredNodeFeatureMissing,
                state.get_public_key(),
            ));
        }
        let trampoline_packet = TrampolineOnionPacket::new(trampoline_bytes.to_vec());
        let prev_channel_state = self
            .store
            .get_channel_actor_state(&previous_tlc.expect("got previous tlc").prev_channel_id)
            .ok_or_else(|| {
                TlcErr::new_node_fail(TlcErrorCode::TemporaryNodeFailure, state.get_public_key())
            })?;
        let udt_type_script = prev_channel_state.funding_udt_type_script.clone();
        let peeled_trampoline = trampoline_packet
            .peel(&state.private_key, Some(payment_hash.as_ref()), SECP256K1)
            .map_err(|_| {
                TlcErr::new_node_fail(TlcErrorCode::TemporaryNodeFailure, state.get_public_key())
            })?;
        match peeled_trampoline.current {
            TrampolineHopPayload::Forward {
                next_node_id,
                amount_to_forward,
                hash_algorithm,
                build_max_fee_amount,
                tlc_expiry_delta,
                tlc_expiry_limit,
                max_parts,
            } => {
                if incoming_amount <= amount_to_forward {
                    error!(
                        "Trampoline forwarding fee insufficient: incoming {}, forward {}",
                        incoming_amount, amount_to_forward
                    );
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::FeeInsufficient,
                        state.get_public_key(),
                    ));
                }
                let available_fee_amount = incoming_amount.saturating_sub(amount_to_forward);
                if available_fee_amount != build_max_fee_amount {
                    error!(
                        "Trampoline forwarding fee mismatch: available {}, build max {}",
                        available_fee_amount, build_max_fee_amount
                    );
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::InvalidOnionPayload,
                        state.get_public_key(),
                    ));
                }

                let (Some(remaining_trampoline_onion), Some(prev_tlc)) =
                    (peeled_trampoline.next.map(|p| p.into_bytes()), previous_tlc)
                else {
                    return Err(TlcErr::new_node_fail(
                        TlcErrorCode::InvalidOnionPayload,
                        state.get_public_key(),
                    ));
                };

                let payment_data =
                    SendPaymentDataBuilder::new(next_node_id, amount_to_forward, payment_hash)
                        .final_tlc_expiry_delta(tlc_expiry_delta)
                        .tlc_expiry_limit(tlc_expiry_limit)
                        .max_fee_amount(Some(build_max_fee_amount))
                        .max_parts(max_parts)
                        .udt_type_script(udt_type_script)
                        .trampoline_context(Some(TrampolineContext {
                            remaining_trampoline_onion,
                            // currently we only support single previous tlc in trampoline forwarding,
                            // maybe we need to support multiple previous tlcs in the future
                            previous_tlcs: vec![prev_tlc],
                            hash_algorithm,
                        }))
                        .allow_mpp(max_parts.is_some_and(|v| v > 1))
                        .build()
                        .map_err(|_| {
                            TlcErr::new_node_fail(
                                TlcErrorCode::TemporaryNodeFailure,
                                state.get_public_key(),
                            )
                        })?;

                let (send, _recv) = oneshot::channel();
                let rpc_reply = RpcReplyPort::from(send);

                match self
                    .start_payment_actor(
                        state.network.clone(),
                        state,
                        payment_hash,
                        PaymentActorMessage::SendPayment(payment_data, rpc_reply),
                    )
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        error!("Failed to start trampoline payment: {}", e);
                        Err(TlcErr::new_node_fail(
                            TlcErrorCode::TemporaryNodeFailure,
                            state.get_public_key(),
                        ))
                    }
                }
            }
            TrampolineHopPayload::Final { .. } => {
                // The channel actor should directly settle when this node is the final recipient.
                // This case should not happen.
                Err(TlcErr::new_node_fail(
                    TlcErrorCode::TemporaryNodeFailure,
                    state.get_public_key(),
                ))
            }
        }
    }

    fn get_tlc_error(
        &self,
        state: &mut NetworkActorState<S, C>,
        error: &Error,
        channel_outpoint: &OutPoint,
    ) -> TlcErr {
        let node_id = state.get_public_key();
        match error {
            Error::ChannelNotFound(_) | Error::PeerNotFound(_) => TlcErr::new_channel_fail(
                TlcErrorCode::UnknownNextPeer,
                node_id,
                channel_outpoint.clone(),
                None,
            ),
            Error::ChannelError(_) => TlcErr::new_channel_fail(
                TlcErrorCode::TemporaryChannelFailure,
                node_id,
                channel_outpoint.clone(),
                None,
            ),
            _ => {
                error!(
                    "Failed to send onion packet to channel: {:?} with err: {:?}",
                    channel_outpoint, error
                );
                TlcErr::new_node_fail(TlcErrorCode::TemporaryNodeFailure, state.get_public_key())
            }
        }
    }

    async fn on_remove_tlc_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
        reason: RemoveTlcReason,
    ) {
        self.resume_payment_actor_and_send_command(
            myself,
            state,
            payment_hash,
            PaymentActorMessage::OnRemoveTlcEvent { attempt_id, reason },
        )
        .await;
    }

    fn on_get_payment(&self, payment_hash: &Hash256) -> Result<SendPaymentResponse, Error> {
        match self.store.get_payment_session(*payment_hash) {
            Some(session_state) => Ok(session_state.into()),
            None => Err(Error::InvalidParameter(format!(
                "Payment session not found: {:?}",
                payment_hash
            ))),
        }
    }

    async fn on_add_tlc_result_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
        add_tlc_result: Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
        previous_tlc: Option<PrevTlcInfo>,
    ) {
        if let Some(PrevTlcInfo {
            prev_channel_id: channel_id,
            prev_tlc_id: tlc_id,
            ..
        }) = previous_tlc
        {
            myself
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                        channel_id,
                        command: ChannelCommand::NotifyEvent(ChannelEvent::ForwardTlcResult(
                            ForwardTlcResult {
                                payment_hash,
                                channel_id,
                                tlc_id,
                                add_tlc_result: add_tlc_result.clone(),
                            },
                        )),
                    }),
                ))
                .expect("network actor alive");
            return;
        }

        self.resume_payment_actor_and_send_command(
            myself,
            state,
            payment_hash,
            PaymentActorMessage::OnAddTlcResultEvent {
                attempt_id,
                add_tlc_result,
            },
        )
        .await;
    }

    async fn resume_payment_actor_and_send_command(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        payment_hash: Hash256,
        message: PaymentActorMessage,
    ) {
        if let Some(actor) = state.inflight_payments.get(&payment_hash) {
            if let Err(err) = actor.send_message(message) {
                debug!(
                            "PaymentActor message dropped because payment actor is likely stopping, error: {err}"
                        );
            }
        } else {
            debug!(
                "Can't find inflight payment actor for {payment_hash:?}, start a new payment actor"
            );

            if let Err(e) = self
                .start_payment_actor(myself, state, payment_hash, message)
                .await
            {
                warn!("Failed to resume payment actor: {}", e);
            }
        }
    }

    async fn start_payment_actor(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S, C>,
        payment_hash: Hash256,
        init_command: PaymentActorMessage,
    ) -> Result<(), String> {
        if state.inflight_payments.contains_key(&payment_hash) {
            error!("Already had a payment actor with the same hash {payment_hash:?}");

            if let PaymentActorMessage::SendPayment(_, reply) = init_command {
                let _ = reply.send(Err(format!(
                    "Payment session already exists, stop start new payment actor for {payment_hash:?}"
                )));
            }
            return Err(format!(
                "Payment session already exists for {payment_hash:?}"
            ));
        }

        let args = PaymentActorArguments {
            payment_hash,
            init_command,
        };
        match Actor::spawn_linked(
            Some(format!(
                "Payment-{} Node({:?})",
                payment_hash,
                myself.get_name(),
            )),
            PaymentActor::new(
                self.store.clone(),
                self.network_graph.clone(),
                myself.clone(),
            ),
            args,
            myself.get_cell(),
        )
        .await
        {
            Ok((actor, _handle)) => {
                debug!("Payment actor start {payment_hash}");
                state.inflight_payments.insert(payment_hash, actor);
                Ok(())
            }
            Err(err) => {
                error!("Failed to start payment actor: {:?}", err);
                Err(format!("Failed to start payment actor: {:?}", err))
            }
        }
    }

    async fn on_build_payment_router(
        &self,
        command: BuildRouterCommand,
    ) -> Result<PaymentRouter, Error> {
        // Only proceed if we have at least one hop requirement
        let Some(_last_hop) = command.hops_info.last() else {
            return Err(Error::InvalidParameter(
                "No hop requirements provided".to_string(),
            ));
        };

        let source = self.network_graph.read().await.get_source_pubkey();
        let router_hops = self
            .network_graph
            .read()
            .await
            .build_path(source, command)?;

        Ok(PaymentRouter { router_hops })
    }

    async fn fund(
        &self,
        tx: FundingTx,
        request: FundingRequest,
    ) -> Result<FundingTx, FundingError> {
        call_t!(
            self.chain_actor.clone(),
            CkbChainMessage::Fund,
            DEFAULT_CHAIN_ACTOR_TIMEOUT,
            tx,
            request
        )?
    }
}

pub struct NetworkActorState<S, C> {
    store: S,
    state_to_be_persisted: PersistentNetworkActorState,
    // The name of the node to be announced to the network, may be empty.
    node_name: Option<AnnouncedNodeName>,
    peer_id: PeerId,
    announced_addrs: Vec<Multiaddr>,
    auto_announce: bool,
    last_node_announcement_message: Option<NodeAnnouncement>,
    // We need to keep private key here in order to sign node announcement messages.
    private_key: Privkey,
    // This is the entropy used to generate various random values.
    // Must be kept secret.
    // TODO: Maybe we should abstract this into a separate trait.
    entropy: [u8; 32],
    // The default lock script to be used when closing a channel, may be overridden by the shutdown command.
    default_shutdown_script: Script,
    network: ActorRef<NetworkActorMessage>,
    // This immutable attribute is placed here because we need to create it in
    // the pre_start function.
    control: ServiceAsyncControl,
    peer_session_map: HashMap<PeerId, ConnectedPeer>,
    session_channels_map: HashMap<SessionId, HashSet<Hash256>>,
    channels: HashMap<Hash256, ActorRef<ChannelActorMessage>>,
    // Channels funding lock script cache
    channels_funding_lock_script_cache: HashMap<Hash256, Script>,
    // Outpoint to channel id mapping, only contains channels with state of Ready.
    // We need to remove the channel from this map when the channel is closed or peer disconnected.
    outpoint_channel_map: HashMap<OutPoint, Hash256>,
    // Channels in this hashmap are pending for acceptance. The user needs to
    // issue an AcceptChannelCommand with the amount of funding to accept the channel.
    to_be_accepted_channels: ToBeAcceptedChannels,
    // Channels in this hashmap are pending for funding transaction confirmation.
    pending_channels: HashMap<OutPoint, Hash256>,
    // Used to broadcast and query network info.
    chain_actor: ActorRef<CkbChainMessage>,
    // Used to query on-chain info.
    chain_client: C,
    // If the other party funding more than this amount, we will automatically accept the channel.
    open_channel_auto_accept_min_ckb_funding_amount: u64,
    // The default amount of CKB to be funded when auto accepting a channel.
    auto_accept_channel_ckb_funding_amount: u64,
    // The default expiry delta to forward tlcs.
    tlc_expiry_delta: u64,
    // The default tlc min and max value of tlcs to be accepted.
    tlc_min_value: u128,
    // The default tlc fee proportional millionths to be used when auto accepting a channel.
    tlc_fee_proportional_millionths: u128,
    // The gossip messages actor to process and send gossip messages.
    // None if gossip is disabled via sync_network_graph config.
    gossip_actor: Option<ActorRef<GossipActorMessage>>,
    max_inbound_peers: usize,
    min_outbound_peers: usize,
    // The features of the node, used to indicate the capabilities of the node.
    features: FeatureVector,
    channel_ephemeral_config: ChannelEphemeralConfig,

    // Inflight payment actors
    inflight_payments: HashMap<Hash256, ActorRef<PaymentActorMessage>>,
}

#[derive(Debug, Clone)]
pub struct ConnectedPeer {
    pub session_id: SessionId,
    pub session_type: SessionType,
    pub address: Multiaddr,
    pub pubkey: Pubkey,
    pub features: Option<FeatureVector>,
}

#[serde_as]
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PersistentNetworkActorState {
    // This map is used to store the public key of the peer.
    #[serde_as(as = "Vec<(DisplayFromStr, _)>")]
    peer_pubkey_map: HashMap<PeerId, Pubkey>,
    // These addresses are saved by the user (e.g. the user sends a ConnectPeer rpc to the node),
    // we will then save these addresses to the peer store.
    #[serde_as(as = "Vec<(DisplayFromStr, _)>")]
    saved_peer_addresses: HashMap<PeerId, Vec<Multiaddr>>,
}

impl PersistentNetworkActorState {
    pub fn new() -> Self {
        Default::default()
    }

    fn get_peer_addresses(&self, peer_id: &PeerId) -> Vec<Multiaddr> {
        self.saved_peer_addresses
            .get(peer_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Save a single peer address to the peer store. If this address for the peer does not exist,
    /// then return false, otherwise return true.
    fn save_peer_address(&mut self, peer_id: PeerId, addr: Multiaddr) -> bool {
        match self.saved_peer_addresses.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                if entry.get().contains(&addr) {
                    false
                } else {
                    entry.get_mut().push(addr);
                    true
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(vec![addr]);
                true
            }
        }
    }

    fn get_peer_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.peer_pubkey_map.get(peer_id).copied()
    }

    // Save a single peer pubkey to the peer store. Returns true if the new pubkey is different from the old one,
    // or there does not exist a old pubkey.
    fn save_peer_pubkey(&mut self, peer_id: PeerId, pubkey: Pubkey) -> bool {
        match self.peer_pubkey_map.insert(peer_id, pubkey) {
            Some(old_pubkey) => old_pubkey != pubkey,
            None => true,
        }
    }

    fn num_of_saved_nodes(&self) -> usize {
        self.saved_peer_addresses.len()
    }

    pub(crate) fn sample_n_peers_to_connect(&self, n: usize) -> HashMap<PeerId, Vec<Multiaddr>> {
        // TODO: we may need to shuffle the nodes before selecting the first n nodes,
        // to avoid some malicious nodes from being always selected.
        self.saved_peer_addresses
            .iter()
            .take(n)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

pub trait NetworkActorStateStore {
    fn get_network_actor_state(&self, id: &PeerId) -> Option<PersistentNetworkActorState>;
    fn insert_network_actor_state(&self, id: &PeerId, state: PersistentNetworkActorState);
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

impl<S, C> NetworkActorState<S, C>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + ChannelOpenRecordStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    pub fn get_or_create_new_node_announcement_message(&mut self) -> NodeAnnouncement {
        let now = now_timestamp_as_millis_u64();
        match self.last_node_announcement_message {
            // If the last node announcement message is still relatively new, we don't need to create a new one.
            // Because otherwise the receiving node may be confused by the multiple announcements,
            // and falsely believe we updated the node announcement, and then forward this message to other nodes.
            // This is undesirable because we don't want to flood the network with the same message.
            // On the other hand, if the message is too old, we need to create a new one.
            Some(ref message) if now.saturating_sub(message.timestamp) < 3600 * 1000 => {
                debug!("Returning old node announcement message as it is still valid");
            }
            _ => {
                let node_name = self.node_name.unwrap_or_default();
                let addresses = self.announced_addrs.clone();
                let announcement = NodeAnnouncement::new(
                    node_name,
                    self.features.clone(),
                    addresses,
                    &self.private_key,
                    now,
                    self.open_channel_auto_accept_min_ckb_funding_amount,
                );
                debug!(
                    "Created new node announcement message: {:?}, previous {:?}",
                    &announcement, self.last_node_announcement_message
                );
                self.last_node_announcement_message = Some(announcement);
            }
        }
        self.last_node_announcement_message
            .clone()
            .expect("last node announcement message is present")
    }

    pub fn get_public_key(&self) -> Pubkey {
        self.private_key.pubkey()
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

    /// Check peer's node announcement and log warnings if funding amount is insufficient for auto-accept
    fn check_and_log_peer_auto_accept_requirements(
        node_info: &super::graph::NodeInfo,
        peer_id: &PeerId,
        funding_amount: u128,
        funding_udt_type_script: &Option<Script>,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if let Some(udt_type_script) = funding_udt_type_script.as_ref() {
            Self::log_sender_udt_funding_warning(
                node_info,
                peer_id,
                funding_amount,
                udt_type_script,
            );
        } else {
            Self::log_sender_ckb_funding_warning(node_info, peer_id, funding_amount);
        }
    }

    /// Log warning when opening channel with UDT funding amount is insufficient for peer's auto-accept
    fn log_sender_udt_funding_warning(
        node_info: &super::graph::NodeInfo,
        peer_id: &PeerId,
        funding_amount: u128,
        udt_type_script: &Script,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if let Some(udt_cfg_info) = node_info.udt_cfg_infos.find_matching_udt(udt_type_script) {
            if let Some(auto_accept_amount) = udt_cfg_info.auto_accept_amount {
                if funding_amount < auto_accept_amount {
                    warn!(
                        "Opening channel to peer {:?} (node: {:?}) with UDT {:?} (name: {:?}) funding amount {} is less than peer's announced auto-accept minimum {}. The channel may not be auto-accepted.",
                        peer_id,
                        node_info.node_name,
                        udt_type_script,
                        udt_cfg_info.name,
                        funding_amount,
                        auto_accept_amount
                    );
                }
            } else {
                warn!(
                    "Opening channel to peer {:?} (node: {:?}) with UDT {:?} (name: {:?}). Peer has this UDT configured but auto-accept is not enabled. The channel may not be auto-accepted.",
                    peer_id,
                    node_info.node_name,
                    udt_type_script,
                    udt_cfg_info.name
                );
            }
        } else {
            warn!(
                "Opening channel to peer {:?} (node: {:?}) with UDT {:?}. UDT type not found in peer's udt_cfg_infos. The channel may not be auto-accepted.",
                peer_id,
                node_info.node_name,
                udt_type_script
            );
        }
    }

    /// Log warning when opening channel with CKB funding amount is insufficient for peer's auto-accept
    fn log_sender_ckb_funding_warning(
        node_info: &super::graph::NodeInfo,
        peer_id: &PeerId,
        funding_amount: u128,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if node_info.auto_accept_min_ckb_funding_amount == 0 {
            warn!(
                "Opening channel to peer {:?} (node: {:?}) with CKB funding amount {}. Auto-accept is disabled (auto_accept_min_ckb_funding_amount is 0). The channel may not be auto-accepted.",
                peer_id,
                node_info.node_name,
                funding_amount
            );
        } else if funding_amount < node_info.auto_accept_min_ckb_funding_amount as u128 {
            warn!(
                "Opening channel to peer {:?} (node: {:?}) with CKB funding amount {} is less than peer's announced auto-accept minimum {}. The channel may not be auto-accepted.",
                peer_id,
                node_info.node_name,
                funding_amount,
                node_info.auto_accept_min_ckb_funding_amount
            );
        }
    }

    /// Log warning when auto-accept fails for a received OpenChannel request
    fn log_receiver_auto_accept_failure(
        &self,
        peer_id: &PeerId,
        open_channel: &OpenChannel,
        temp_channel_id: Hash256,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if let Some(udt_type_script) = open_channel.funding_udt_type_script.as_ref() {
            Self::log_receiver_udt_auto_accept_failure(
                peer_id,
                udt_type_script,
                open_channel.funding_amount,
                temp_channel_id,
            );
        } else {
            Self::log_receiver_ckb_auto_accept_failure(
                peer_id,
                open_channel.funding_amount,
                temp_channel_id,
                self.auto_accept_channel_ckb_funding_amount,
                self.open_channel_auto_accept_min_ckb_funding_amount,
            );
        }
    }

    /// Log warning when auto-accept fails for UDT channel
    fn log_receiver_udt_auto_accept_failure(
        peer_id: &PeerId,
        udt_type_script: &Script,
        funding_amount: u128,
        temp_channel_id: Hash256,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        // Find matching UDT in local whitelist
        if let Some(udt_info) = get_udt_info(udt_type_script) {
            if let Some(auto_accept_amount) = udt_info.auto_accept_amount {
                warn!(
                    "Received OpenChannel request from peer {:?} with UDT {:?} (name: {:?}) funding amount {} is less than required auto-accept minimum {}. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                    peer_id,
                    udt_type_script,
                    udt_info.name,
                    funding_amount,
                    auto_accept_amount,
                    temp_channel_id
                );
            } else {
                warn!(
                    "Received OpenChannel request from peer {:?} with UDT {:?} (name: {:?}). Auto-accept is not enabled for this UDT. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                    peer_id,
                    udt_type_script,
                    udt_info.name,
                    temp_channel_id
                );
            }
        } else {
            warn!(
                "Received OpenChannel request from peer {:?} with UDT {:?} that is not configured for auto-accept. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                peer_id,
                udt_type_script,
                temp_channel_id
            );
        }
    }

    /// Log warning when auto-accept fails for CKB channel
    fn log_receiver_ckb_auto_accept_failure(
        peer_id: &PeerId,
        funding_amount: u128,
        temp_channel_id: Hash256,
        auto_accept_channel_ckb_funding_amount: u64,
        open_channel_auto_accept_min_ckb_funding_amount: u64,
    ) {
        if !tracing::enabled!(tracing::Level::WARN) {
            return;
        }
        if auto_accept_channel_ckb_funding_amount == 0 {
            warn!(
                "Received OpenChannel request from peer {:?} with CKB funding amount {}. Auto-accept is disabled (auto_accept_channel_ckb_funding_amount is 0). Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                peer_id,
                funding_amount,
                temp_channel_id
            );
        } else {
            warn!(
                "Received OpenChannel request from peer {:?} with CKB funding amount {} is less than required auto-accept minimum {}. Channel {:?} will not be auto-accepted and is pending manual acceptance.",
                peer_id,
                funding_amount,
                open_channel_auto_accept_min_ckb_funding_amount,
                temp_channel_id
            );
        }
    }

    pub async fn create_outbound_channel(
        &mut self,
        open_channel: OpenChannelCommand,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256), ProcessingChannelError> {
        let store = self.store.clone();
        let network = self.network.clone();
        let OpenChannelCommand {
            peer_id,
            funding_amount,
            public,
            one_way,
            shutdown_script,
            funding_udt_type_script,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            tlc_expiry_delta,
            tlc_min_value,
            tlc_fee_proportional_millionths,
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
        } = open_channel;

        self.check_feature_compatibility(&peer_id)?;

        if public && one_way {
            return Err(ProcessingChannelError::InvalidParameter(
                "An one-way channel cannot be public".to_string(),
            ));
        }

        let remote_pubkey =
            self.get_peer_pubkey(&peer_id)
                .ok_or(ProcessingChannelError::InvalidParameter(format!(
                    "Peer {:?} pubkey not found",
                    &peer_id
                )))?;

        // Check peer's node announcement for auto-accept requirements
        let graph = network_graph.read().await;
        if let Some(node_info) = graph.get_node(&remote_pubkey) {
            Self::check_and_log_peer_auto_accept_requirements(
                node_info,
                &peer_id,
                funding_amount,
                &funding_udt_type_script,
            );
        }
        drop(graph);

        if let Some(udt_type_script) = funding_udt_type_script.as_ref() {
            if !check_udt_script(udt_type_script) {
                return Err(ProcessingChannelError::InvalidParameter(
                    "Invalid UDT type script".to_string(),
                ));
            }
        }

        if tlc_expiry_delta.is_some_and(|d| d < MIN_TLC_EXPIRY_DELTA) {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "TLC expiry delta is too small, expect larger than {}, got {}",
                MIN_TLC_EXPIRY_DELTA,
                tlc_expiry_delta.unwrap()
            )));
        }

        let tlc_expiry_delta = tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta);
        let commitment_delay_epochs = commitment_delay_epoch.map_or_else(
            || EpochNumberWithFraction::new(DEFAULT_COMMITMENT_DELAY_EPOCHS, 0, 1).full_value(),
            |epochs| epochs.full_value(),
        );
        check_tlc_delta_with_epochs(tlc_expiry_delta, commitment_delay_epochs)?;

        let shutdown_script =
            shutdown_script.unwrap_or_else(|| self.default_shutdown_script.clone());

        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            Some(generate_channel_actor_name(&self.peer_id, &peer_id)),
            ChannelActor::new(self.get_public_key(), remote_pubkey, network.clone(), store),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::OpenChannel(OpenChannelParameter {
                    funding_amount,
                    seed,
                    tlc_info: ChannelTlcInfo::new(
                        tlc_min_value.unwrap_or(self.tlc_min_value),
                        tlc_expiry_delta,
                        tlc_fee_proportional_millionths
                            .unwrap_or(self.tlc_fee_proportional_millionths),
                    ),
                    public_channel_info: public.then_some(PublicChannelInfo::new()),
                    is_one_way: one_way,
                    funding_udt_type_script,
                    shutdown_script,
                    channel_id_sender: tx,
                    commitment_fee_rate,
                    commitment_delay_epoch,
                    funding_fee_rate,
                    max_tlc_value_in_flight: max_tlc_value_in_flight
                        .unwrap_or(DEFAULT_MAX_TLC_VALUE_IN_FLIGHT),
                    max_tlc_number_in_flight: max_tlc_number_in_flight
                        .unwrap_or(MAX_TLC_NUMBER_IN_FLIGHT),
                }),
                ephemeral_config: self.channel_ephemeral_config.clone(),
                private_key: self.private_key.clone(),
            },
            network.clone().get_cell(),
        )
        .await
        .map_err(|e| ProcessingChannelError::SpawnErr(e.to_string()))?
        .0;
        let temp_channel_id = rx.await.expect("msg received");
        self.on_channel_created(temp_channel_id, &peer_id, channel.clone());

        // Record the channel opening attempt so it can be queried via RPC.
        let record = ChannelOpenRecord::new(temp_channel_id, peer_id, funding_amount);
        self.store.insert_channel_open_record(record);

        Ok((channel, temp_channel_id))
    }

    pub async fn create_inbound_channel(
        &mut self,
        accept_channel: AcceptChannelCommand,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256, Hash256), ProcessingChannelError> {
        let store = self.store.clone();
        let AcceptChannelCommand {
            temp_channel_id,
            funding_amount,
            shutdown_script,
            max_tlc_number_in_flight,
            max_tlc_value_in_flight,
            min_tlc_value,
            tlc_fee_proportional_millionths,
            tlc_expiry_delta,
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

        let shutdown_script =
            shutdown_script.unwrap_or_else(|| self.default_shutdown_script.clone());
        let (funding_amount, reserved_ckb_amount) = get_funding_and_reserved_amount(
            funding_amount,
            &shutdown_script,
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
            Some(generate_channel_actor_name(&self.peer_id, &peer_id)),
            ChannelActor::new(self.get_public_key(), remote_pubkey, network.clone(), store),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::AcceptChannel(AcceptChannelParameter {
                    funding_amount,
                    reserved_ckb_amount,
                    tlc_info: ChannelTlcInfo::new(
                        min_tlc_value.unwrap_or(self.tlc_min_value),
                        tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta),
                        tlc_fee_proportional_millionths
                            .unwrap_or(self.tlc_fee_proportional_millionths),
                    ),
                    public_channel_info: open_channel
                        .is_public()
                        .then_some(PublicChannelInfo::new()),
                    seed,
                    open_channel,
                    shutdown_script,
                    channel_id_sender: Some(tx),
                    max_tlc_number_in_flight: max_tlc_number_in_flight
                        .unwrap_or(MAX_TLC_NUMBER_IN_FLIGHT),
                    max_tlc_value_in_flight: max_tlc_value_in_flight.unwrap_or(u128::MAX),
                }),
                ephemeral_config: self.channel_ephemeral_config.clone(),
                private_key: self.private_key.clone(),
            },
            network.clone().get_cell(),
        )
        .await
        .map_err(|e| ProcessingChannelError::SpawnErr(e.to_string()))?
        .0;
        let new_id = rx.await.expect("msg received");
        self.on_channel_created(new_id, &peer_id, channel.clone());

        // Re-key the inbound ChannelOpenRecord from the temp channel ID to the final channel ID
        // and advance the status to FundingTxBuilding now that the channel has been accepted.
        if let Some(mut record) = self.store.get_channel_open_record(&temp_channel_id) {
            self.store.delete_channel_open_record(&temp_channel_id);
            record.channel_id = new_id;
            record.update_status(ChannelOpeningStatus::FundingTxBuilding);
            self.store.insert_channel_open_record(record);
        }

        Ok((channel, temp_channel_id, new_id))
    }

    fn check_feature_compatibility(&self, peer_id: &PeerId) -> ProcessingChannelResult {
        if let Some(ConnectedPeer {
            features: Some(peer_features),
            ..
        }) = self.peer_session_map.get(peer_id)
        {
            // check peer features
            if !self.features.compatible_with(peer_features) {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Peer {:?} features {:?} are not compatible with our features {:?}",
                    peer_id, peer_features, self.features
                )));
            }
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Peer {:?}'s feature not found, waiting for peer to send Init message",
                peer_id
            )));
        }
        Ok(())
    }

    pub async fn trace_tx(
        &mut self,
        tx_hash: Hash256,
        tx_kind: InFlightCkbTxKind,
    ) -> crate::Result<()> {
        let handler = InFlightCkbTxActor {
            chain_actor: self.chain_actor.clone(),
            chain_client: self.chain_client.clone(),
            network_actor: self.network.clone(),
            tx_hash,
            tx_kind,
            confirmations: CKB_TX_TRACING_CONFIRMATIONS,
        };

        Actor::spawn_linked(
            None,
            handler,
            InFlightCkbTxActorArguments { transaction: None },
            self.network.get_cell(),
        )
        .await?;

        Ok(())
    }

    pub async fn send_tx(
        &mut self,
        tx: TransactionView,
        tx_kind: InFlightCkbTxKind,
    ) -> crate::Result<()> {
        let tx_hash = tx.hash().into();

        let handler = InFlightCkbTxActor {
            chain_actor: self.chain_actor.clone(),
            chain_client: self.chain_client.clone(),
            network_actor: self.network.clone(),
            tx_hash,
            tx_kind,
            confirmations: CKB_TX_TRACING_CONFIRMATIONS,
        };

        Actor::spawn_linked(
            None,
            handler,
            InFlightCkbTxActorArguments {
                transaction: Some(tx),
            },
            self.network.get_cell(),
        )
        .await?;

        Ok(())
    }

    pub async fn abort_funding(&mut self, channel_id_or_outpoint: Either<Hash256, OutPoint>) {
        let channel_id = match channel_id_or_outpoint {
            Either::Left(channel_id) => channel_id,
            Either::Right(outpoint) => match self.pending_channels.remove(&outpoint) {
                Some(channel_id) => channel_id,
                None => {
                    warn!(
                        "Funding transaction failed for outpoint {:?} but no channel found",
                        &outpoint
                    );
                    return;
                }
            },
        };

        self.send_message_to_channel_actor(
            channel_id,
            None,
            ChannelActorMessage::Event(ChannelEvent::Stop(StopReason::AbortFunding)),
        )
        .await;
    }

    pub async fn abandon_channel(&mut self, channel_id: Hash256) -> ProcessingChannelResult {
        if let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) {
            match channel_actor_state.state {
                ChannelState::ChannelReady
                | ChannelState::ShuttingDown(_)
                | ChannelState::Closed(_)
                | ChannelState::AwaitingChannelReady(_) => {
                    return Err(ProcessingChannelError::InvalidParameter(format!(
                        "Channel {} is in state {:?}, cannot be abandoned, please shutdown the channel instead",
                        channel_id, channel_actor_state.state
                    )));
                }
                ChannelState::AwaitingTxSignatures(flags)
                    if flags.contains(AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT) =>
                {
                    return Err(ProcessingChannelError::InvalidParameter(format!(
                        "Channel {} is in state {:?} and our signature has been sent. It cannot be abandoned. please wait for chain commitment.",
                        channel_id, channel_actor_state.state
                    )));
                }
                _ => {
                    if channel_actor_state.funding_tx_confirmed_at.is_some() {
                        return Err(ProcessingChannelError::InvalidParameter(format!(
                            "Channel {} funding transaction is already confirmed, please shutdown the channel instead",
                            channel_id,
                        )));
                    }
                }
            }
        }

        if let Some(channel) = self.channels.get(&channel_id) {
            if channel
                .send_message(ChannelActorMessage::Event(ChannelEvent::Stop(
                    StopReason::Abandon,
                )))
                .is_err()
            {
                return Err(ProcessingChannelError::InternalError(format!(
                    "Failed to stop channel actor {}",
                    channel_id
                )));
            }
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Channel {} not found",
                channel_id
            )));
        }
        return Ok(());
    }

    fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        self.peer_session_map.get(peer_id).map(|s| s.session_id)
    }

    fn inbound_peer_sessions(&self) -> Vec<SessionId> {
        self.peer_session_map
            .values()
            .filter_map(|s| (s.session_type == SessionType::Inbound).then_some(s.session_id))
            .collect()
    }

    fn num_of_outbound_peers(&self) -> usize {
        self.peer_session_map
            .values()
            .filter(|s| s.session_type == SessionType::Outbound)
            .count()
    }

    fn is_connected(&self, peer_id: &PeerId) -> bool {
        self.peer_session_map.contains_key(peer_id)
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
        self.peer_session_map
            .values()
            .take(n)
            .map(|s| s.session_id)
            .collect()
    }

    fn get_peer_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.state_to_be_persisted.get_peer_pubkey(peer_id)
    }

    async fn send_fiber_message_to_session(
        &self,
        session_id: SessionId,
        message: FiberMessage,
    ) -> crate::Result<()> {
        self.control
            .send_message_to(session_id, FIBER_PROTOCOL_ID, message.to_molecule_bytes())
            .await?;
        Ok(())
    }

    async fn send_fiber_message_to_peer(
        &self,
        peer_id: &PeerId,
        message: FiberMessage,
    ) -> crate::Result<()> {
        match self.get_peer_session(peer_id) {
            Some(session) => self.send_fiber_message_to_session(session, message).await,
            None => Err(Error::PeerNotFound(peer_id.clone())),
        }
    }

    async fn send_command_to_channel(
        &self,
        channel_id: Hash256,
        command: ChannelCommand,
    ) -> crate::Result<()> {
        match command {
            // Need to handle the force shutdown command specially because the ChannelActor
            // may not exist when remote peer is disconnected.
            ChannelCommand::Shutdown(shutdown, rpc_reply) if shutdown.force => {
                if let Some(actor) = self.channels.get(&channel_id) {
                    actor.send_message(ChannelActorMessage::Command(ChannelCommand::Shutdown(
                        shutdown, rpc_reply,
                    )))?;
                    Ok(())
                } else {
                    match self.store.get_channel_actor_state(&channel_id) {
                        Some(mut state) => {
                            match state.state {
                                ChannelState::ChannelReady => {
                                    debug!("Handling force shutdown command in ChannelReady state");
                                }
                                ChannelState::ShuttingDown(flags) => {
                                    debug!("Handling force shutdown command in ShuttingDown state, flags: {:?}", &flags);
                                }
                                _ => {
                                    let error = Error::ChannelError(
                                        ProcessingChannelError::InvalidState(format!(
                                            "Handling force shutdown command invalid state {:?}",
                                            &state.state
                                        )),
                                    );

                                    let _ = rpc_reply.send(Err(error.to_string()));
                                    return Err(error);
                                }
                            };

                            let transaction = match state.get_latest_commitment_transaction().await
                            {
                                Ok(tx) => tx,
                                Err(e) => {
                                    let error = Error::ChannelError(e);
                                    let _ = rpc_reply.send(Err(error.to_string()));
                                    return Err(error);
                                }
                            };

                            self.network
                                .send_message(NetworkActorMessage::new_event(
                                    NetworkActorEvent::ClosingTransactionPending(
                                        state.get_id(),
                                        state.get_remote_peer_id(),
                                        transaction,
                                        true,
                                    ),
                                ))
                                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                            state.update_state(ChannelState::ShuttingDown(
                                ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
                            ));
                            self.store.insert_channel_actor_state(state);

                            let _ = rpc_reply.send(Ok(()));
                            Ok(())
                        }
                        None => Err(Error::ChannelNotFound(channel_id)),
                    }
                }
            }
            _ => match self.channels.get(&channel_id) {
                Some(actor) => {
                    actor.send_message(ChannelActorMessage::Command(command))?;
                    Ok(())
                }
                None => {
                    // if it's relay remove tlc, insert it into ChannelActorState's retryable queue
                    if let ChannelCommand::RemoveTlc(remove_tlc, _) = &command {
                        if let Some(mut state) = self.store.get_channel_actor_state(&channel_id) {
                            if matches!(
                                state.state,
                                ChannelState::ChannelReady | ChannelState::ShuttingDown(_)
                            ) {
                                if let RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                                    payment_preimage,
                                }) = remove_tlc.reason
                                {
                                    if let Some(tlc) =
                                        state.tlc_state.get(&TLCId::Received(remove_tlc.id))
                                    {
                                        let payment_hash = tlc.payment_hash;
                                        self.store.insert_preimage(payment_hash, payment_preimage);
                                        self.network
                                            .send_message(NetworkActorMessage::new_notification(
                                                NetworkServiceEvent::PreimageCreated(
                                                    payment_hash,
                                                    payment_preimage,
                                                ),
                                            ))
                                            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                                    }
                                }

                                let operation = RetryableTlcOperation::RemoveTlc(
                                    TLCId::Received(remove_tlc.id),
                                    remove_tlc.reason.clone(),
                                );
                                state.retryable_tlc_operations.push_back(operation);
                                self.store.insert_channel_actor_state(state);
                            }
                        }
                    }

                    let error = Error::ChannelNotFound(channel_id);
                    if let Some(rpc_reply) = command.rpc_reply_port() {
                        let _ = rpc_reply.send(Err(error.to_string()));
                    }
                    Err(error)
                }
            },
        }
    }

    async fn reestablish_channel(
        &mut self,
        peer_id: &PeerId,
        channel_id: Hash256,
    ) -> Result<ActorRef<ChannelActorMessage>, Error> {
        if let Some(actor) = self.channels.get(&channel_id) {
            debug!(
                "Channel {:x} already exists, skipping reestablishment",
                &channel_id
            );
            return Ok(actor.clone());
        }

        if let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) {
            // this function is also called from `send_message_to_channel_actor`,
            // which may happened when peer received a message from a channel that is not in the channel map.
            // we should not restart the channel actor in a closed state.
            if channel_actor_state.is_closed() {
                return Err(Error::ChannelError(ProcessingChannelError::InvalidState(
                    format!("Channel {:x} is already closed", &channel_id),
                )));
            }
        } else {
            return Err(Error::ChannelNotFound(channel_id));
        }

        let remote_pubkey =
            self.get_peer_pubkey(peer_id)
                .ok_or(ProcessingChannelError::InvalidState(format!(
                    "Peer {:?}'s pubkey not found, this should never happen",
                    &peer_id
                )))?;

        debug!("Reestablishing channel {:x}", &channel_id);
        let (channel, _) = Actor::spawn_linked(
            Some(generate_channel_actor_name(&self.peer_id, peer_id)),
            ChannelActor::new(
                self.get_public_key(),
                remote_pubkey,
                self.network.clone(),
                self.store.clone(),
            ),
            ChannelInitializationParameter {
                operation: ChannelInitializationOperation::ReestablishChannel(channel_id),
                ephemeral_config: self.channel_ephemeral_config.clone(),
                private_key: self.private_key.clone(),
            },
            self.network.get_cell(),
        )
        .await?;
        info!("channel {:x} reestablished successfully", &channel_id);
        self.on_channel_created(channel_id, peer_id, channel.clone());

        Ok(channel)
    }

    async fn on_peer_connected(
        &mut self,
        remote_peer_id: &PeerId,
        remote_pubkey: Pubkey,
        session: &SessionContext,
    ) {
        debug!("Peer {remote_peer_id:?} connected");
        self.peer_session_map.insert(
            remote_peer_id.clone(),
            ConnectedPeer {
                session_id: session.id,
                session_type: session.ty,
                pubkey: remote_pubkey,
                address: session.address.clone(),
                features: None,
            },
        );
        if self
            .state_to_be_persisted
            .save_peer_pubkey(remote_peer_id.clone(), remote_pubkey)
        {
            self.persist_state();
        }

        if self.auto_announce {
            let message = self.get_or_create_new_node_announcement_message();
            debug!(
                "Auto announcing our node to peer {:?} (message: {:?})",
                remote_peer_id, &message
            );
            let _ = self.network.send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::BroadcastMessages(vec![
                    BroadcastMessageWithTimestamp::NodeAnnouncement(message),
                ]),
            ));
        } else {
            debug!(
                "Auto announcing is disabled, skipping node announcement to peer {:?}",
                remote_peer_id
            );
        }

        // send Init message to the peer
        self.send_fiber_message_to_peer(
            remote_peer_id,
            FiberMessage::init(Init {
                features: self.features.clone(),
                chain_hash: get_chain_hash(),
            }),
        )
        .await
        .expect("send Init message to peer must succeed");

        let remote_peer_id = remote_peer_id.clone();
        let session_id = session.id;
        self.network.send_after(CHECK_PEER_INIT_INTERVAL, move || {
            NetworkActorMessage::new_command(NetworkActorCommand::CheckPeerInit(
                remote_peer_id,
                session_id,
            ))
        });
    }

    fn on_peer_disconnected(&mut self, id: &PeerId) {
        debug!("Peer {id:?} disconnected");
        if let Some(peer) = self.peer_session_map.remove(id) {
            if let Some(channel_ids) = self.session_channels_map.remove(&peer.session_id) {
                for channel_id in channel_ids {
                    if let Some(channel) = self.channels.get(&channel_id) {
                        let _ = channel.send_message(ChannelActorMessage::Event(
                            ChannelEvent::Stop(StopReason::PeerDisConnected),
                        ));
                    }
                }
            }
        }

        // Also fail any inbound pending channels from this peer that are still waiting for
        // local acceptance (not yet in self.channels, no channel actor).
        let failed_channels: Vec<Hash256> = self
            .to_be_accepted_channels
            .map
            .iter()
            .filter(|(_, (peer_id, _))| peer_id == id)
            .map(|(channel_id, _)| *channel_id)
            .collect();
        for channel_id in failed_channels {
            if let Some(mut record) = self.store.get_channel_open_record(&channel_id) {
                record.fail("Peer disconnected during channel opening".to_string());
                self.store.insert_channel_open_record(record);
            }
            self.to_be_accepted_channels.remove(&channel_id);
        }
    }

    pub(crate) fn get_peer_addresses(&self, peer_id: &PeerId) -> HashSet<Multiaddr> {
        self.get_peer_pubkey(peer_id)
            .and_then(|pk| self.store.get_latest_node_announcement(&pk))
            .map(|a| a.addresses)
            .unwrap_or_default()
            .into_iter()
            .chain(self.state_to_be_persisted.get_peer_addresses(peer_id))
            .collect()
    }

    pub(crate) fn save_peer_address(&mut self, peer_id: PeerId, address: Multiaddr) -> bool {
        if self
            .state_to_be_persisted
            .save_peer_address(peer_id, address)
        {
            self.persist_state();
            true
        } else {
            false
        }
    }

    fn persist_state(&self) {
        self.store
            .insert_network_actor_state(&self.peer_id, self.state_to_be_persisted.clone());
    }

    fn on_channel_created(
        &mut self,
        id: Hash256,
        peer_id: &PeerId,
        actor: ActorRef<ChannelActorMessage>,
    ) {
        if let Some(session) = self.get_peer_session(peer_id) {
            self.channels.insert(id, actor.clone());
            self.session_channels_map
                .entry(session)
                .or_default()
                .insert(id);
        }
        debug!("Channel {:x} created", &id);
        // Notify outside observers.
        self.network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::ChannelCreated(peer_id.clone(), id),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
    }

    async fn on_closing_transaction_pending(
        &mut self,
        channel_id: Hash256,
        peer_id: PeerId,
        transaction: TransactionView,
        force: bool,
    ) {
        let tx_hash: Byte32 = transaction.hash();
        let force_flag = if force { "forcefully" } else { "cooperatively" };
        info!(
            "Channel ({:?}) to peer {:?} is closed {:?}. Broadcasting closing transaction ({:?}) now.",
            &channel_id, &peer_id, &tx_hash, force_flag
        );
        if let Err(err) = self
            .send_tx(
                transaction,
                InFlightCkbTxKind::Closing(peer_id, channel_id, force),
            )
            .await
        {
            error!("failed to send closing tx: {}", err);
        }
    }

    async fn on_closing_transaction_confirmed(
        &mut self,
        peer_id: &PeerId,
        channel_id: &Hash256,
        tx_hash: Byte32,
        force: bool,
        close_by_us: bool,
    ) {
        match self.channels.get(channel_id) {
            Some(channel_actor) => {
                let _ = channel_actor.send_message(ChannelActorMessage::Event(
                    ChannelEvent::ClosingTransactionConfirmed(tx_hash.unpack(), force, close_by_us),
                ));
            }
            None => {
                debug!("Channel {channel_id} actor is exit, try to update channel state");
                // channel is already exit, we should not try to reestablish channel since we
                // received a close transaction, so we just update channel actor state
                if let Some(mut state) = self.store.get_channel_actor_state(channel_id) {
                    // setup required field:
                    state.network = Some(self.network.clone());
                    state.private_key = Some(self.private_key.clone());
                    match state
                        .update_close_transaction_confirmed(tx_hash.unpack(), force, close_by_us)
                        .await
                    {
                        Ok(_) => {
                            self.store.insert_channel_actor_state(state);
                        }
                        Err(err) => {
                            error!("failed to update_close_transaction_confirmed {err:?}");
                        }
                    }
                }
            }
        }

        if let Some(session) = self.get_peer_session(peer_id) {
            if let Some(set) = self.session_channels_map.get_mut(&session) {
                set.remove(channel_id);
            }
        }
        if !force {
            // Notify outside observers.
            self.network
                .send_message(NetworkActorMessage::new_notification(
                    NetworkServiceEvent::ChannelClosed(
                        peer_id.clone(),
                        *channel_id,
                        tx_hash.clone(),
                    ),
                ))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        }
    }

    async fn on_channel_actor_stopped(&mut self, channel_id: Hash256, reason: StopReason) {
        // all check passed, now begin to remove from memory and DB
        self.channels.remove(&channel_id);
        self.channels_funding_lock_script_cache.remove(&channel_id);
        for (_peer_id, connected_peer) in self.peer_session_map.iter() {
            if let Some(session_channels) = self
                .session_channels_map
                .get_mut(&connected_peer.session_id)
            {
                session_channels.remove(&channel_id);
            }
        }

        if reason == StopReason::Abandon || reason == StopReason::AbortFunding {
            if let Some(channel_actor_state) = self.store.get_channel_actor_state(&channel_id) {
                // remove from transaction track actor
                if let Some(funding_tx) = channel_actor_state.funding_tx.as_ref() {
                    self.chain_actor
                        .send_message(CkbChainMessage::RemoveFundingTx(
                            funding_tx.calc_tx_hash().into(),
                        ))
                        .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW);
                }
                self.store.delete_channel_actor_state(&channel_id);
            }
            // notify event observers, such as remove from watchtower
            self.network
                .send_message(NetworkActorMessage::new_notification(
                    if reason == StopReason::Abandon {
                        NetworkServiceEvent::ChannelAbandon(channel_id)
                    } else {
                        NetworkServiceEvent::ChannelFundingAborted(channel_id)
                    },
                ))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        }

        self.to_be_accepted_channels.remove(&channel_id);
        if let Some((outpoint, _)) = self
            .outpoint_channel_map
            .iter()
            .find(|(_, id)| *id == &channel_id)
        {
            self.pending_channels.remove(outpoint);
        }
        self.outpoint_channel_map.retain(|_, id| *id != channel_id);
    }

    pub async fn on_init_msg(
        &mut self,
        _myself: ActorRef<NetworkActorMessage>,
        peer_id: PeerId,
        init_msg: Init,
    ) -> ProcessingChannelResult {
        if !self.is_connected(&peer_id) {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Peer {:?} is not connected",
                &peer_id
            )));
        }

        check_chain_hash(&init_msg.chain_hash).map_err(|e| {
            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::DisconnectPeer(
                        peer_id.clone(),
                        PeerDisconnectReason::ChainHashMismatch,
                    ),
                ))
                .expect(ASSUME_NETWORK_MYSELF_ALIVE);

            error!(
                "chain hash mismatch with peer {:?}: {:?}, disconnect now...",
                &peer_id, e
            );
            ProcessingChannelError::InvalidParameter(e.to_string())
        })?;

        if let Some(info) = self.peer_session_map.get_mut(&peer_id) {
            info.features = Some(init_msg.features);
            debug_event!(_myself, "PeerInit");

            for channel_id in self.store.get_active_channel_ids_by_peer(&peer_id) {
                if let Err(e) = self.reestablish_channel(&peer_id, channel_id).await {
                    error!("Failed to reestablish channel {:x}: {:?}", &channel_id, &e);
                }
            }
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Peer {:?} session not found",
                &peer_id
            )));
        }

        Ok(())
    }

    pub async fn on_open_channel_msg(
        &mut self,
        peer_id: PeerId,
        open_channel: OpenChannel,
    ) -> ProcessingChannelResult {
        let id = open_channel.channel_id;
        let remote_funding_amount = open_channel.funding_amount;
        let result = check_open_channel_parameters(
            &open_channel.funding_udt_type_script,
            &open_channel.shutdown_script,
            open_channel.reserved_ckb_amount,
            open_channel.funding_fee_rate,
            open_channel.commitment_fee_rate,
            open_channel.commitment_delay_epoch,
            open_channel.max_tlc_number_in_flight,
        )
        .and_then(|_| {
            self.to_be_accepted_channels
                .try_insert(id, peer_id.clone(), open_channel)
        });

        match result {
            Ok(_) => {
                // Create a persistent record so the accepting side can see this pending channel
                // via list_channels(only_pending=true) and across node restarts.
                let record =
                    ChannelOpenRecord::new_inbound(id, peer_id.clone(), remote_funding_amount);
                self.store.insert_channel_open_record(record);

                // Notify outside observers.
                self.network
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, id),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            Err(ProcessingChannelError::RepeatedProcessing(_)) => {
                // ignore duplicated open channel request
            }
            Err(_) => {
                debug_event!(self.network, "ChannelPendingToBeRejected");
            }
        };

        result
    }

    async fn on_funding_transaction_pending(
        &mut self,
        channel_id: Hash256,
        transaction: Transaction,
        outpoint: OutPoint,
    ) {
        // Just a sanity check to ensure that no two channels are associated with the same outpoint.
        if let Some(old) = self.pending_channels.remove(&outpoint) {
            if old != channel_id {
                panic!("Trying to associate a new channel id {:?} with the same outpoint {:?} when old channel id is {:?}. Rejecting.", channel_id, outpoint, old);
            }
        }
        self.pending_channels.insert(outpoint.clone(), channel_id);
        let transaction = transaction.into_view();
        let tx_hash: Byte32 = transaction.hash();
        debug!(
            "Funding transaction (outpoint {:?}) for channel {:?} is now ready. Broadcast it {:?} now.",
            &outpoint, &channel_id, &tx_hash
        );

        if let Err(err) = self
            .send_tx(transaction, InFlightCkbTxKind::Funding(channel_id))
            .await
        {
            error!("failed to send funding tx: {}", err);
        }
    }

    async fn on_funding_transaction_confirmed(
        &mut self,
        outpoint: OutPoint,
        block_hash: H256,
        tx_index: u32,
        timestamp: u64,
    ) {
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
            None,
            ChannelActorMessage::Event(ChannelEvent::FundingTransactionConfirmed(
                block_hash, tx_index, timestamp,
            )),
        )
        .await;
    }

    async fn on_payment_actor_stopped(&mut self, payment_hash: Hash256) {
        debug!("Payment actor stopped {payment_hash}");
        if self.inflight_payments.remove(&payment_hash).is_none() {
            error!("Can't find inflight payment actor");
        }

        // If this payment has associated previous TLCs,
        // meaning it's a trampoline forwarding payment,
        // we need to resolve those upstream TLCs based on the payment outcome.
        let Some(session) = self.store.get_payment_session(payment_hash) else {
            return;
        };
        let trampoline_context = session.request.trampoline_context.as_ref();

        if let Some(context) = trampoline_context {
            match session.status {
                PaymentStatus::Success => {
                    let preimage = session
                        .attempts()
                        .find(|a| a.is_success())
                        .and_then(|a| a.preimage);

                    if let Some(preimage) = preimage {
                        self.store.insert_preimage(payment_hash, preimage);
                        for prev_tlc in &context.previous_tlcs {
                            let (send, _recv) = oneshot::channel();
                            let rpc_reply = RpcReplyPort::from(send);
                            let command = ChannelCommand::RemoveTlc(
                                RemoveTlcCommand {
                                    id: prev_tlc.prev_tlc_id,
                                    reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                                        payment_preimage: preimage,
                                    }),
                                },
                                rpc_reply,
                            );
                            if let Err(e) = self
                                .send_command_to_channel(prev_tlc.prev_channel_id, command)
                                .await
                            {
                                error!("Failed to send fulfillment to upstream channel: {:?}", e);
                            }
                        }
                    } else {
                        error!("Payment success but no preimage found for {payment_hash}");
                    }
                }
                PaymentStatus::Failed => {
                    let error_code = session
                        .last_error_code
                        .unwrap_or(TlcErrorCode::TemporaryNodeFailure);
                    for prev_tlc in &context.previous_tlcs {
                        let (send, _recv) = oneshot::channel();
                        let rpc_reply = RpcReplyPort::from(send);
                        let shared_secret = prev_tlc.shared_secret.unwrap_or([0u8; 32]);
                        let command = ChannelCommand::RemoveTlc(
                            RemoveTlcCommand {
                                id: prev_tlc.prev_tlc_id,
                                reason: RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                                    TlcErr::new(error_code),
                                    &shared_secret,
                                )),
                            },
                            rpc_reply,
                        );
                        if let Err(e) = self
                            .send_command_to_channel(prev_tlc.prev_channel_id, command)
                            .await
                        {
                            error!("Failed to send failure to upstream channel: {:?}", e);
                        }
                    }
                }
                _ => {
                    warn!("Trampoline payment stopped with unknown state for {payment_hash}");
                }
            }
        }
    }

    async fn send_message_to_channel_actor(
        &mut self,
        channel_id: Hash256,
        // Sometimes we need to know the peer id in order to send the message to the channel actor.
        peer_id: Option<&PeerId>,
        message: ChannelActorMessage,
    ) {
        match self.channels.get(&channel_id) {
            None => match (message, peer_id) {
                // TODO: ban the adversary who constantly send messages related to non-existing channels.
                (
                    ChannelActorMessage::PeerMessage(FiberChannelMessage::ReestablishChannel(r)),
                    Some(remote_peer_id),
                ) if self.store.get_channel_actor_state(&channel_id).is_some() => {
                    debug!("Received a ReestablishChannel message for channel {:?} which has persisted state, but no corresponding channel actor, starting it now", &channel_id);
                    match self.reestablish_channel(remote_peer_id, channel_id).await {
                        Ok(actor) => {
                            actor
                                .send_message(ChannelActorMessage::PeerMessage(
                                    FiberChannelMessage::ReestablishChannel(r),
                                ))
                                .expect("channel actor alive");
                        }
                        Err(e) => {
                            error!("Failed to reestablish channel {:x}: {:?}", &channel_id, &e);
                        }
                    }
                }
                (message, _) => {
                    error!(
                            "Failed to send message to channel actor: channel {:?} not found, message: {:?}",
                            &channel_id, &message,
                        );
                }
            },
            Some(actor) => {
                // There is a possibility that the channel actor is not alive, but we assume it is
                // alive for this moment. For example, in force shutdown case, the ChannelActor received
                // ClosingTransactionConfirmed event then stopped after processing the message,
                // NetworkActor will remove it from `channels` when receiving ChannelActorStopped from it,
                // but at the same time, NetworkActor received another ClosingTransactionConfirmed,
                // we will try to send another event message to the stopped ChannelActor here.
                //
                // In short, it's safer to ignore sending message failure from NetworkActor
                // to ChannelActor, since NetworkActor is responsible for multiple channels and a lot of stuff.
                let _ = actor.send_message(message);
            }
        }
    }

    fn get_cached_channel_funding_lock_script(
        &mut self,
        channel_id: Hash256,
        state: &ChannelActorState,
    ) -> Script {
        if self.channels.contains_key(&channel_id) {
            self.channels_funding_lock_script_cache
                .entry(channel_id)
                .or_insert_with(|| state.get_funding_lock_script())
                .to_owned()
        } else {
            // To prevent potential memory leak, we do not cache this branch
            tracing::warn!("Get funding lock script for unknown channel {channel_id:?}");
            state.get_funding_lock_script()
        }
    }
}

pub struct NetworkActorStartArguments {
    pub config: FiberConfig,
    pub tracker: TaskTracker,
    pub default_shutdown_script: Script,
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<S, C> Actor for NetworkActor<S, C>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + ChannelOpenRecordStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    type Msg = NetworkActorMessage;
    type State = NetworkActorState<S, C>;
    type Arguments = NetworkActorStartArguments;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let NetworkActorStartArguments {
            config,
            #[cfg(not(target_arch = "wasm32"))]
            tracker,
            default_shutdown_script,
            ..
        } = args;
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let private_key: Privkey = <[u8; 32]>::try_from(kp.as_ref())
            .expect("valid length for key")
            .into();
        let mut entropy_rand = [0u8; 32];
        getrandom(&mut entropy_rand).expect("getrandom should not fail");
        let entropy = blake2b_hash_with_salt(
            [kp.as_ref(), entropy_rand.as_slice()].concat().as_slice(),
            b"FIBER_NETWORK_ENTROPY",
        );
        let secio_kp = SecioKeyPair::from(kp);
        let secio_pk = secio_kp.public_key();
        let my_peer_id: PeerId = PeerId::from(secio_pk);
        let handle = NetworkServiceHandle::new(myself.clone());
        let fiber_handle = FiberProtocolHandle::from(&handle);

        // Conditionally start GossipService based on sync_network_graph config
        let (gossip_actor, gossip_handle_opt) = if config.sync_network_graph() {
            let mut gossip_config = GossipConfig::from(&config);
            gossip_config.peer_id = Some(my_peer_id.clone());
            let (gossip_service, gossip_handle) = GossipService::start(
                gossip_config,
                self.store.clone(),
                self.chain_actor.clone(),
                self.chain_client.clone(),
                myself.get_cell(),
            )
            .await;

            let graph_subscribing_cursor = {
                let graph = self.network_graph.write().await;
                graph
                    .get_latest_cursor()
                    .go_back_for_some_time(MAX_GRAPH_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT)
            };

            gossip_service
                .get_subscriber()
                .subscribe(graph_subscribing_cursor, myself.clone(), |m| {
                    Some(NetworkActorMessage::new_event(
                        NetworkActorEvent::GossipMessageUpdates(m),
                    ))
                })
                .await
                .expect("subscribe to gossip store updates");
            (Some(gossip_handle.actor().clone()), Some(gossip_handle))
        } else {
            info!("Gossip network synchronization is disabled (sync_network_graph = false)");
            (None, None)
        };

        // Build service with or without gossip protocol based on configuration
        #[cfg(not(target_arch = "wasm32"))]
        let mut service = {
            let mut builder = ServiceBuilder::default()
                .insert_protocol(fiber_handle.create_meta())
                .handshake_type(secio_kp.into());
            if let Some(gossip_handle) = gossip_handle_opt {
                builder = builder.insert_protocol(gossip_handle.create_meta());
            }
            builder.build(handle)
        };
        #[cfg(target_arch = "wasm32")]
        let mut service = {
            let mut builder = ServiceBuilder::default()
                .insert_protocol(fiber_handle.create_meta())
                .handshake_type(secio_kp.into())
                // Sets forever to true so the network service won't be shutdown due to no incoming connections
                .forever(true);
            if let Some(gossip_handle) = gossip_handle_opt {
                builder = builder.insert_protocol(gossip_handle.create_meta());
            }
            builder.build(handle)
        };

        let mut announced_addrs = Vec::with_capacity(config.announced_addrs.len() + 1);

        #[cfg(not(target_arch = "wasm32"))]
        let listening_addr = {
            let mut addresses_to_listen = vec![MultiAddr::from_str(config.listening_addr())
                .expect("valid tentacle listening address")];
            if config.reuse_port_for_websocket {
                // Re-use the same port for websocket
                let ws_listens = addresses_to_listen
                    .iter()
                    .cloned()
                    .filter_map(|mut addr| {
                        if matches!(find_type(&addr), TransportType::Tcp) {
                            addr.push(Protocol::Ws);
                            Some(addr)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                addresses_to_listen.extend(ws_listens);
            }
            let mut listening_addr = vec![];
            for addr in addresses_to_listen.into_iter() {
                let mut current_addr = service.listen(addr).await.expect("listen tentacle");

                current_addr.push(Protocol::P2P(Cow::Owned(my_peer_id.clone().into_bytes())));
                if config.announce_listening_addr() {
                    announced_addrs.push(current_addr.clone());
                }
                listening_addr.push(current_addr);
            }

            listening_addr
        };
        #[cfg(target_arch = "wasm32")]
        // There is no listening_addr on wasm, since it can't listen to anything
        let listening_addr = vec![];
        for announced_addr in &config.announced_addrs {
            let mut multiaddr =
                MultiAddr::from_str(announced_addr.as_str()).expect("valid announced listen addr");
            match multiaddr.pop() {
                Some(Protocol::P2P(c)) => {
                    // If the announced listen addr has a peer id, it must match our peer id.
                    if c.as_ref() != my_peer_id.as_bytes() {
                        panic!("Announced listen addr is using invalid peer id: announced addr {}, actual peer id {:?}", announced_addr, my_peer_id);
                    }
                }
                Some(component) => {
                    // Push this unrecognized component back to the multiaddr.
                    multiaddr.push(component);
                }
                None => {
                    // Should never happen
                }
            }
            // Push our peer id to the multiaddr.
            multiaddr.push(Protocol::P2P(Cow::Owned(my_peer_id.clone().into_bytes())));
            announced_addrs.push(multiaddr);
        }

        if !config.announce_private_addr.unwrap_or_default() {
            announced_addrs.retain(|addr| {
                multiaddr_to_socketaddr(addr)
                    .map(|socket_addr| is_reachable(socket_addr.ip()))
                    .unwrap_or_default()
            });
        }
        #[cfg(not(target_arch = "wasm32"))]
        info!(
            "Started listening tentacle on {:?}, peer id {:?}, announced addresses {:?}",
            &listening_addr, &my_peer_id, &announced_addrs
        );

        #[cfg(target_arch = "wasm32")]
        info!(
            "Started fiber network service peer id {:?}, announced addresses {:?}",
            &my_peer_id, &announced_addrs
        );
        let control = service.control().to_owned();
        myself
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::NetworkStarted(
                    my_peer_id.clone(),
                    listening_addr.clone(),
                    announced_addrs.clone(),
                ),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);

        #[cfg(not(target_arch = "wasm32"))]
        tracker.spawn(async move {
            service.run().await;
            debug!("Tentacle service stopped");
        });
        #[cfg(target_arch = "wasm32")]
        ractor::concurrency::spawn(async move {
            service.run().await;
            debug!("Tentacle service stopped");
        });
        let mut state_to_be_persisted = self
            .store
            .get_network_actor_state(&my_peer_id)
            .unwrap_or_default();

        for bootnode in &config.bootnode_addrs {
            let addr = Multiaddr::from_str(bootnode.as_str()).expect("valid bootnode");
            let peer_id = extract_peer_id(&addr).expect("valid peer id");
            state_to_be_persisted.save_peer_address(peer_id, addr);
        }

        let chain_actor = self.chain_actor.clone();
        let features = config.gen_node_features();

        let mut state = NetworkActorState {
            store: self.store.clone(),
            state_to_be_persisted,
            node_name: config.announced_node_name,
            peer_id: my_peer_id,
            announced_addrs,
            auto_announce: config.auto_announce_node(),
            last_node_announcement_message: None,
            private_key,
            entropy,
            default_shutdown_script,
            network: myself.clone(),
            control,
            peer_session_map: Default::default(),
            session_channels_map: Default::default(),
            channels: Default::default(),
            outpoint_channel_map: Default::default(),
            channels_funding_lock_script_cache: Default::default(),
            to_be_accepted_channels: ToBeAcceptedChannels::new_with_config(&config),
            pending_channels: Default::default(),
            chain_actor,
            chain_client: self.chain_client.clone(),
            open_channel_auto_accept_min_ckb_funding_amount: config
                .open_channel_auto_accept_min_ckb_funding_amount(),
            auto_accept_channel_ckb_funding_amount: config.auto_accept_channel_ckb_funding_amount(),
            tlc_expiry_delta: config.tlc_expiry_delta(),
            tlc_min_value: config.tlc_min_value(),
            tlc_fee_proportional_millionths: config.tlc_fee_proportional_millionths(),
            gossip_actor,
            max_inbound_peers: config.max_inbound_peers(),
            min_outbound_peers: config.min_outbound_peers(),
            features,
            channel_ephemeral_config: ChannelEphemeralConfig {
                funding_timeout_seconds: config.funding_timeout_seconds,
            },
            inflight_payments: Default::default(),
        };

        let node_announcement = state.get_or_create_new_node_announcement_message();
        {
            let mut graph = self.network_graph.write().await;
            graph.process_node_announcement(node_announcement);
        }
        let announce_node_interval_seconds = config.announce_node_interval_seconds();
        if announce_node_interval_seconds > 0 {
            myself.send_interval(Duration::from_secs(announce_node_interval_seconds), || {
                NetworkActorMessage::new_command(NetworkActorCommand::BroadcastLocalInfo(
                    LocalInfoKind::NodeAnnouncement,
                ))
            });
        }

        // Save bootnodes to the network actor state.
        state.persist_state();

        Ok(state)
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // MAINTAINING_CONNECTIONS_INTERVAL is long, we need to trigger when start
        myself
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::MaintainConnections,
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        myself.send_interval(MAINTAINING_CONNECTIONS_INTERVAL, || {
            NetworkActorMessage::new_command(NetworkActorCommand::MaintainConnections)
        });
        myself.send_interval(CHECK_CHANNELS_INTERVAL, || {
            NetworkActorMessage::new_command(NetworkActorCommand::CheckChannels)
        });
        myself.send_interval(CHECK_CHANNELS_SHUTDOWN_INTERVAL, || {
            NetworkActorMessage::new_command(NetworkActorCommand::CheckChannelsShutdown)
        });

        // Trigger mmp tlc set fulfill check and hold tlc timeout
        let now = now_timestamp_as_millis_u64();
        for (payment_hash, hold_tlcs) in self.store.get_node_hold_tlcs() {
            // timeout hold tlc
            let already_timeout = hold_tlcs
                .iter()
                .any(|hold_tlc| now >= hold_tlc.hold_expire_at);
            if !already_timeout {
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SettleHoldTlcSet(payment_hash),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }
        debug_event!(myself, "network actor started");
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _handle_log_guard = ActorHandleLogGuard::new(
            "NetworkActor",
            message.to_string(),
            "fiber.network_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            NetworkActorMessage::Event(event) => {
                if let Err(err) = self.handle_event(myself, state, event).await {
                    error!("Failed to handle fiber network event: {}", err);
                }
            }
            NetworkActorMessage::Command(command) => {
                if let Err(err) = self.handle_command(myself, state, command).await {
                    error!("Failed to handle fiber network command: {}", err);
                }
            }
            NetworkActorMessage::Notification(event) => {
                if let Err(err) = self.event_sender.send(event).await {
                    error!("Failed to notify outside observers: {}", err);
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        myself
            .get_cell()
            .stop_children_and_wait(Some("Network actor stopped".to_string()), None)
            .await;

        if let Err(err) = state.control.close().await {
            error!("Failed to close tentacle service: {}", err);
        }
        debug!("Saving network actor state for {:?}", state.peer_id);
        state.persist_state();
        debug!("Network service for {:?} shutdown", state.peer_id);
        // The event receiver may have been closed already.
        // We ignore the error here.
        let _ = self
            .event_sender
            .send(NetworkServiceEvent::NetworkStopped(state.peer_id.clone()))
            .await;
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorTerminated(who, _state, reason) => {
                debug!("Actor {:?} terminated with reason {:?}", who, reason);
            }
            SupervisionEvent::ActorFailed(who, err) => {
                panic!("Actor unexpectedly panicked (id: {:?}): {:?}", who, err);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct FiberProtocolHandle {
    actor: ActorRef<NetworkActorMessage>,
}

impl FiberProtocolHandle {
    fn create_meta(self) -> ProtocolMeta {
        MetaBuilder::new()
            .id(FIBER_PROTOCOL_ID)
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

#[async_trait]
impl ServiceProtocol for FiberProtocolHandle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, _version: &str) {
        let _session = context.session;
        if let Some(remote_pubkey) = context.session.remote_pubkey.clone() {
            let remote_peer_id = PeerId::from_public_key(&remote_pubkey);
            try_send_actor_message(
                &self.actor,
                NetworkActorMessage::new_event(NetworkActorEvent::PeerConnected(
                    remote_peer_id,
                    remote_pubkey.into(),
                    context.session.clone(),
                )),
            );
        } else {
            warn!("Peer connected without remote pubkey {:?}", context.session);
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        match context.session.remote_pubkey.as_ref() {
            Some(pubkey) => {
                let peer_id = PeerId::from_public_key(pubkey);
                try_send_actor_message(
                    &self.actor,
                    NetworkActorMessage::new_event(NetworkActorEvent::PeerDisconnected(
                        peer_id,
                        context.session.clone(),
                    )),
                );
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
                try_send_actor_message(
                    &self.actor,
                    NetworkActorMessage::new_event(NetworkActorEvent::FiberMessage(peer_id, msg)),
                );
            }
            None => {
                unreachable!("Received message without remote pubkey");
            }
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[derive(Clone, Debug)]
struct NetworkServiceHandle {
    actor: ActorRef<NetworkActorMessage>,
}

impl NetworkServiceHandle {
    fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        NetworkServiceHandle { actor }
    }
}

impl From<&NetworkServiceHandle> for FiberProtocolHandle {
    fn from(handle: &NetworkServiceHandle) -> Self {
        FiberProtocolHandle {
            actor: handle.actor.clone(),
        }
    }
}

#[async_trait]
impl ServiceHandle for NetworkServiceHandle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        debug!("Service error: {:?}", error);
        // TODO
        // ServiceError::DialerError => remove address from peer store
        // ServiceError::ProtocolError => ban peer
    }

    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        debug!("Service event: {:?}", event);
    }
}

// If we are closing the whole network service, we may have already stopped the network actor.
// In that case the send_message will fail.
// Ideally, we should close tentacle network service first, then stop the network actor.
// But ractor provides only api for `post_stop` instead of `pre_stop`.
fn try_send_actor_message(actor: &ActorRef<NetworkActorMessage>, message: NetworkActorMessage) {
    let _ = actor.send_message(message);
}

#[allow(clippy::too_many_arguments)]
pub async fn start_network<
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + ChannelOpenRecordStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
>(
    config: FiberConfig,
    chain_client: C,
    chain_actor: ActorRef<CkbChainMessage>,
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    tracker: TaskTracker,
    root_actor: ActorCell,
    store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    default_shutdown_script: Script,
) -> ActorRef<NetworkActorMessage> {
    let my_pubkey = config.public_key();
    let my_peer_id = PeerId::from_public_key(&my_pubkey);

    let (actor, _handle) = Actor::spawn_linked(
        Some(format!("Network {}", my_peer_id)),
        NetworkActor::new(
            event_sender,
            chain_actor,
            store,
            network_graph,
            chain_client,
        ),
        NetworkActorStartArguments {
            config,
            tracker,
            default_shutdown_script,
        },
        root_actor,
    )
    .await
    .expect("Failed to start network actor");

    actor
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn find_type(addr: &Multiaddr) -> TransportType {
    let mut iter = addr.iter();

    iter.find_map(|proto| match proto {
        Protocol::Ws => Some(TransportType::Ws),
        Protocol::Wss => Some(TransportType::Wss),
        _ => None,
    })
    .unwrap_or(TransportType::Tcp)
}

struct ToBeAcceptedChannels {
    total_number_limit: usize,
    total_bytes_limit: usize,
    map: HashMap<Hash256, (PeerId, OpenChannel)>,
}

impl Default for ToBeAcceptedChannels {
    fn default() -> Self {
        Self {
            total_number_limit: usize::MAX,
            total_bytes_limit: usize::MAX,
            map: HashMap::default(),
        }
    }
}

// Remember to sync fiber/config.rs
const DEFAULT_TO_BE_ACCEPTED_CHANNELS_NUMBER_LIMIT: usize = 20;
// Remember to sync fiber/config.rs
const DEFAULT_TO_BE_ACCEPTED_CHANNELS_BYTES_LIMIT: usize = 51200; // 50KB

impl ToBeAcceptedChannels {
    fn new_with_config(config: &FiberConfig) -> Self {
        Self {
            total_number_limit: config
                .to_be_accepted_channels_number_limit
                .unwrap_or(DEFAULT_TO_BE_ACCEPTED_CHANNELS_NUMBER_LIMIT),
            total_bytes_limit: config
                .to_be_accepted_channels_bytes_limit
                .unwrap_or(DEFAULT_TO_BE_ACCEPTED_CHANNELS_BYTES_LIMIT),
            map: HashMap::default(),
        }
    }

    fn remove(&mut self, id: &Hash256) -> Option<(PeerId, OpenChannel)> {
        self.map.remove(id)
    }

    // insert and apply throttle control
    fn try_insert(
        &mut self,
        id: Hash256,
        peer_id: PeerId,
        open_channel: OpenChannel,
    ) -> ProcessingChannelResult {
        if let Some(existing_value) = self.map.get(&id) {
            let err_message = format!(
                "A channel from {:?} of id {:?} is already awaiting to be accepted",
                &peer_id, &id,
            );
            warn!("{}: {:?}", err_message, existing_value);
            return Err(ProcessingChannelError::RepeatedProcessing(err_message));
        }

        // The map should be small because of the flow control, so calculate the total number and
        // bytes on the fly.
        let (total_number, total_bytes) = self
            .map
            .values()
            .filter(|(saved_peer_id, _)| *saved_peer_id == peer_id)
            .fold(
                (1, open_channel.mem_size()),
                |(count, size), (_, saved_open_channel)| {
                    (count + 1, size + saved_open_channel.mem_size())
                },
            );

        if total_number > self.total_number_limit {
            return Err(ProcessingChannelError::ToBeAcceptedChannelsExceedLimit(
                format!("Total number exceeds the limit {}", self.total_number_limit),
            ));
        }
        if total_bytes > self.total_bytes_limit {
            return Err(ProcessingChannelError::ToBeAcceptedChannelsExceedLimit(
                format!("Total bytes exceeds the limit {}", self.total_bytes_limit),
            ));
        }

        debug!(
            "Channel from {:?} of id {:?} is now awaiting to be accepted: {:?}",
            &peer_id, &id, &open_channel
        );
        self.map.insert(id, (peer_id, open_channel));
        Ok(())
    }
}
