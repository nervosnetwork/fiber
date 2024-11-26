use ckb_hash::blake2b_256;
use ckb_jsonrpc_types::{BlockNumber, Status, TxStatus};
use ckb_types::core::{EpochNumberWithFraction, TransactionView};
use ckb_types::packed::{self, Byte32, CellOutput, OutPoint, Script, Transaction};
use ckb_types::prelude::{IntoTransactionView, Pack, Unpack};
use musig2::CompactSignature;
use once_cell::sync::OnceCell;
use ractor::concurrency::Duration;
use ractor::{
    async_trait as rasync_trait, call, call_t, Actor, ActorCell, ActorProcessingErr, ActorRef,
    RactorErr, RpcReplyPort, SupervisionEvent,
};
use rand::Rng;
use secp256k1::{Message, Secp256k1};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::RandomState;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use std::u64;
use tentacle::multiaddr::{MultiAddr, Protocol};
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
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, trace, warn};

use super::channel::{
    get_funding_and_reserved_amount, occupied_capacity, AcceptChannelParameter, ChannelActor,
    ChannelActorMessage, ChannelActorStateStore, ChannelCommand, ChannelCommandWithId,
    ChannelEvent, ChannelInitializationParameter, ChannelState, ChannelSubscribers,
    OpenChannelParameter, ProcessingChannelError, ProcessingChannelResult, PublicChannelInfo,
    ShuttingDownFlags, DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE, MAX_COMMITMENT_DELAY_EPOCHS,
    MIN_COMMITMENT_DELAY_EPOCHS, SYS_MAX_TLC_NUMBER_IN_FLIGHT,
};
use super::config::{AnnouncedNodeName, MIN_TLC_EXPIRY_DELTA};
use super::fee::calculate_commitment_tx_fee;
use super::graph::{NetworkGraph, NetworkGraphStateStore, SessionRoute};
use super::graph_syncer::{GraphSyncer, GraphSyncerMessage};
use super::key::blake2b_hash_with_salt;
use super::types::{
    ChannelAnnouncement, ChannelAnnouncementQuery, ChannelUpdate, ChannelUpdateQuery,
    EcdsaSignature, FiberBroadcastMessage, FiberBroadcastMessageQuery, FiberMessage,
    FiberQueryInformation, GetBroadcastMessages, GetBroadcastMessagesResult, Hash256,
    NodeAnnouncement, NodeAnnouncementQuery, OpenChannel, PaymentHopData, Privkey, Pubkey,
    QueryBroadcastMessagesWithinTimeRange, QueryBroadcastMessagesWithinTimeRangeResult,
    QueryChannelsWithinBlockRange, QueryChannelsWithinBlockRangeResult, RemoveTlcReason, TlcErr,
    TlcErrData, TlcErrPacket, TlcErrorCode,
};
use super::{FiberConfig, ASSUME_NETWORK_ACTOR_ALIVE};

use crate::ckb::config::UdtCfgInfos;
use crate::ckb::contracts::{check_udt_script, get_udt_whitelist, is_udt_type_auto_accept};
use crate::ckb::{CkbChainMessage, FundingRequest, FundingTx, TraceTxRequest, TraceTxResponse};
use crate::fiber::channel::{
    AddTlcCommand, AddTlcResponse, TxCollaborationCommand, TxUpdateCommand,
};
use crate::fiber::config::{DEFAULT_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT};
use crate::fiber::graph::{ChannelInfo, PaymentSession, PaymentSessionStatus};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::types::{
    secp256k1_instance, FiberChannelMessage, PaymentOnionPacket, PeeledPaymentOnionPacket,
    TxSignatures,
};
use crate::fiber::KeyPair;
use crate::invoice::{CkbInvoice, InvoiceStore};
use crate::{unwrap_or_return, Error};

pub const FIBER_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

pub const DEFAULT_CHAIN_ACTOR_TIMEOUT: u64 = 300000;

// tx index is not returned on older ckb version, using dummy tx index instead.
// Waiting for https://github.com/nervosnetwork/ckb/pull/4583/ to be released.
const DUMMY_FUNDING_TX_INDEX: u32 = 0;

// This is a temporary way to document that we assume the chain actor is always alive.
// We may later relax this assumption. At the moment, if the chain actor fails, we
// should panic with this message, and later we may find all references to this message
// to make sure that we handle the case where the chain actor is not alive.
const ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW: &str =
    "We currently assume that chain actor is always alive, but it failed. This is a known issue.";

const ASSUME_NETWORK_MYSELF_ALIVE: &str = "network actor myself alive";

// This is the default approximate number of peers that we need to keep connection to to make the
// network operating normally.
const NUM_PEER_CONNECTIONS: usize = 40;

// The duration for which we will try to maintain the number of peers in connection.
const MAINTAINING_CONNECTIONS_INTERVAL: Duration = Duration::from_secs(3600);

static CHAIN_HASH_INSTANCE: OnceCell<Hash256> = OnceCell::new();

pub fn init_chain_hash(chain_hash: Hash256) {
    CHAIN_HASH_INSTANCE
        .set(chain_hash)
        .expect("init_chain_hash should only be called once");
}

pub(crate) fn get_chain_hash() -> Hash256 {
    CHAIN_HASH_INSTANCE.get().cloned().unwrap_or_default()
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
    pub status: PaymentSessionStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub failed_error: Option<String>,
    pub fee: u128,
}

/// What kind of local information should be broadcasted to the network.
#[derive(Debug)]
pub enum LocalInfoKind {
    NodeAnnouncement,
}

#[derive(Debug, Clone)]
pub struct NodeInfoResponse {
    pub node_name: Option<AnnouncedNodeName>,
    pub peer_id: PeerId,
    pub public_key: Pubkey,
    pub addresses: Vec<MultiAddr>,
    pub chain_hash: Hash256,
    pub open_channel_auto_accept_min_ckb_funding_amount: u64,
    pub auto_accept_channel_ckb_funding_amount: u64,
    pub tlc_expiry_delta: u64,
    pub tlc_min_value: u128,
    pub tlc_max_value: u128,
    pub tlc_fee_proportional_millionths: u128,
    pub channel_count: u32,
    pub pending_channel_count: u32,
    pub peers_count: u32,
    pub network_sync_status: String,
    pub udt_cfg_infos: UdtCfgInfos,
}

/// The struct here is used both internally and as an API to the outside world.
/// If we want to send a reply to the caller, we need to wrap the message with
/// a RpcReplyPort. Since outsider users have no knowledge of RpcReplyPort, we
/// need to hide it from the API. So in case a reply is needed, we need to put
/// an optional RpcReplyPort in the of the definition of this message.
#[derive(Debug)]
pub enum NetworkActorCommand {
    /// Network commands
    // Connect to a peer, and optionally also save the peer to the peer store.
    ConnectPeer(Multiaddr),
    DisconnectPeer(PeerId),
    // Save the address of a peer to the peer store, the address here must be a valid
    // multiaddr with the peer id.
    SavePeerAddress(Multiaddr),
    // We need to maintain a certain number of peers connections to keep the network running.
    MaintainConnections(usize),
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
    // The first parameter is the peeled onion in binary via `PeeledOnionPacket::serialize`. `PeeledOnionPacket::current`
    // is for the current node.
    SendPaymentOnionPacket(
        SendOnionPacketCommand,
        RpcReplyPort<Result<u64, TlcErrPacket>>,
    ),
    PeelPaymentOnionPacket(
        Vec<u8>, // onion_packet
        Hash256, // payment_hash
        RpcReplyPort<Result<PeeledPaymentOnionPacket, String>>,
    ),
    UpdateChannelFunding(Hash256, Transaction, FundingRequest),
    SignTx(PeerId, Hash256, Transaction, Option<Vec<Vec<u8>>>),
    // A ChannelAnnouncement is ready to broadcast, we need to
    // update our network graph and broadcast it to the network.
    // The channel counterparty should definitely be part of the
    // nodes that are going to receive this message.
    ProcessChannelAnnouncement(PeerId, BlockNumber, u32, ChannelAnnouncement),
    // A ChannelUpdate is ready to broadcast, we need to update
    // our network graph and broadcast it to the network.
    // The channel counterparty should definitely be part of the
    // nodes that are going to receive this message.
    ProccessChannelUpdate(PeerId, ChannelUpdate),
    // Broadcast node/channel information to the network.
    BroadcastMessage(FiberBroadcastMessage),
    // Broadcast local information to the network.
    BroadcastLocalInfo(LocalInfoKind),
    SignMessage([u8; 32], RpcReplyPort<EcdsaSignature>),
    // Payment related commands
    SendPayment(
        SendPaymentCommand,
        RpcReplyPort<Result<SendPaymentResponse, String>>,
    ),
    // Get Payment Session for query payment status and errors
    GetPayment(Hash256, RpcReplyPort<Result<SendPaymentResponse, String>>),
    GetAndProcessChannelsWithinBlockRangeFromPeer(
        (PeerId, u64, u64),
        RpcReplyPort<Result<(u64, bool), Error>>,
    ),
    GetAndProcessBroadcastMessagesWithinTimeRangeFromPeer(
        (PeerId, u64, u64),
        RpcReplyPort<Result<(u64, bool), Error>>,
    ),
    StartSyncing,
    StopSyncing,
    MarkSyncingDone,
    NodeInfo((), RpcReplyPort<Result<NodeInfoResponse, String>>),
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
    pub shutdown_script: Option<Script>,
    pub funding_udt_type_script: Option<Script>,
    pub commitment_fee_rate: Option<u64>,
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,
    pub funding_fee_rate: Option<u64>,
    pub tlc_expiry_delta: Option<u64>,
    pub tlc_min_value: Option<u128>,
    pub tlc_max_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_tlc_number_in_flight: Option<u64>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentCommand {
    // the identifier of the payment target
    pub target_pubkey: Option<Pubkey>,
    // the amount of the payment
    pub amount: Option<u128>,
    // The hash to use within the payment's HTLC
    pub payment_hash: Option<Hash256>,
    // the encoded invoice to send to the recipient
    pub invoice: Option<String>,
    // the TLC expiry delta that should be used to set the timelock for the final hop
    pub final_tlc_expiry_delta: Option<u64>,
    // the TLC expiry for whole payment, in milliseconds
    pub tlc_expiry_limit: Option<u64>,
    // the payment timeout in seconds, if the payment is not completed within this time, it will be cancelled
    pub timeout: Option<u64>,
    // the maximum fee amounts in shannons that the sender is willing to pay, default is 1000 shannons CKB.
    pub max_fee_amount: Option<u128>,
    // max parts for the payment, only used for multi-part payments
    pub max_parts: Option<u64>,
    // keysend payment, default is false
    pub keysend: Option<bool>,
    // udt type script
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    // allow self payment, default is false
    pub allow_self_payment: bool,
    // dry_run only used for checking, default is false
    pub dry_run: bool,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentData {
    pub target_pubkey: Pubkey,
    pub amount: u128,
    pub payment_hash: Hash256,
    pub invoice: Option<String>,
    pub final_tlc_expiry_delta: u64,
    pub tlc_expiry_limit: u64,
    pub timeout: Option<u64>,
    pub max_fee_amount: Option<u128>,
    pub max_parts: Option<u64>,
    pub keysend: bool,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub preimage: Option<Hash256>,
    pub allow_self_payment: bool,
    pub dry_run: bool,
}

impl SendPaymentData {
    pub fn new(command: SendPaymentCommand) -> Result<SendPaymentData, String> {
        let invoice = command
            .invoice
            .as_ref()
            .map(|invoice| invoice.parse::<CkbInvoice>())
            .transpose()
            .map_err(|_| "invoice is invalid".to_string())?;

        if let Some(invoice) = invoice.clone() {
            if invoice.is_expired() {
                return Err("invoice is expired".to_string());
            }
        }

        fn validate_field<T: PartialEq + Clone>(
            field: Option<T>,
            invoice_field: Option<T>,
            field_name: &str,
        ) -> Result<T, String> {
            match (field, invoice_field) {
                (Some(f), Some(i)) => {
                    if f != i {
                        return Err(format!("{} does not match the invoice", field_name));
                    }
                    Ok(f)
                }
                (Some(f), None) => Ok(f),
                (None, Some(i)) => Ok(i),
                (None, None) => Err(format!("{} is missing", field_name)),
            }
        }

        let target = validate_field(
            command.target_pubkey,
            invoice
                .as_ref()
                .and_then(|i| i.payee_pub_key().cloned().map(Pubkey::from)),
            "target_pubkey",
        )?;

        let amount = validate_field(
            command.amount,
            invoice.as_ref().and_then(|i| i.amount()),
            "amount",
        )?;

        let udt_type_script = match validate_field(
            command.udt_type_script.clone(),
            invoice.as_ref().and_then(|i| i.udt_type_script().cloned()),
            "udt_type_script",
        ) {
            Ok(script) => Some(script),
            Err(e) if e == "udt_type_script is missing" => None,
            Err(e) => return Err(e),
        };

        // check htlc expiry delta and limit are both valid if it is set
        let final_tlc_expiry_delta = command
            .final_tlc_expiry_delta
            .or_else(|| {
                invoice
                    .as_ref()
                    .and_then(|i| i.final_tlc_minimum_expiry_delta().copied())
            })
            .unwrap_or(DEFAULT_TLC_EXPIRY_DELTA);
        if final_tlc_expiry_delta < MIN_TLC_EXPIRY_DELTA
            || final_tlc_expiry_delta > MAX_PAYMENT_TLC_EXPIRY_LIMIT
        {
            return Err(format!(
                "invalid final_tlc_expiry_delta, expect between {} and {}",
                MIN_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT
            ));
        }

        let tlc_expiry_limit = command
            .tlc_expiry_limit
            .unwrap_or(MAX_PAYMENT_TLC_EXPIRY_LIMIT);

        if tlc_expiry_limit < final_tlc_expiry_delta || tlc_expiry_limit < MIN_TLC_EXPIRY_DELTA {
            return Err("tlc_expiry_limit is too small".to_string());
        }
        if tlc_expiry_limit > MAX_PAYMENT_TLC_EXPIRY_LIMIT {
            return Err(format!(
                "tlc_expiry_limit is too large, expect it to less than {}",
                MAX_PAYMENT_TLC_EXPIRY_LIMIT
            ));
        }

        let keysend = command.keysend.unwrap_or(false);
        let (payment_hash, preimage) = if !keysend {
            (
                validate_field(
                    command.payment_hash,
                    invoice.as_ref().map(|i| *i.payment_hash()),
                    "payment_hash",
                )?,
                None,
            )
        } else {
            if invoice.is_some() {
                return Err("keysend payment should not have invoice".to_string());
            }
            if command.payment_hash.is_some() {
                return Err("keysend payment should not have payment_hash".to_string());
            }
            // generate a random preimage for keysend payment
            let mut rng = rand::thread_rng();
            let mut result = [0u8; 32];
            rng.fill(&mut result[..]);
            let preimage: Hash256 = result.into();
            // use the default payment hash algorithm here for keysend payment
            let payment_hash: Hash256 = blake2b_256(preimage).into();
            (payment_hash, Some(preimage))
        };

        Ok(SendPaymentData {
            target_pubkey: target,
            amount,
            payment_hash,
            invoice: command.invoice,
            final_tlc_expiry_delta,
            tlc_expiry_limit,
            timeout: command.timeout,
            max_fee_amount: command.max_fee_amount,
            max_parts: command.max_parts,
            keysend,
            udt_type_script,
            preimage,
            allow_self_payment: command.allow_self_payment,
            dry_run: command.dry_run,
        })
    }
}

#[derive(Debug)]
pub struct AcceptChannelCommand {
    pub temp_channel_id: Hash256,
    pub funding_amount: u128,
    pub shutdown_script: Option<Script>,
}

#[derive(Debug)]
pub struct SendOnionPacketCommand {
    pub packet: Vec<u8>,
    pub previous_tlc: Option<(Hash256, u64)>,
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

#[derive(Debug)]
pub enum NetworkServiceEvent {
    NetworkStarted(PeerId, MultiAddr, Vec<Multiaddr>),
    NetworkStopped(PeerId),
    PeerConnected(PeerId, Multiaddr),
    PeerDisConnected(PeerId, Multiaddr),
    // An incoming/outgoing channel is created.
    ChannelCreated(PeerId, Hash256),
    // A outgoing channel is pending to be accepted.
    ChannelPendingToBeAccepted(PeerId, Hash256),
    // The channel is ready to use (with funding transaction confirmed
    // and both parties sent ChannelReady messages).
    ChannelReady(PeerId, Hash256, OutPoint),
    ChannelClosed(PeerId, Hash256, Byte32),
    // We should sign a commitment transaction and send it to the other party.
    CommitmentSignaturePending(PeerId, Hash256, u64),
    // We have signed a commitment transaction and sent it to the other party.
    LocalCommitmentSigned(
        PeerId,          /* Peer Id */
        Hash256,         /* Channel Id */
        u64,             /* Commitment number */
        TransactionView, /* Commitment transaction, not valid per se (requires other party's signature) */
    ),
    // A RevokeAndAck is received from the peer. Other data relevant to this
    // RevokeAndAck message are also assembled here. The watch tower may use this.
    RevokeAndAckReceived(
        PeerId,           /* Peer Id */
        Hash256,          /* Channel Id */
        u64,              /* Commitment number */
        [u8; 32],         /* Aggregated public key x-only */
        CompactSignature, /* Aggregated signature */
        CellOutput,
        packed::Bytes,
    ),
    // The other party has signed a valid commitment transaction,
    // and we successfully assemble the partial signature from other party
    // to create a complete commitment transaction.
    RemoteCommitmentSigned(PeerId, Hash256, u64, TransactionView),
    // The syncing of network information has completed.
    SyncingCompleted,
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

    /// A funding transaction has been confirmed. The transaction was included in the
    /// block with the given transaction index.
    FundingTransactionConfirmed(OutPoint, BlockNumber, u32),

    /// A funding transaction has failed.
    FundingTransactionFailed(OutPoint),

    /// A commitment transaction is signed by us and has sent to the other party.
    LocalCommitmentSigned(PeerId, Hash256, u64, TransactionView),

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

    // A tlc remove message is received. (payment_hash, remove_tlc)
    TlcRemoveReceived(Hash256, RemoveTlcReason),
}

#[derive(Copy, Clone, Debug)]
pub enum GraphSyncerExitStatus {
    Succeeded,
    Failed,
}

impl Default for GraphSyncerExitStatus {
    fn default() -> Self {
        Self::Failed
    }
}

#[derive(Debug)]
pub enum NetworkActorMessage {
    Command(NetworkActorCommand),
    Event(NetworkActorEvent),
    Notification(NetworkServiceEvent),
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

pub struct NetworkActor<S> {
    // An event emitter to notify ourside observers.
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    chain_actor: ActorRef<CkbChainMessage>,
    store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
}

impl<S> NetworkActor<S>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + NetworkGraphStateStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn new(
        event_sender: mpsc::Sender<NetworkServiceEvent>,
        chain_actor: ActorRef<CkbChainMessage>,
        store: S,
        network_graph: Arc<RwLock<NetworkGraph<S>>>,
    ) -> Self {
        Self {
            event_sender,
            chain_actor,
            store: store.clone(),
            network_graph,
        }
    }

    pub async fn handle_peer_message(
        &self,
        state: &mut NetworkActorState<S>,
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
                            };
                            state.create_inbound_channel(accept_channel).await?;
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
                    .send_message_to_channel_actor(
                        channel_id,
                        Some(&peer_id),
                        ChannelActorMessage::PeerMessage(m),
                    )
                    .await;
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
                    let original_id = match state.get_original_request_id(&peer_id, id) {
                        Some(id) => id,
                        None => {
                            return Err(Error::InvalidPeerMessage(format!(
                                "No original request for query broadcast messages with id {} from peer {:?}",
                                id, &peer_id)
                            ));
                        }
                    };
                    for message in messages {
                        if let Err(e) = self.process_broadcasted_message(state, message).await {
                            let fail_message =
                                format!("Failed to process broadcasted message: {:?}", &e);
                            error!("{}", &fail_message);
                            state.mark_request_failed(&peer_id, original_id, e);
                            return Err(Error::InvalidPeerMessage(fail_message));
                        }
                    }
                    debug!(
                        "Successfully processed all the messages from peer {:?} with id {}",
                        &peer_id, id
                    );
                    state.mark_request_finished(&peer_id, original_id);
                }
                FiberQueryInformation::QueryChannelsWithinBlockRange(
                    QueryChannelsWithinBlockRange {
                        id,
                        chain_hash: _,
                        start_block,
                        end_block,
                    },
                ) => {
                    let (channels, next_offset, is_finished) = self
                        .query_channels_within_block_range(start_block, end_block)
                        .await;
                    debug!(
                        "Query channels within block range: {:?}, {:?}, {:?}",
                        &channels, next_offset, is_finished
                    );

                    let channels = channels.into_iter().map(|c| c.out_point()).collect();
                    let reply = FiberQueryInformation::QueryChannelsWithinBlockRangeResult(
                        QueryChannelsWithinBlockRangeResult {
                            id,
                            channels,
                            next_block: next_offset,
                            is_finished,
                        },
                    );
                    state
                        .send_message_to_peer(&peer_id, FiberMessage::QueryInformation(reply))
                        .await?;
                }
                FiberQueryInformation::QueryChannelsWithinBlockRangeResult(
                    QueryChannelsWithinBlockRangeResult {
                        id,
                        next_block,
                        is_finished,
                        channels,
                    },
                ) => {
                    if channels.is_empty() {
                        // No query to the peer needed, early return.
                        match state
                            .broadcast_message_responses
                            .remove(&(peer_id.clone(), id))
                        {
                            Some(reply) => {
                                let _ = reply.1.send(Ok((next_block, is_finished)));
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
                    if let Some(new_id) =
                        state.record_request_result(&peer_id, id, next_block, is_finished)
                    {
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
                    let (queries, next_time, is_finished) = self
                        .query_broadcast_messages_within_time_range(start_time, end_time)
                        .await?;
                    let reply = FiberQueryInformation::QueryBroadcastMessagesWithinTimeRangeResult(
                        QueryBroadcastMessagesWithinTimeRangeResult {
                            id,
                            queries,
                            next_time,
                            is_finished,
                        },
                    );
                    state
                        .send_message_to_peer(&peer_id, FiberMessage::QueryInformation(reply))
                        .await?;
                }
                FiberQueryInformation::QueryBroadcastMessagesWithinTimeRangeResult(
                    QueryBroadcastMessagesWithinTimeRangeResult {
                        id,
                        next_time,
                        is_finished,
                        queries,
                    },
                ) => {
                    if queries.is_empty() {
                        // No query to the peer needed, early return.
                        match state
                            .broadcast_message_responses
                            .remove(&(peer_id.clone(), id))
                        {
                            Some(reply) => {
                                let _ = reply.1.send(Ok((next_time, is_finished)));
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

                    if let Some(new_id) =
                        state.record_request_result(&peer_id, id, next_time, is_finished)
                    {
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
    ) -> (Vec<ChannelInfo>, u64, bool) {
        let network_graph = self.network_graph.read().await;
        let (channels, next_offset, is_finished) =
            network_graph.get_channels_within_block_range(start_block, end_block);
        (channels.cloned().collect(), next_offset, is_finished)
    }

    // TODO: set a upper limit for the number of message to send.
    pub async fn query_broadcast_messages_within_time_range(
        &self,
        start_time: u64,
        end_time: u64,
    ) -> Result<(Vec<FiberBroadcastMessageQuery>, u64, bool), Error> {
        let is_within_range = |timestamp: u64| timestamp >= start_time && timestamp < end_time;

        let network_graph = self.network_graph.read().await;

        let mut queries = Vec::new();
        for node_info in network_graph.nodes() {
            if is_within_range(node_info.timestamp) {
                queries.push(FiberBroadcastMessageQuery::NodeAnnouncement(
                    NodeAnnouncementQuery {
                        node_id: node_info.node_id,
                        flags: 0,
                    },
                ));
            }
        }
        for channel_info in network_graph.channels() {
            if let Some(t) = channel_info.channel_update_node1_to_node2_timestamp() {
                if is_within_range(t) {
                    queries.push(FiberBroadcastMessageQuery::ChannelUpdate(
                        ChannelUpdateQuery {
                            channel_outpoint: channel_info.out_point(),
                            flags: channel_info.node1_to_node2_channel_update_flags(),
                        },
                    ));
                }
            }

            if let Some(t) = channel_info.channel_update_node2_to_node1_timestamp() {
                if is_within_range(t) {
                    queries.push(FiberBroadcastMessageQuery::ChannelUpdate(
                        ChannelUpdateQuery {
                            channel_outpoint: channel_info.out_point(),
                            flags: channel_info.node2_to_node1_channel_update_flags(),
                        },
                    ));
                }
            }
        }
        Ok((queries, end_time, true))
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
                let node_info = network_graph.get_node(node_id);
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
                let is_node1_to_node2 = flags & 1 == 0;
                match channel_info {
                    Some(channel_info) => {
                        let update = if is_node1_to_node2 {
                            channel_info
                                .node1_to_node2
                                .as_ref()
                                .map(|u| u.last_update_message.clone())
                        } else {
                            channel_info
                                .node2_to_node1
                                .as_ref()
                                .map(|u| u.last_update_message.clone())
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
        state: &mut NetworkActorState<S>,
        event: NetworkActorEvent,
    ) -> crate::Result<()> {
        debug!("Handling event: {:?}", event);
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
                    .insert(channel_outpoint.clone(), channel_id);

                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::ChannelReady(peer_id, channel_id, channel_outpoint),
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
            NetworkActorEvent::FundingTransactionConfirmed(outpoint, block_number, tx_index) => {
                state
                    .on_funding_transaction_confirmed(outpoint, block_number, tx_index)
                    .await;
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
            NetworkActorEvent::LocalCommitmentSigned(peer_id, channel_id, version, tx) => {
                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::LocalCommitmentSigned(
                            peer_id, channel_id, version, tx,
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
                        actor
                            .get_cell()
                            .stop(Some("stopping syncer normally".to_string()));
                    }
                    debug!("Changing sync succeeded/failed counter");
                    match reason {
                        GraphSyncerExitStatus::Succeeded => state.succeeded += 1,
                        GraphSyncerExitStatus::Failed => state.failed += 1,
                    }
                }
                state.maybe_finish_sync();
            }
            NetworkActorEvent::TlcRemoveReceived(payment_hash, remove_tlc_reason) => {
                // When a node is restarted, RemoveTLC will also be resent if necessary
                self.on_remove_tlc_event(state, payment_hash, remove_tlc_reason)
                    .await;
            }
        }
        Ok(())
    }

    pub async fn handle_command(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        command: NetworkActorCommand,
    ) -> crate::Result<()> {
        match command {
            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId { peer_id, message }) => {
                state.send_message_to_peer(&peer_id, message).await?;
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
                } else {
                    error!("Failed to extract peer id from address: {:?}", addr);
                    return Ok(());
                }

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

            NetworkActorCommand::SavePeerAddress(addr) => match extract_peer_id(&addr) {
                Some(peer) => {
                    debug!("Saved peer id {:?} with address {:?}", &peer, &addr);
                    state.save_peer_address(peer, addr);
                }
                None => {
                    error!("Failed to save address to peer store: unable to extract peer id from address {:?}", &addr);
                }
            },

            NetworkActorCommand::MaintainConnections(num_peers) => {
                debug!("Maintaining connections to {} peers", num_peers);

                let num_connected_peers = state.peer_session_map.len();
                if num_connected_peers >= num_peers {
                    debug!(
                        "Already connected to {} peers, skipping connecting to more peers",
                        num_connected_peers,
                    );
                    return Ok(());
                }
                let peers_to_connect = state
                    .state_to_be_persisted
                    .sample_n_peers_to_connect(num_peers - num_connected_peers);
                debug!(
                    "Randomly selected peers to connect: {:?}",
                    &peers_to_connect
                );
                for (peer_id, addresses) in peers_to_connect {
                    if let Some(session) = state.get_peer_session(&peer_id) {
                        debug!(
                            "Randomly selected peer {:?} already connected with session id {:?}, skipping connection",
                            peer_id, session
                        );
                        continue;
                    }
                    for addr in addresses {
                        state
                            .network
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::ConnectPeer(addr.clone()),
                            ))
                            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                    }
                }
            }

            NetworkActorCommand::OpenChannel(open_channel, reply) => {
                match state.create_outbound_channel(open_channel).await {
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

            NetworkActorCommand::ControlFiberChannel(c) => {
                info!("send command to channel: {:?}", c);
                state
                    .send_command_to_channel(c.channel_id, c.command)
                    .await?
            }

            // TODO: we should check the OnionPacket is valid or not, only the current node can decrypt it.
            NetworkActorCommand::SendPaymentOnionPacket(command, reply) => {
                self.handle_send_onion_packet_command(state, command, reply)
                    .await;
            }
            NetworkActorCommand::PeelPaymentOnionPacket(onion_packet, payment_hash, reply) => {
                let response = PaymentOnionPacket::new(onion_packet)
                    .peel(
                        &state.private_key,
                        Some(payment_hash.as_ref()),
                        &Secp256k1::new(),
                    )
                    .map_err(|err| err.to_string());

                let _ = reply.send(response);
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
            NetworkActorCommand::BroadcastMessage(message) => {
                const MAX_BROADCAST_SESSIONS: usize = 5;
                // TODO: It is possible that the remote peer of the channel may repeatedly
                // receive the same message.
                let peer_ids = state.get_n_peer_peer_ids(MAX_BROADCAST_SESSIONS, HashSet::new());
                debug!(
                    "Broadcasting message to randomly selected peers {:?} (from {:?})",
                    &peer_ids, &state.peer_id
                );
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
                match self.on_send_payment(state, payment_request).await {
                    Ok(payment) => {
                        let _ = reply.send(Ok(payment));
                    }
                    Err(e) => {
                        error!("Failed to send payment: {:?}", e);
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
                    // Need also to update our own graph with the new node announcement.
                    let mut graph = self.network_graph.write().await;
                    graph.process_node_announcement(message.clone());
                    myself
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::BroadcastMessage(
                                FiberBroadcastMessage::NodeAnnouncement(message),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                }
            },
            NetworkActorCommand::MarkSyncingDone => {
                info!("Syncing network information finished");
                state.sync_status = NetworkSyncStatus::Done;
                let mut broadcasted_message_queue = vec![];
                // Consume broadcasted message queue without consue the whole state.
                std::mem::swap(
                    &mut state.broadcasted_message_queue,
                    &mut broadcasted_message_queue,
                );
                for message in broadcasted_message_queue {
                    let (_peer_id, message) = message;
                    if let Err(e) = self.process_broadcasted_message(state, message).await {
                        error!("Failed to process broadcasted message: {:?}", e);
                    }
                }
                // Send a service event that manifests the syncing is done.
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::SyncingCompleted,
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
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
            NetworkActorCommand::StartSyncing => match &mut state.sync_status {
                NetworkSyncStatus::NotRunning(ref sync_state) => {
                    debug!(
                        "Starting syncing network information from state {:?}",
                        sync_state
                    );
                    let sync_state = sync_state.refresh(self.chain_actor.clone()).await;
                    state.sync_status = NetworkSyncStatus::Running(sync_state);
                    let peers: Vec<_> = state.peer_session_map.keys().cloned().collect();
                    debug!(
                        "Trying to sync network information from peers: {:?}, sync parameters: {:?}",
                        &peers, &state.sync_status
                    );
                    for peer_id in peers {
                        state.maybe_sync_network_graph(&peer_id).await;
                    }
                }
                _ => {
                    error!(
                        "Syncing is already started or is done: {:?}",
                        &state.sync_status
                    );
                }
            },
            NetworkActorCommand::StopSyncing => match &mut state.sync_status {
                NetworkSyncStatus::Running(s) => {
                    debug!("Stopping syncing network information");
                    let mut s = s.clone();
                    for syncer in s.active_syncers.values() {
                        syncer
                            .get_cell()
                            .stop(Some("stopping syncer on request".to_string()));
                    }
                    s.active_syncers.clear();
                    state.sync_status = NetworkSyncStatus::NotRunning(s);
                }
                _ => {
                    error!(
                        "Syncing is not running or is already done: {:?}",
                        &state.sync_status
                    );
                }
            },
            NetworkActorCommand::ProcessChannelAnnouncement(
                peer_id,
                block_number,
                tx_index,
                channel_announcement,
            ) => {
                debug!(
                    "Processing our channel announcement message (confirmed at #{} block #{} tx) to peer {:?}: {:?}",
                    &block_number,
                    &tx_index,
                    &peer_id,
                    &channel_announcement
                );
                // Adding this owned channel to the network graph.
                let channel_info = ChannelInfo {
                    funding_tx_block_number: block_number.into(),
                    funding_tx_index: tx_index,
                    announcement_msg: channel_announcement.clone(),
                    node1_to_node2: None, // wait for channel update message
                    node2_to_node1: None,
                    timestamp: std::time::UNIX_EPOCH
                        .elapsed()
                        .expect("Duration since unix epoch")
                        .as_millis() as u64,
                };
                let mut graph = self.network_graph.write().await;
                graph.add_channel(channel_info);

                // Send the channel announcement to other peer first.
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId {
                            peer_id,
                            message: FiberMessage::BroadcastMessage(
                                FiberBroadcastMessage::ChannelAnnouncement(
                                    channel_announcement.clone(),
                                ),
                            ),
                        }),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                // Also broadcast channel announcement to the network.
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::BroadcastMessage(
                            FiberBroadcastMessage::ChannelAnnouncement(channel_announcement),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }

            NetworkActorCommand::ProccessChannelUpdate(peer_id, channel_update) => {
                debug!(
                    "Processing our channel update message to peer {:?}: {:?}",
                    &peer_id, &channel_update
                );
                let mut graph = self.network_graph.write().await;
                graph
                    .process_channel_update(channel_update.clone())
                    .expect("Valid channel update");

                // Send the channel update to other peer first.
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId {
                            peer_id,
                            message: FiberMessage::BroadcastMessage(
                                FiberBroadcastMessage::ChannelUpdate(channel_update.clone()),
                            ),
                        }),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
                // Also broadcast channel update to the network.
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::BroadcastMessage(
                            FiberBroadcastMessage::ChannelUpdate(channel_update),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
            NetworkActorCommand::NodeInfo(_, rpc) => {
                let response = NodeInfoResponse {
                    node_name: state.node_name.clone(),
                    peer_id: state.peer_id.clone(),
                    public_key: state.get_public_key().clone(),
                    addresses: state.announced_addrs.clone(),
                    chain_hash: get_chain_hash(),
                    open_channel_auto_accept_min_ckb_funding_amount: state
                        .open_channel_auto_accept_min_ckb_funding_amount,
                    auto_accept_channel_ckb_funding_amount: state
                        .auto_accept_channel_ckb_funding_amount,
                    tlc_expiry_delta: state.tlc_expiry_delta,
                    tlc_min_value: state.tlc_min_value,
                    tlc_max_value: state.tlc_max_value,
                    tlc_fee_proportional_millionths: state.tlc_fee_proportional_millionths,
                    channel_count: state.channels.len() as u32,
                    pending_channel_count: state.pending_channels.len() as u32,
                    peers_count: state.peer_session_map.len() as u32,
                    network_sync_status: state.sync_status.as_str().to_string(),
                    udt_cfg_infos: get_udt_whitelist(),
                };
                let _ = rpc.send(Ok(response));
            }
        };
        Ok(())
    }

    async fn process_or_stash_broadcasted_message(
        &self,
        state: &mut NetworkActorState<S>,
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
                NetworkActorCommand::BroadcastMessage(message.clone()),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        self.process_broadcasted_message(state, message).await
    }

    async fn process_broadcasted_message(
        &self,
        state: &mut NetworkActorState<S>,
        message: FiberBroadcastMessage,
    ) -> Result<(), Error> {
        match message {
            FiberBroadcastMessage::NodeAnnouncement(node_announcement) => {
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
                        let mut node_announcement = node_announcement.clone();
                        if !state.announce_private_addr {
                            node_announcement.addresses.retain(|addr| {
                                multiaddr_to_socketaddr(addr)
                                    .map(|socket_addr| is_reachable(socket_addr.ip()))
                                    .unwrap_or_default()
                            });
                        }
                        if !node_announcement.addresses.is_empty() {
                            // Add the node to the network graph.
                            self.network_graph
                                .write()
                                .await
                                .process_node_announcement(node_announcement.clone());

                            let peer_id = node_announcement.peer_id();
                            state.save_announced_peer_addresses(
                                peer_id,
                                node_announcement.addresses,
                            );
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
                if channel_announcement.node1_id == channel_announcement.node2_id {
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
                    }) => (tx, block_number.into(), DUMMY_FUNDING_TX_INDEX),
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

                // Add the channel to the network graph.
                let channel_info = ChannelInfo {
                    funding_tx_block_number: block_number,
                    funding_tx_index: tx_index,
                    announcement_msg: channel_announcement.clone(),
                    node1_to_node2: None, // wait for channel update message
                    node2_to_node1: None,
                    timestamp: std::time::UNIX_EPOCH
                        .elapsed()
                        .expect("Duration since unix epoch")
                        .as_millis() as u64,
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

    async fn handle_send_onion_packet_command(
        &self,
        state: &mut NetworkActorState<S>,
        command: SendOnionPacketCommand,
        reply: RpcReplyPort<Result<u64, TlcErrPacket>>,
    ) {
        let SendOnionPacketCommand {
            packet,
            previous_tlc,
        } = command;

        let invalid_onion_error = |reply: RpcReplyPort<Result<u64, TlcErrPacket>>| {
            let error_detail =
                TlcErr::new_node_fail(TlcErrorCode::InvalidOnionPayload, state.get_public_key());
            reply
                .send(Err(TlcErrPacket::new(error_detail)))
                .expect("send error failed");
        };

        let Ok(peeled_packet) = PeeledPaymentOnionPacket::deserialize(&packet) else {
            info!("onion packet is empty, ignore it");
            return invalid_onion_error(reply);
        };

        let info = peeled_packet.current;
        debug!("Processing onion packet info: {:?}", info);

        let Some(channel_outpoint) = &info.channel_outpoint else {
            return invalid_onion_error(reply);
        };

        let unknown_next_peer = |reply: RpcReplyPort<Result<u64, TlcErrPacket>>| {
            let error_detail = TlcErr::new_channel_fail(
                TlcErrorCode::UnknownNextPeer,
                channel_outpoint.clone(),
                None,
            );
            reply
                .send(Err(TlcErrPacket::new(error_detail)))
                .expect("send add tlc response");
        };

        let channel_id = match state.outpoint_channel_map.get(channel_outpoint) {
            Some(channel_id) => channel_id,
            None => {
                error!(
                        "Channel id not found in outpoint_channel_map with {:?}, are we connected to the peer?",
                        channel_outpoint
                    );
                return unknown_next_peer(reply);
            }
        };
        let (send, recv) = oneshot::channel::<Result<AddTlcResponse, TlcErrPacket>>();
        let rpc_reply = RpcReplyPort::from(send);
        let command = ChannelCommand::AddTlc(
            AddTlcCommand {
                amount: info.amount,
                preimage: None,
                payment_hash: Some(info.payment_hash),
                expiry: info.expiry,
                hash_algorithm: info.tlc_hash_algorithm,
                onion_packet: peeled_packet.next.map(|next| next.data).unwrap_or_default(),
                previous_tlc,
            },
            rpc_reply,
        );

        // we have already checked the channel_id is valid,
        match state.send_command_to_channel(*channel_id, command).await {
            Ok(()) => {}
            Err(Error::ChannelNotFound(_)) => {
                return unknown_next_peer(reply);
            }
            Err(err) => {
                // must be some error from tentacle, set it as temporary node failure
                error!(
                    "Failed to send onion packet to channel: {:?} with err: {:?}",
                    channel_id, err
                );
                let error_detail = TlcErr::new(TlcErrorCode::TemporaryNodeFailure);
                return reply
                    .send(Err(TlcErrPacket::new(error_detail)))
                    .expect("send add tlc response");
            }
        }
        let add_tlc_res = recv.await.expect("recv error").map(|res| res.tlc_id);
        reply.send(add_tlc_res).expect("send error");
    }

    async fn on_remove_tlc_event(
        &self,
        state: &mut NetworkActorState<S>,
        payment_hash: Hash256,
        reason: RemoveTlcReason,
    ) {
        if let Some(mut payment_session) = self.store.get_payment_session(payment_hash) {
            if payment_session.status == PaymentSessionStatus::Inflight {
                match reason {
                    RemoveTlcReason::RemoveTlcFulfill(_) => {
                        self.network_graph
                            .write()
                            .await
                            .record_payment_success(payment_session);
                    }
                    RemoveTlcReason::RemoveTlcFail(reason) => {
                        let error_detail = reason.decode().expect("decoded error");
                        self.update_graph_with_tlc_fail(&error_detail).await;
                        let need_to_retry = self
                            .network_graph
                            .write()
                            .await
                            .record_payment_fail(&payment_session, error_detail.clone());
                        if need_to_retry {
                            let res = self.try_payment_session(state, payment_session).await;
                            if res.is_err() {
                                debug!("Failed to retry payment session: {:?}", res);
                            }
                        } else {
                            self.set_payment_fail_with_error(
                                &mut payment_session,
                                error_detail.error_code.as_ref(),
                            );
                        }
                    }
                }
            }
        }
    }

    async fn update_graph_with_tlc_fail(&self, tcl_error_detail: &TlcErr) {
        let error_code = tcl_error_detail.error_code();
        // https://github.com/lightning/bolts/blob/master/04-onion-routing.md#rationale-6
        // we now still update the graph, maybe we need to remove it later?
        if error_code.is_update() {
            if let Some(extra_data) = &tcl_error_detail.extra_data {
                match extra_data {
                    TlcErrData::ChannelFailed { channel_update, .. } => {
                        if let Some(channel_update) = channel_update {
                            let _ = self
                                .network_graph
                                .write()
                                .await
                                .process_channel_update(channel_update.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        match tcl_error_detail.error_code() {
            TlcErrorCode::PermanentChannelFailure
            | TlcErrorCode::ChannelDisabled
            | TlcErrorCode::UnknownNextPeer => {
                let channel_outpoint = tcl_error_detail
                    .error_channel_outpoint()
                    .expect("expect channel outpoint");
                debug!("mark channel failed: {:?}", channel_outpoint);
                let mut graph = self.network_graph.write().await;
                graph.mark_channel_failed(&channel_outpoint);
            }
            TlcErrorCode::PermanentNodeFailure => {
                let node_id = tcl_error_detail.error_node_id().expect("expect node id");
                let mut graph = self.network_graph.write().await;
                graph.mark_node_failed(node_id);
            }
            _ => {}
        }
    }

    fn on_get_payment(&self, payment_hash: &Hash256) -> Result<SendPaymentResponse, Error> {
        match self.store.get_payment_session(*payment_hash) {
            Some(payment_session) => Ok(payment_session.into()),
            None => Err(Error::InvalidParameter(format!(
                "Payment session not found: {:?}",
                payment_hash
            ))),
        }
    }

    async fn build_payment_route(
        &self,
        payment_session: &mut PaymentSession,
        payment_data: &SendPaymentData,
    ) -> Result<Vec<PaymentHopData>, Error> {
        match self
            .network_graph
            .read()
            .await
            .build_route(payment_data.clone())
        {
            Err(e) => {
                let error = format!("Failed to build route, {}", e);
                self.set_payment_fail_with_error(payment_session, &error);
                return Err(Error::SendPaymentError(error));
            }
            Ok(hops) => {
                assert!(hops[0].channel_outpoint.is_some());
                return Ok(hops);
            }
        };
    }

    async fn send_payment_onion_packet(
        &self,
        state: &mut NetworkActorState<S>,
        payment_session: &mut PaymentSession,
        payment_data: &SendPaymentData,
        hops: Vec<PaymentHopData>,
    ) -> Result<PaymentSession, Error> {
        let session_key = Privkey::from_slice(KeyPair::generate_random_key().as_ref());
        let first_channel_outpoint = hops[0]
            .channel_outpoint
            .clone()
            .expect("first hop channel must exist");

        payment_session.route =
            SessionRoute::new(state.get_public_key(), payment_data.target_pubkey, &hops);

        let (send, recv) = oneshot::channel::<Result<u64, TlcErrPacket>>();
        let rpc_reply = RpcReplyPort::from(send);
        let peeled_packet =
            match PeeledPaymentOnionPacket::create(session_key, hops, &Secp256k1::signing_only()) {
                Ok(packet) => packet,
                Err(e) => {
                    let err = format!(
                        "Failed to create onion packet: {:?}, error: {:?}",
                        payment_data.payment_hash, e
                    );
                    self.set_payment_fail_with_error(payment_session, &err);
                    return Err(Error::SendPaymentError(err));
                }
            };
        let command = SendOnionPacketCommand {
            packet: peeled_packet.serialize(),
            previous_tlc: None,
        };

        self.handle_send_onion_packet_command(state, command, rpc_reply)
            .await;
        match recv.await.expect("msg recv error") {
            Err(e) => {
                if let Some(error_detail) = e.decode() {
                    // This is the error implies we send payment request to the first hop failed
                    // graph or payment history need to update and then have a retry
                    self.update_graph_with_tlc_fail(&error_detail).await;
                    let _ = self
                        .network_graph
                        .write()
                        .await
                        .record_payment_fail(&payment_session, error_detail.clone());
                    let err = format!(
                        "Failed to send onion packet with error {}",
                        error_detail.error_code_as_str()
                    );
                    self.set_payment_fail_with_error(payment_session, &err);
                    return Err(Error::SendPaymentFirstHopError(err));
                } else {
                    // This expected never to be happended, to be safe, we will set the payment session to failed
                    let err = format!("Failed to send onion packet, got malioucious error message");
                    self.set_payment_fail_with_error(payment_session, &err);
                    return Err(Error::SendPaymentError(err));
                }
            }
            Ok(tlc_id) => {
                payment_session.set_inflight_status(first_channel_outpoint, tlc_id);
                self.store.insert_payment_session(payment_session.clone());
                return Ok(payment_session.clone());
            }
        }
    }

    fn set_payment_fail_with_error(&self, payment_session: &mut PaymentSession, error: &str) {
        payment_session.set_failed_status(error);
        self.store.insert_payment_session(payment_session.clone());
    }

    async fn try_payment_session(
        &self,
        state: &mut NetworkActorState<S>,
        mut payment_session: PaymentSession,
    ) -> Result<PaymentSession, Error> {
        let payment_data = payment_session.request.clone();
        while payment_session.can_retry() {
            payment_session.retried_times += 1;

            let hops_info = self
                .build_payment_route(&mut payment_session, &payment_data)
                .await?;

            match self
                .send_payment_onion_packet(state, &mut payment_session, &payment_data, hops_info)
                .await
            {
                Ok(payment_session) => return Ok(payment_session),
                Err(Error::SendPaymentFirstHopError(_)) => {
                    // we will retry the payment session
                    continue;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        let error = payment_session.last_error.clone().unwrap_or_else(|| {
            format!(
                "Failed to send payment session: {:?}, retried times: {}",
                payment_data.payment_hash, payment_session.retried_times
            )
        });
        return Err(Error::SendPaymentError(error));
    }

    async fn on_send_payment(
        &self,
        state: &mut NetworkActorState<S>,
        payment_request: SendPaymentCommand,
    ) -> Result<SendPaymentResponse, Error> {
        let payment_data = SendPaymentData::new(payment_request.clone()).map_err(|e| {
            error!("Failed to validate payment request: {:?}", e);
            Error::InvalidParameter(format!("Failed to validate payment request: {:?}", e))
        })?;

        // for dry run, we only build the route and return the hops info,
        // will not store the payment session and send the onion packet
        if payment_data.dry_run {
            let mut payment_session = PaymentSession::new(payment_data.clone(), 0);
            let hops = self
                .build_payment_route(&mut payment_session, &payment_data)
                .await?;
            payment_session.route =
                SessionRoute::new(state.get_public_key(), payment_data.target_pubkey, &hops);
            return Ok(payment_session.into());
        }

        // initialize the payment session in db and begin the payment process lifecycle
        if let Some(payment_session) = self.store.get_payment_session(payment_data.payment_hash) {
            // we only allow retrying payment session with status failed
            debug!("Payment session already exists: {:?}", payment_session);
            if payment_session.status != PaymentSessionStatus::Failed {
                return Err(Error::InvalidParameter(format!(
                    "Payment session already exists: {} with payment session status: {:?}",
                    payment_data.payment_hash, payment_session.status
                )));
            }
        }

        let payment_session = PaymentSession::new(payment_data, 5);
        self.store.insert_payment_session(payment_session.clone());
        let session = self.try_payment_session(state, payment_session).await?;
        return Ok(session.into());
    }
}

#[derive(Debug, Clone)]
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
    // TODO: the intention of passing a few peer addresses to the sync status was to let the user
    // select a few peers to sync network graph (these peers may have faster connection to the node).
    // After some refactoring, the code below is a little bit clouded. We are currently only connecting
    // to random peers. If this functionality is desired, we should make a config option for it.
    // Otherwise, remove this completely.
    pinned_syncing_peers: Vec<(PeerId, Multiaddr)>,
    active_syncers: HashMap<PeerId, ActorRef<GraphSyncerMessage>>,
    // Number of peers with whom we succeeded to sync.
    succeeded: usize,
    // Number of peers with whom we failed to sync.
    failed: usize,
}

impl NetworkSyncState {
    async fn refresh(&self, chain_actor: ActorRef<CkbChainMessage>) -> Self {
        let mut cloned = self.clone();
        cloned.active_syncers.clear();
        cloned.succeeded = 0;
        cloned.failed = 0;
        // TODO: The calling to chain actor will block the calling actor from handling other messages.
        let current_block_number = call!(chain_actor, CkbChainMessage::GetCurrentBlockNumber, ())
            .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
            .expect("Get current block number from chain");
        cloned.ending_height = current_block_number;
        cloned
    }

    // Note that this function may actually change the state, this is because,
    // when the sync to all peers failed, we actually want to start a new syncer,
    // and we want to track this syncer.
    async fn maybe_create_graph_syncer(
        &mut self,
        peer_id: &PeerId,
        network: ActorRef<NetworkActorMessage>,
    ) -> Option<ActorRef<GraphSyncerMessage>> {
        let should_create = !self.active_syncers.contains_key(peer_id)
            && if self.failed >= self.pinned_syncing_peers.len() {
                // There are two possibility for the above condition to be true:
                // 1) we don't have any pinned syncing peers.
                // 2) we have some pinned syncing peers, and all of them failed to sync.
                // In the first case, both self.pinned_syncing_peers.len() is always 0,
                // and self.failed is alway greater or equal 0, so the condition is always true.
                // In the second case, if self.failed is larger than the length of pinned_syncing_peers,
                // then all of pinned sync peers failed to sync. This is because
                // we will always try to sync with all the pinned syncing peers first.
                if self.succeeded != 0 {
                    // TODO: we may want more than one successful syncing.
                    false
                } else {
                    debug!("Adding peer to dynamic syncing peers list: peer {:?}, succeeded syncing {}, failed syncing {}, pinned syncing peers {}",
                            peer_id, self.succeeded, self.failed, self.pinned_syncing_peers.len());
                    true
                }
            } else {
                self.pinned_syncing_peers
                    .iter()
                    .any(|(id, _)| id == peer_id)
            };

        if should_create {
            let graph_syncer = Actor::spawn_linked(
                Some(format!(
                    "Graph syncer to {} started at {:?}",
                    peer_id,
                    SystemTime::now()
                )),
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

#[derive(Debug, Clone)]
enum NetworkSyncStatus {
    // The syncing is not running, but we have all the information to start syncing.
    NotRunning(NetworkSyncState),
    // We should start running the syncing immediately or the syncing is already in progress.
    Running(NetworkSyncState),
    // Syncing done, unless we restart the node, we don't have to sync again
    // (we will automatically process the newest broadcasted network messages).
    Done,
}

impl NetworkSyncStatus {
    fn new(
        start_immediately: bool,
        starting_height: u64,
        ending_height: u64,
        starting_time: u64,
        pinned_syncing_peers: Vec<(PeerId, Multiaddr)>,
    ) -> Self {
        let state = NetworkSyncState {
            starting_height,
            ending_height,
            starting_time,
            pinned_syncing_peers,
            active_syncers: Default::default(),
            succeeded: 0,
            failed: 0,
        };
        if start_immediately {
            NetworkSyncStatus::Running(state)
        } else {
            NetworkSyncStatus::NotRunning(state)
        }
    }

    fn is_syncing(&self) -> bool {
        match self {
            NetworkSyncStatus::NotRunning(_) => false,
            NetworkSyncStatus::Running(_) => true,
            NetworkSyncStatus::Done => false,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            NetworkSyncStatus::NotRunning(_) => "NotRunning",
            NetworkSyncStatus::Running(_) => "Running",
            NetworkSyncStatus::Done => "Done",
        }
    }
}

#[derive(Debug)]
enum RequestState {
    RequestSent,
    RequestReturned(u64, bool),
}

pub struct NetworkActorState<S> {
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
    peer_session_map: HashMap<PeerId, SessionId>,
    // This map is used to store the public key of the peer.
    peer_pubkey_map: HashMap<PeerId, Pubkey>,
    session_channels_map: HashMap<SessionId, HashSet<Hash256>>,
    channels: HashMap<Hash256, ActorRef<ChannelActorMessage>>,
    // Outpoint to channel id mapping, only contains channels with state of Ready.
    // We need to remove the channel from this map when the channel is closed or peer disconnected.
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
    // The default expiry delta to forward tlcs.
    tlc_expiry_delta: u64,
    // The default tlc min and max value of tlcs to be accepted.
    tlc_min_value: u128,
    tlc_max_value: u128,
    // The default tlc fee proportional millionths to be used when auto accepting a channel.
    tlc_fee_proportional_millionths: u128,
    // Whether to announce private address to the network.
    announce_private_addr: bool,
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
    // The first parameter is the next offset of the request, and the second parameter
    // indicates whether the previous query is already fulfilled without additional results.
    broadcast_message_responses:
        HashMap<(PeerId, u64), (RequestState, RpcReplyPort<Result<(u64, bool), Error>>)>,
    original_requests: HashMap<(PeerId, u64), u64>,
    // This field holds the information about our syncing status.
    sync_status: NetworkSyncStatus,
    // A queue of messages that are received while we are syncing network messages.
    // Need to be processed after the sync is done.
    broadcasted_message_queue: Vec<(PeerId, FiberBroadcastMessage)>,
}

#[serde_as]
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PersistentNetworkActorState {
    // These addresses are announced by the peer itself to the network.
    // When a new NodeAnnouncement message is received, we will overwrite the old addresses.
    #[serde_as(as = "Vec<(DisplayFromStr, _)>")]
    announced_peer_addresses: HashMap<PeerId, Vec<Multiaddr>>,
    // These addresses are saved by the user (e.g. the user sends a ConnectPeer rpc to the node),
    // we will then save these addresses to the peer store.
    #[serde_as(as = "Vec<(DisplayFromStr, _)>")]
    saved_peer_addresses: HashMap<PeerId, Vec<Multiaddr>>,
}

impl PersistentNetworkActorState {
    pub fn new() -> Self {
        Default::default()
    }

    fn get_peer_addresses(&self, peer_id: &PeerId) -> HashSet<Multiaddr> {
        let empty = vec![];
        self.announced_peer_addresses
            .get(peer_id)
            .unwrap_or(&empty)
            .iter()
            .chain(
                self.saved_peer_addresses
                    .get(peer_id)
                    .unwrap_or(&empty)
                    .iter(),
            )
            .map(|addr| addr.clone())
            .collect::<HashSet<_, RandomState>>()
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

    /// Save announced peer addresses to the peer store. If the peer addresses are updated,
    /// return true, otherwise return false. This method will NOT keep the old announced addresses.
    fn save_announced_peer_addresses(&mut self, peer_id: PeerId, addr: Vec<Multiaddr>) -> bool {
        match self.announced_peer_addresses.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                if entry.get() == &addr {
                    false
                } else {
                    entry.insert(addr);
                    true
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(addr);
                true
            }
        }
    }

    pub(crate) fn sample_n_peers_to_connect(&self, n: usize) -> HashMap<PeerId, Vec<Multiaddr>> {
        let nodes = self
            .saved_peer_addresses
            .keys()
            .into_iter()
            .chain(self.announced_peer_addresses.keys().into_iter())
            .collect::<HashSet<_, RandomState>>();

        nodes
            .into_iter()
            .take(n)
            .map(|peer_id| {
                (
                    peer_id.clone(),
                    self.get_peer_addresses(peer_id).into_iter().collect(),
                )
            })
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

impl<S> NetworkActorState<S>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + NetworkGraphStateStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn get_or_create_new_node_announcement_message(&mut self) -> NodeAnnouncement {
        let now = std::time::UNIX_EPOCH
            .elapsed()
            .expect("Duration since unix epoch")
            .as_millis() as u64;
        match self.last_node_announcement_message {
            // If the last node announcement message is still relatively new, we don't need to create a new one.
            // Because otherwise the receiving node may be confused by the multiple announcements,
            // and falsely believe we updated the node announcement, and then forward this message to other nodes.
            // This is undesirable because we don't want to flood the network with the same message.
            // On the other hand, if the message is too old, we need to create a new one.
            Some(ref message) if now - message.version < 3600 * 1000 => {
                debug!("Node announcement message is still valid: {:?}", &message);
            }
            _ => {
                let alias = self.node_name.unwrap_or_default();
                let addresses = self.announced_addrs.clone();
                let announcement = NodeAnnouncement::new(
                    alias,
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

    pub fn should_message_be_broadcasted(&mut self, message: &FiberBroadcastMessage) -> bool {
        self.broadcasted_messages.insert(message.id())
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

    // Create a channel which will receive the response from the peer.
    // The channel will be closed after the response is received.
    pub fn create_request_id_for_reply_port(
        &mut self,
        peer_id: &PeerId,
        reply_port: RpcReplyPort<Result<(u64, bool), Error>>,
    ) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.broadcast_message_responses.insert(
            (peer_id.clone(), id),
            (RequestState::RequestSent, reply_port),
        );
        id
    }

    pub fn mark_request_finished(&mut self, peer_id: &PeerId, id: u64) {
        match self
            .broadcast_message_responses
            .remove(&(peer_id.clone(), id))
        {
            None => {
                error!(
                    "Invalid request id {} for peer {:?}, original request not found",
                    id, peer_id
                );
            }
            Some((RequestState::RequestReturned(next_offset, is_finished), reply)) => {
                let _ = reply.send(Ok((next_offset, is_finished)));
            }
            Some(state) => {
                error!(
                    "Invalid state for request id {} from peer {}: {:?}",
                    id, peer_id, &state
                );
            }
        }
    }

    pub fn mark_request_failed(&mut self, peer_id: &PeerId, id: u64, error: Error) {
        match self
            .broadcast_message_responses
            .remove(&(peer_id.clone(), id))
        {
            None => {
                error!(
                    "Invalid request id {} for peer {:?}, original request not found",
                    id, peer_id
                );
            }
            Some((_state, reply)) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub fn get_original_request_id(&mut self, peer_id: &PeerId, request_id: u64) -> Option<u64> {
        self.original_requests
            .remove(&(peer_id.clone(), request_id))
    }

    fn record_request_result(
        &mut self,
        peer_id: &PeerId,
        old_id: u64,
        next_offset: u64,
        is_finished: bool,
    ) -> Option<u64> {
        if !self
            .broadcast_message_responses
            .contains_key(&(peer_id.clone(), old_id))
        {
            return None;
        }
        self.broadcast_message_responses
            .get_mut(&(peer_id.clone(), old_id))
            .expect("key must exist in the hash map")
            .0 = RequestState::RequestReturned(next_offset, is_finished);
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.original_requests.insert((peer_id.clone(), id), old_id);
        Some(id)
    }

    pub async fn create_outbound_channel(
        &mut self,
        open_channel: OpenChannelCommand,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256), ProcessingChannelError> {
        let store = self.store.clone();
        let network = self.network.clone();
        let OpenChannelCommand {
            peer_id,
            funding_amount,
            public,
            shutdown_script,
            funding_udt_type_script,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            tlc_expiry_delta,
            tlc_min_value,
            tlc_max_value,
            tlc_fee_proportional_millionths,
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
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

        if let Some(_delta) = tlc_expiry_delta.filter(|&d| d < MIN_TLC_EXPIRY_DELTA) {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "TLC expiry delta is too small, expect larger than {}",
                MIN_TLC_EXPIRY_DELTA
            )));
        }

        let shutdown_script =
            shutdown_script.unwrap_or_else(|| self.default_shutdown_script.clone());

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
                    tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta),
                    tlc_min_value.unwrap_or(self.tlc_min_value),
                    tlc_max_value.unwrap_or(self.tlc_max_value),
                    tlc_fee_proportional_millionths.unwrap_or(self.tlc_fee_proportional_millionths),
                )),
                funding_udt_type_script,
                shutdown_script,
                channel_id_sender: tx,
                commitment_fee_rate,
                commitment_delay_epoch,
                funding_fee_rate,
                max_tlc_value_in_flight,
                max_tlc_number_in_flight,
            }),
            network.clone().get_cell(),
        )
        .await?
        .0;
        let temp_channel_id = rx.await.expect("msg received");
        self.on_channel_created(temp_channel_id, &peer_id, channel.clone());
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
                public_channel_info: open_channel.is_public().then_some(PublicChannelInfo::new(
                    self.tlc_expiry_delta,
                    self.tlc_min_value,
                    self.tlc_max_value,
                    self.tlc_fee_proportional_millionths,
                )),
                seed,
                open_channel,
                shutdown_script,
                channel_id_sender: Some(tx),
            }),
            network.clone().get_cell(),
        )
        .await?
        .0;
        let new_id = rx.await.expect("msg received");
        self.on_channel_created(new_id, &peer_id, channel.clone());
        Ok((channel, temp_channel_id, new_id))
    }

    // This function send the transaction to the network and then trace the transaction status.
    // Either the sending or the tracing may fail, in which case the callback will be called with
    // the error.
    async fn broadcast_tx_with_callback<F>(&self, transaction: TransactionView, callback: F)
    where
        F: Send + 'static + FnOnce(Result<TraceTxResponse, RactorErr<CkbChainMessage>>),
    {
        let chain = self.chain_actor.clone();
        // Spawn a new task to avoid blocking current actor message processing.
        ractor::concurrency::tokio_primatives::spawn(async move {
            debug!("Trying to broadcast transaction {:?}", &transaction);
            let result = match call_t!(
                &chain,
                CkbChainMessage::SendTx,
                DEFAULT_CHAIN_ACTOR_TIMEOUT,
                transaction.clone()
            )
            .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
            {
                Err(err) => {
                    error!("Failed to send transaction to the network: {:?}", &err);
                    // TODO: the caller of this function will deem the failure returned here as permanent.
                    // But SendTx may only fail temporarily. We need to handle this case.
                    Ok(TraceTxResponse {
                        tx: None,
                        status: TxStatus {
                            status: Status::Rejected,
                            block_number: None,
                            block_hash: None,
                            tx_index: None,
                            reason: Some(format!("Sending transaction failed: {:?}", &err)),
                        },
                    })
                }
                Ok(_) => {
                    let tx_hash = transaction.hash();
                    // TODO: make number of confirmation to transaction configurable.
                    const NUM_CONFIRMATIONS: u64 = 4;
                    let request = TraceTxRequest {
                        tx_hash: tx_hash.clone(),
                        confirmations: NUM_CONFIRMATIONS,
                    };
                    debug!(
                        "Transaction sent to the network, waiting for it to be confirmed: {:?}",
                        &request.tx_hash
                    );
                    call_t!(
                        chain,
                        CkbChainMessage::TraceTx,
                        DEFAULT_CHAIN_ACTOR_TIMEOUT,
                        request.clone()
                    )
                }
            };

            debug!("Transaction trace result: {:?}", &result);
            callback(result);
        });
    }

    fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        self.peer_session_map.get(peer_id).cloned()
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
        self.peer_session_map.values().take(n).cloned().collect()
    }

    fn get_peer_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.peer_pubkey_map.get(peer_id).cloned()
    }

    // TODO: this fn is duplicated with ChannelActorState::check_open_channel_parameters, but is not easy to refactor, just keep it for now.
    fn check_open_channel_parameters(
        &self,
        open_channel: &OpenChannel,
    ) -> Result<(), ProcessingChannelError> {
        let udt_type_script = &open_channel.funding_udt_type_script;

        // reserved_ckb_amount
        let occupied_capacity =
            occupied_capacity(&open_channel.shutdown_script, udt_type_script)?.as_u64();
        if open_channel.reserved_ckb_amount < occupied_capacity {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Reserved CKB amount {} is less than {}",
                open_channel.reserved_ckb_amount, occupied_capacity,
            )));
        }

        // funding_fee_rate
        if open_channel.funding_fee_rate < DEFAULT_FEE_RATE {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Funding fee rate is less than {}",
                DEFAULT_FEE_RATE,
            )));
        }

        // commitment_fee_rate
        if open_channel.commitment_fee_rate < DEFAULT_COMMITMENT_FEE_RATE {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee rate is less than {}",
                DEFAULT_COMMITMENT_FEE_RATE,
            )));
        }
        let commitment_fee =
            calculate_commitment_tx_fee(open_channel.commitment_fee_rate, udt_type_script);
        let reserved_fee = open_channel.reserved_ckb_amount - occupied_capacity;
        if commitment_fee * 2 > reserved_fee {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee {} which caculated by commitment fee rate {} is larger than half of reserved fee {}",
                commitment_fee, open_channel.commitment_fee_rate, reserved_fee
            )));
        }

        // commitment_delay_epoch
        let epoch =
            EpochNumberWithFraction::from_full_value_unchecked(open_channel.commitment_delay_epoch);
        if !epoch.is_well_formed() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment delay epoch {} is not a valid value",
                open_channel.commitment_delay_epoch,
            )));
        }

        let min = EpochNumberWithFraction::new(MIN_COMMITMENT_DELAY_EPOCHS, 0, 1);
        if epoch < min {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment delay epoch {} is less than the minimal value {}",
                epoch, min
            )));
        }

        let max = EpochNumberWithFraction::new(MAX_COMMITMENT_DELAY_EPOCHS, 0, 1);
        if epoch > max {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment delay epoch {} is greater than the maximal value {}",
                epoch, max
            )));
        }

        // max_tlc_number_in_flight
        if open_channel.max_tlc_number_in_flight > SYS_MAX_TLC_NUMBER_IN_FLIGHT {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Max TLC number in flight {} is greater than the system maximal value {}",
                open_channel.max_tlc_number_in_flight, SYS_MAX_TLC_NUMBER_IN_FLIGHT
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
        match command {
            // Need to handle the force shutdown command specially because the ChannelActor may not exist when remote peer is disconnected.
            ChannelCommand::Shutdown(shutdown, rpc_reply) if shutdown.force => {
                match self.store.get_channel_actor_state(&channel_id) {
                    Some(mut state) => {
                        match state.state {
                            ChannelState::ChannelReady() => {
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

                        let transaction = state
                            .latest_commitment_transaction
                            .clone()
                            .expect("latest_commitment_transaction should exist when channel is in ChannelReady of ShuttingDown state");
                        self.network
                            .send_message(NetworkActorMessage::new_event(
                                NetworkActorEvent::CommitmentTransactionPending(
                                    transaction,
                                    channel_id,
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
            _ => match self.channels.get(&channel_id) {
                Some(actor) => {
                    actor.send_message(ChannelActorMessage::Command(command))?;
                    Ok(())
                }
                None => Err(Error::ChannelNotFound(channel_id)),
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
                self.channel_subscribers.clone(),
            ),
            ChannelInitializationParameter::ReestablishChannel(channel_id),
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
        let store = self.store.clone();
        self.peer_session_map
            .insert(remote_peer_id.clone(), session.id);
        self.peer_pubkey_map
            .insert(remote_peer_id.clone(), remote_pubkey);

        if self.auto_announce {
            let message = self.get_or_create_new_node_announcement_message();
            debug!(
                "Auto announcing our node to peer {:?} (message: {:?})",
                remote_peer_id, &message
            );
            if let Err(e) = self
                .send_message_to_session(
                    session.id,
                    FiberMessage::BroadcastMessage(FiberBroadcastMessage::NodeAnnouncement(
                        message,
                    )),
                )
                .await
            {
                error!(
                    "Failed to send NodeAnnouncement message to peer {:?}: {:?}",
                    remote_peer_id, e
                );
            }
        } else {
            debug!(
                "Auto announcing is disabled, skipping node announcement to peer {:?}",
                remote_peer_id
            );
        }

        for channel_id in store.get_active_channel_ids_by_peer(remote_peer_id) {
            if let Err(e) = self.reestablish_channel(remote_peer_id, channel_id).await {
                error!("Failed to reestablish channel {:x}: {:?}", &channel_id, &e);
            }
        }
        self.maybe_sync_network_graph(remote_peer_id).await;
    }

    fn remove_channel(&mut self, channel_id: &Hash256) -> Option<ActorRef<ChannelActorMessage>> {
        self.channels
            .remove(channel_id)
            .inspect(|_| self.outpoint_channel_map.retain(|_, v| v != channel_id))
    }

    fn on_peer_disconnected(&mut self, id: &PeerId) {
        info!("Peer {:?} disconnected from us ({:?})", id, &self.peer_id);
        if let Some(session) = self.peer_session_map.remove(id) {
            if let Some(channel_ids) = self.session_channels_map.remove(&session) {
                for channel_id in channel_ids {
                    if let Some(channel) = self.remove_channel(&channel_id) {
                        let _ = channel.send_message(ChannelActorMessage::Event(
                            ChannelEvent::PeerDisconnected,
                        ));
                    }
                }
            }
        }
        self.maybe_tell_syncer_peer_disconnected(id);
    }

    pub(crate) fn get_peer_addresses(&self, peer_id: &PeerId) -> HashSet<Multiaddr> {
        self.state_to_be_persisted.get_peer_addresses(peer_id)
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

    pub(crate) fn save_announced_peer_addresses(
        &mut self,
        peer_id: PeerId,
        addresses: Vec<Multiaddr>,
    ) {
        if self
            .state_to_be_persisted
            .save_announced_peer_addresses(peer_id, addresses)
        {
            self.persist_state();
        }
    }

    fn persist_state(&self) {
        self.store
            .insert_network_actor_state(&self.peer_id, self.state_to_be_persisted.clone());
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
                syncer.stop(Some("Peer disconnected".to_string()));
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
        self.remove_channel(channel_id);
        if let Some(session) = self.get_peer_session(peer_id) {
            if let Some(set) = self.session_channels_map.get_mut(&session) {
                set.remove(channel_id);
            }
        }
        self.send_message_to_channel_actor(
            *channel_id,
            None,
            ChannelActorMessage::Event(ChannelEvent::ClosingTransactionConfirmed),
        )
        .await;
        // Notify outside observers.
        self.network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::ChannelClosed(peer_id.clone(), *channel_id, tx_hash),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
    }

    pub async fn on_open_channel_msg(
        &mut self,
        peer_id: PeerId,
        open_channel: OpenChannel,
    ) -> ProcessingChannelResult {
        self.check_open_channel_parameters(&open_channel)?;

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
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, id),
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
                            block_number: Some(block_number),
                            ..
                        },
                    ..
                }) => {
                    info!("Funding transaction {:?} confirmed", &tx_hash);
                    NetworkActorEvent::FundingTransactionConfirmed(
                        outpoint,
                        block_number.into(),
                        DUMMY_FUNDING_TX_INDEX,
                    )
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

    async fn on_funding_transaction_confirmed(
        &mut self,
        outpoint: OutPoint,
        block_number: BlockNumber,
        tx_index: u32,
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
                block_number,
                tx_index,
            )),
        )
        .await;
    }

    async fn on_commitment_transaction_confirmed(&mut self, tx_hash: Hash256, channel_id: Hash256) {
        debug!("Commitment transaction is confirmed: {:?}", tx_hash);
        self.send_message_to_channel_actor(
            channel_id,
            None,
            ChannelActorMessage::Event(ChannelEvent::CommitmentTransactionConfirmed),
        )
        .await;
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
                // There is some chance that the peer send a message related to a channel that is not created yet,
                // e.g. when we just started trying to reestablish channel, we may have
                // no reference to that channel yet.
                // We should stash the message and process it later.
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
                actor.send_message(message).expect("channel actor alive");
            }
        }
    }
}

pub struct NetworkActorStartArguments {
    pub config: FiberConfig,
    pub tracker: TaskTracker,
    pub channel_subscribers: ChannelSubscribers,
    pub default_shutdown_script: Script,
}

#[rasync_trait]
impl<S> Actor for NetworkActor<S>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + NetworkGraphStateStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    type Msg = NetworkActorMessage;
    type State = NetworkActorState<S>;
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
            default_shutdown_script,
        } = args;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("SystemTime::now() should after UNIX_EPOCH");
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let private_key = <[u8; 32]>::try_from(kp.as_ref())
            .expect("valid length for key")
            .into();
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

        if !config.announce_private_addr.unwrap_or_default() {
            announced_addrs.retain(|addr| {
                multiaddr_to_socketaddr(addr)
                    .map(|socket_addr| is_reachable(socket_addr.ip()))
                    .unwrap_or_default()
            });
        }

        info!(
            "Started listening tentacle on {:?}, peer id {:?}, announced addresses {:?}",
            &listening_addr, &my_peer_id, &announced_addrs
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

        tracker.spawn(async move {
            service.run().await;
            debug!("Tentacle service stopped");
        });

        let mut graph = self.network_graph.write().await;

        let mut state_to_be_persisted = self
            .store
            .get_network_actor_state(&my_peer_id)
            .unwrap_or_default();

        for bootnode in &config.bootnode_addrs {
            let addr = Multiaddr::from_str(bootnode.as_str()).expect("valid bootnode");
            let peer_id = extract_peer_id(&addr).expect("valid peer id");
            state_to_be_persisted.save_peer_address(peer_id, addr);
        }

        let height = graph.get_best_height();
        let last_update = graph.get_last_update_timestamp();

        let chain_actor = self.chain_actor.clone();
        let current_block_number = call!(chain_actor, CkbChainMessage::GetCurrentBlockNumber, ())
            .expect(ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW)
            .expect("Get current block number from chain");
        let sync_status = NetworkSyncStatus::new(
            config.sync_network_graph(),
            height,
            current_block_number,
            last_update,
            vec![],
        );

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
            tlc_expiry_delta: config.tlc_expiry_delta(),
            tlc_min_value: config.tlc_min_value(),
            tlc_max_value: config.tlc_max_value(),
            tlc_fee_proportional_millionths: config.tlc_fee_proportional_millionths(),
            announce_private_addr: config.announce_private_addr.unwrap_or_default(),
            broadcasted_messages: Default::default(),
            channel_subscribers,
            next_request_id: Default::default(),
            broadcast_message_responses: Default::default(),
            original_requests: Default::default(),
            sync_status,
            broadcasted_message_queue: Default::default(),
        };

        // Save our own NodeInfo to the network graph.
        let node_announcement = state.get_or_create_new_node_announcement_message();
        graph.process_node_announcement(node_announcement);

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
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Trying to connect to peers with mutual channels");
        for (peer_id, channel_id, channel_state) in self.store.get_channel_states(None) {
            let addresses = state.get_peer_addresses(&peer_id);

            debug!(
                "Reconnecting channel {:x} peers {:?} in state {:?}",
                &channel_id, &peer_id, &channel_state
            );
            for addr in addresses {
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::ConnectPeer(addr),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }

        myself
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::MaintainConnections(NUM_PEER_CONNECTIONS),
            ))
            .expect(ASSUME_NETWORK_MYSELF_ALIVE);
        myself.send_interval(MAINTAINING_CONNECTIONS_INTERVAL, || {
            NetworkActorMessage::new_command(NetworkActorCommand::MaintainConnections(
                NUM_PEER_CONNECTIONS,
            ))
        });
        Ok(())
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
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
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
            "proto id [{}] close on session [{}], address: [{}], type: [{:?}]",
            context.proto_id, context.session.id, &context.session.address, &context.session.ty
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
        trace!("Service error: {:?}", error);
        // TODO
        // ServiceError::DialerError => remove address from peer store
        // ServiceError::ProtocolError => ban peer
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        trace!("Service event: {:?}", event);
    }
}

pub async fn start_network<
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + NetworkGraphStateStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
>(
    config: FiberConfig,
    chain_actor: ActorRef<CkbChainMessage>,
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    tracker: TaskTracker,
    root_actor: ActorCell,
    store: S,
    channel_subscribers: ChannelSubscribers,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
    default_shutdown_script: Script,
) -> ActorRef<NetworkActorMessage> {
    let my_pubkey = config.public_key();
    let my_peer_id = PeerId::from_public_key(&my_pubkey);

    let (actor, _handle) = Actor::spawn_linked(
        Some(format!("Network {}", my_peer_id)),
        NetworkActor::new(event_sender, chain_actor, store, network_graph),
        NetworkActorStartArguments {
            config,
            tracker,
            channel_subscribers,
            default_shutdown_script,
        },
        root_actor,
    )
    .await
    .expect("Failed to start network actor");

    actor
}
