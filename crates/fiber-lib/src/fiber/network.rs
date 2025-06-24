use ckb_hash::blake2b_256;
use ckb_types::core::{EpochNumberWithFraction, FeeRate, TransactionView};
use ckb_types::packed::{Byte32, OutPoint, Script, Transaction};
use ckb_types::prelude::{IntoTransactionView, Pack, Unpack};
use ckb_types::H256;
use either::Either;
use once_cell::sync::OnceCell;
use ractor::concurrency::Duration;
use ractor::{
    async_trait as rasync_trait, call, call_t, Actor, ActorCell, ActorProcessingErr, ActorRef,
    RactorErr, RpcReplyPort, SupervisionEvent,
};
use rand::Rng;
use secp256k1::Secp256k1;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tentacle::multiaddr::{MultiAddr, Protocol};
use tentacle::service::SessionType;
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
    get_funding_and_reserved_amount, occupied_capacity, AcceptChannelParameter,
    AwaitingTxSignaturesFlags, ChannelActor, ChannelActorMessage, ChannelActorStateStore,
    ChannelCommand, ChannelCommandWithId, ChannelEvent, ChannelInitializationParameter,
    ChannelState, ChannelSubscribers, ChannelTlcInfo, OpenChannelParameter, PrevTlcInfo,
    ProcessingChannelError, ProcessingChannelResult, PublicChannelInfo, RemoveTlcCommand,
    RevocationData, SettlementData, ShuttingDownFlags, StopReason, TLCId,
    DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE, DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
    MAX_COMMITMENT_DELAY_EPOCHS, MAX_TLC_NUMBER_IN_FLIGHT, MIN_COMMITMENT_DELAY_EPOCHS,
    SYS_MAX_TLC_NUMBER_IN_FLIGHT,
};
use super::config::{AnnouncedNodeName, DEFAULT_MAX_PARTS, MIN_TLC_EXPIRY_DELTA};
use super::features::FeatureVector;
use super::fee::calculate_commitment_tx_fee;
use super::gossip::{GossipActorMessage, GossipMessageStore, GossipMessageUpdates};
use super::graph::{
    Attempt, NetworkGraph, NetworkGraphStateStore, OwnedChannelUpdateEvent, RouterHop, SessionRoute,
};
use super::key::blake2b_hash_with_salt;
use super::types::{
    BroadcastMessageWithTimestamp, EcdsaSignature, FiberMessage, ForwardTlcResult, GossipMessage,
    Hash256, Init, NodeAnnouncement, OpenChannel, PaymentDataRecord, PaymentHopData, Privkey,
    Pubkey, RemoveTlcFulfill, RemoveTlcReason, TlcErr, TlcErrData, TlcErrorCode,
};
use super::{
    FiberConfig, InFlightCkbTxActor, InFlightCkbTxActorArguments, InFlightCkbTxActorMessage,
    InFlightCkbTxKind, ASSUME_NETWORK_ACTOR_ALIVE,
};

use crate::ckb::config::UdtCfgInfos;
use crate::ckb::contracts::{check_udt_script, get_udt_whitelist, is_udt_type_auto_accept};
use crate::ckb::{CkbChainMessage, FundingRequest, FundingTx};
use crate::fiber::channel::{
    AddTlcCommand, AddTlcResponse, ShutdownCommand, TxCollaborationCommand, TxUpdateCommand,
};
use crate::fiber::config::{
    DEFAULT_MPP_MIN_AMOUNT, DEFAULT_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT,
    PAYMENT_MAX_PARTS_LIMIT,
};
use crate::fiber::gossip::{GossipConfig, GossipService, SubscribableGossipMessageStore};
use crate::fiber::graph::{AttemptStatus, GraphChannelStat, PaymentSession, PaymentStatus};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::types::{
    FiberChannelMessage, PaymentOnionPacket, PeeledPaymentOnionPacket, TlcErrPacket, TxSignatures,
};
use crate::fiber::KeyPair;
use crate::invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceStore, PreimageStore};
use crate::{now_timestamp_as_millis_u64, unwrap_or_return, Error};

pub const FIBER_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

pub const GOSSIP_PROTOCOL_ID: ProtocolId = ProtocolId::new(43);

pub const DEFAULT_CHAIN_ACTOR_TIMEOUT: u64 = 300000;

// TODO: make it configurable
pub const CKB_TX_TRACING_CONFIRMATIONS: u64 = 4;

pub const DEFAULT_PAYMENT_TRY_LIMIT: u32 = 5;
pub const DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT: u32 = 3;

// This is a temporary way to document that we assume the chain actor is always alive.
// We may later relax this assumption. At the moment, if the chain actor fails, we
// should panic with this message, and later we may find all references to this message
// to make sure that we handle the case where the chain actor is not alive.
const ASSUME_CHAIN_ACTOR_ALWAYS_ALIVE_FOR_NOW: &str =
    "We currently assume that chain actor is always alive, but it failed. This is a known issue.";

const ASSUME_NETWORK_MYSELF_ALIVE: &str = "network actor myself alive";

const ASSUME_GOSSIP_ACTOR_ALIVE: &str = "gossip actor must be alive";

// The duration for which we will try to maintain the number of peers in connection.
const MAINTAINING_CONNECTIONS_INTERVAL: Duration = Duration::from_secs(3600);

// The duration for which we will check all channels.
#[cfg(debug_assertions)]
const CHECK_CHANNELS_INTERVAL: Duration = Duration::from_secs(3); // use a short interval for debugging build
#[cfg(not(debug_assertions))]
const CHECK_CHANNELS_INTERVAL: Duration = Duration::from_secs(60);

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
    pub status: PaymentStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub failed_error: Option<String>,
    pub custom_records: Option<PaymentCustomRecords>,
    pub fee: u128,
    #[cfg(any(debug_assertions, feature = "bench"))]
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

    /// A list of multi-addresses associated with the peer.
    pub addresses: Vec<MultiAddr>,
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
    MaintainConnections,
    // Check all channels and see if we need to force close any of them or settle down tlc with preimage.
    CheckChannels,
    // Timeout a hold tlc
    TimeoutHoldTlc(Hash256, Hash256, u64),
    // Settle MPP tlc set
    SettleMPPTlcSet(Hash256),
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
    // The first parameter is the peeled onion in binary via `PeeledOnionPacket::serialize`. `PeeledOnionPacket::current`
    // is for the current node.
    SendPaymentOnionPacket(SendOnionPacketCommand),
    PeelPaymentOnionPacket(
        PaymentOnionPacket, // onion_packet
        Hash256,            // payment_hash
        RpcReplyPort<Result<PeeledPaymentOnionPacket, String>>,
    ),
    UpdateChannelFunding(Hash256, Transaction, FundingRequest),
    SignFundingTx(PeerId, Hash256, Transaction, Option<Vec<Vec<u8>>>),
    NotifyFundingTx(Transaction),
    // Broadcast our BroadcastMessage to the network.
    BroadcastMessages(Vec<BroadcastMessageWithTimestamp>),
    // Broadcast local information to the network.
    BroadcastLocalInfo(LocalInfoKind),
    SignMessage([u8; 32], RpcReplyPort<EcdsaSignature>),
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

    NodeInfo((), RpcReplyPort<Result<NodeInfoResponse, String>>),
    ListPeers((), RpcReplyPort<Result<Vec<PeerInfo>, String>>),
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
    pub tlc_fee_proportional_millionths: Option<u128>,
    pub max_tlc_value_in_flight: Option<u128>,
    pub max_tlc_number_in_flight: Option<u64>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
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
    // custom records
    pub custom_records: Option<PaymentCustomRecords>,
    // the hop hint which may help the find path algorithm to find the path
    pub hop_hints: Option<Vec<HopHint>>,
    // dry_run only used for checking, default is false
    pub dry_run: bool,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SendPaymentWithRouterCommand {
    /// the hash to use within the payment's HTLC
    pub payment_hash: Option<Hash256>,

    /// The router to use for the payment
    pub router: Vec<RouterHop>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// Some custom records for the payment which contains a map of u32 to Vec<u8>
    /// The key is the record type, and the value is the serialized data
    /// For example:
    /// ```json
    /// "custom_records": {
    ///    "0x1": "0x01020304",
    ///    "0x2": "0x05060708",
    ///    "0x3": "0x090a0b0c",
    ///    "0x4": "0x0d0e0f10010d090a0b0c"
    ///  }
    /// ```
    pub custom_records: Option<PaymentCustomRecords>,

    /// keysend payment
    pub keysend: Option<bool>,

    /// udt type script for the payment
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
    pub dry_run: bool,
}

/// The custom records to be included in the payment.
/// The key is hex encoded of `u32`, and the value is hex encoded of `Vec<u8>` with `0x` as prefix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    pub data: HashMap<u32, Vec<u8>>,
}

/// A hop hint is a hint for a node to use a specific channel,
/// will usually used for the last hop to the target node.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopHint {
    /// The public key of the node
    pub(crate) pubkey: Pubkey,
    /// The outpoint for the channel
    #[serde_as(as = "EntityHex")]
    pub(crate) channel_outpoint: OutPoint,
    /// The fee rate to use this hop to forward the payment.
    pub(crate) fee_rate: u64,
    /// The TLC expiry delta to use this hop to forward the payment.
    pub(crate) tlc_expiry_delta: u64,
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
    /// The number of parts for the payment, only used for multi-part payment
    pub max_parts: Option<u64>,
    pub keysend: bool,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub preimage: Option<Hash256>,
    pub custom_records: Option<PaymentCustomRecords>,
    pub allow_self_payment: bool,
    pub hop_hints: Vec<HopHint>,
    pub router: Vec<RouterHop>,
    pub allow_mpp: bool,
    pub dry_run: bool,
    #[serde(skip)]
    pub channel_stats: GraphChannelStat,
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
        if !(MIN_TLC_EXPIRY_DELTA..=MAX_PAYMENT_TLC_EXPIRY_LIMIT).contains(&final_tlc_expiry_delta)
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

        if udt_type_script.is_none() && amount >= u64::MAX as u128 {
            return Err(format!(
                "The payment amount ({}) should be less than {}",
                amount,
                u64::MAX
            ));
        }

        let max_fee_amount = command.max_fee_amount.unwrap_or(0);
        if amount.checked_add(max_fee_amount).is_none() {
            return Err(format!(
                "amount + max_fee_amount overflow: amount = {}, max_fee_amount = {}",
                amount, max_fee_amount
            ));
        }

        let hop_hints = command.hop_hints.unwrap_or_default();

        let allow_mpp = invoice.as_ref().is_some_and(|inv| inv.allow_mpp());
        let payment_secret = invoice
            .as_ref()
            .and_then(|inv| inv.payment_secret().cloned());
        if allow_mpp && payment_secret.is_none() {
            return Err("payment secret is required for multi-path payment".to_string());
        }
        if allow_mpp
            && command
                .max_parts
                .is_some_and(|max_parts| max_parts <= 1 || max_parts > PAYMENT_MAX_PARTS_LIMIT)
        {
            return Err(format!(
                "invalid max_parts, value should be in range [1, {}]",
                PAYMENT_MAX_PARTS_LIMIT
            ));
        }

        let mut custom_records = command.custom_records;

        // bolt04 write payment data record to custom records if payment secret is set
        if let Some(payment_secret) = payment_secret {
            if custom_records.is_none() {
                custom_records = Some(PaymentCustomRecords::default());
            }
            let records = custom_records.as_mut().unwrap();

            if records.data.contains_key(&PaymentDataRecord::RECORD_TYPE) {
                return Err("custom_records should not contain payment_data_record".to_string());
            }

            let payment_data_record = PaymentDataRecord::new(payment_secret, amount);
            payment_data_record.write(records);
        }

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
            custom_records,
            allow_self_payment: command.allow_self_payment,
            hop_hints,
            allow_mpp,
            router: vec![],
            dry_run: command.dry_run,
            channel_stats: Default::default(),
        })
    }

    pub fn max_parts(&self) -> usize {
        self.max_parts.unwrap_or(DEFAULT_MAX_PARTS) as usize
    }

    pub fn allow_mpp(&self) -> bool {
        // only allow mpp if max_parts is greater than 1 and not keysend
        self.allow_mpp && self.max_parts() > 1 && !self.keysend
    }

    pub fn allow_minimal_amount(&self) -> u128 {
        if self.allow_mpp() {
            DEFAULT_MPP_MIN_AMOUNT
        } else {
            self.amount
        }
    }
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

#[derive(Debug, Clone)]
pub struct SendOnionPacketCommand {
    pub peeled_onion_packet: PeeledPaymentOnionPacket,
    // We are currently forwarding a previous tlc. The previous tlc's channel id, tlc id
    // and the fee paid are included here.
    pub previous_tlc: Option<PrevTlcInfo>,
    pub payment_hash: Hash256,
    pub attempt_id: Option<u64>,
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
                NetworkServiceEvent::DebugEvent(DebugEvent::Common($debug_event.to_string())),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    };
}

#[derive(Clone, Debug)]
pub enum NetworkServiceEvent {
    NetworkStarted(PeerId, MultiAddr, Vec<Multiaddr>),
    NetworkStopped(PeerId),
    PeerConnected(PeerId, Multiaddr),
    PeerDisConnected(PeerId, Multiaddr),
    // An incoming/outgoing channel is created.
    ChannelCreated(PeerId, Hash256),
    // A outgoing channel is pending to be accepted.
    ChannelPendingToBeAccepted(PeerId, Hash256),
    // A funding tx is completed. The watch tower may use this to monitor the channel.
    RemoteTxComplete(PeerId, Hash256, Script, SettlementData),
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
#[derive(Debug)]
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

    /// A closing transaction has been confirmed.
    ClosingTransactionConfirmed(PeerId, Hash256, Byte32, bool),

    /// A closing transaction has failed (either because of invalid transaction or timeout)
    ClosingTransactionFailed(PeerId, Hash256, Byte32),

    // A tlc remove message is received. (payment_hash, attempt_id, remove_tlc)
    TlcRemoveReceived(Hash256, Option<u64>, RemoveTlcReason),

    // A payment need to retry
    RetrySendPayment(Hash256, Option<u64>),

    // AddTlc result from peer (payment_hash, attempt_id, (process_channel_error, tlc_err), (previous_channel_id, previous_tlc_id))
    AddTlcResult(
        Hash256,
        Option<u64>,
        Option<(ProcessingChannelError, TlcErr)>,
        Option<PrevTlcInfo>,
    ),

    // An owned channel is updated.
    OwnedChannelUpdateEvent(OwnedChannelUpdateEvent),

    // A channel actor stopped event.
    ChannelActorStopped(Hash256, StopReason),
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
pub struct GossipMessageWithPeerId {
    pub peer_id: PeerId,
    pub message: GossipMessage,
}

impl GossipMessageWithPeerId {
    pub fn new(peer_id: PeerId, message: GossipMessage) -> Self {
        Self { peer_id, message }
    }
}

pub struct NetworkActor<S> {
    // An event emitter to notify outside observers.
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
        + GossipMessageStore
        + PreimageStore
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
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
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
                                max_tlc_number_in_flight: None,
                                max_tlc_value_in_flight: None,
                                min_tlc_value: None,
                                tlc_fee_proportional_millionths: None,
                                tlc_expiry_delta: None,
                            };
                            state.create_inbound_channel(accept_channel).await?;
                        }
                    }
                    Err(err) => {
                        error!("Failed to process OpenChannel message: {}", err);
                    }
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
        };
        Ok(())
    }

    // We normally don't need to manually call this to update graph from store data,
    // because network actor will automatically update the graph when it receives
    // updates. But in some standalone tests, we may need to manually update the graph.
    async fn update_graph(&self) {
        let mut graph = self.network_graph.write().await;
        graph.load_from_store();
    }

    pub async fn handle_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
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

                // FIXME(yukang): need to make sure ChannelReady is sent after the channel is reestablished
                state
                    .outpoint_channel_map
                    .insert(channel_outpoint.clone(), channel_id);

                // Notify outside observers.
                myself
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::ChannelReady(
                            peer_id,
                            channel_id,
                            channel_outpoint.clone(),
                        ),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);

                // retry related payment attempts for this channel
                for attempt in self
                    .store
                    .get_attempts_with_statuses(&[AttemptStatus::Created, AttemptStatus::Retrying])
                {
                    if attempt.first_hop_channel_outpoint_eq(&channel_outpoint) {
                        debug!(
                            "Now retrying payment attempt {:?} for channel {:?} reestablished",
                            attempt.payment_hash, channel_id
                        );
                        self.register_payment_retry(
                            myself.clone(),
                            attempt.payment_hash,
                            Some(attempt.id),
                        );
                    }
                }
            }
            NetworkActorEvent::FiberMessage(peer_id, message) => {
                self.handle_peer_message(myself, state, peer_id, message)
                    .await?
            }
            NetworkActorEvent::FundingTransactionPending(transaction, outpoint, channel_id) => {
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
            NetworkActorEvent::ClosingTransactionConfirmed(peer_id, channel_id, tx_hash, force) => {
                state
                    .on_closing_transaction_confirmed(&peer_id, &channel_id, tx_hash, force)
                    .await;
            }
            NetworkActorEvent::ClosingTransactionFailed(peer_id, tx_hash, channel_id) => {
                error!(
                    "Closing transaction failed for channel {:?}, tx hash: {:?}, peer id: {:?}",
                    &channel_id, &tx_hash, &peer_id
                );
                state.remove_in_flight_tx(tx_hash);
            }
            NetworkActorEvent::TlcRemoveReceived(payment_hash, attempt_id, remove_tlc_reason) => {
                // When a node is restarted, RemoveTLC will also be resent if necessary
                self.on_remove_tlc_event(myself, payment_hash, attempt_id, remove_tlc_reason)
                    .await;
            }
            NetworkActorEvent::RetrySendPayment(payment_hash, attempt_id) => {
                let _ = self
                    .resume_payment_session(myself, state, payment_hash, attempt_id)
                    .await;
            }
            NetworkActorEvent::AddTlcResult(payment_hash, attempt_id, error_info, previous_tlc) => {
                self.on_add_tlc_result_event(
                    myself,
                    payment_hash,
                    attempt_id,
                    error_info,
                    previous_tlc,
                )
                .await;
            }
            NetworkActorEvent::GossipMessageUpdates(gossip_message_updates) => {
                let mut graph = self.network_graph.write().await;
                graph.update_for_messages(gossip_message_updates.messages);
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
                state.on_channel_actor_stopped(channel_id, reason).await;
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
            NetworkActorCommand::MaintainConnections => {
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
            NetworkActorCommand::CheckChannels => {
                let now = now_timestamp_as_millis_u64();

                for (_peer_id, channel_id, channel_state) in self.store.get_channel_states(None) {
                    if matches!(channel_state, ChannelState::ChannelReady) {
                        if let Some(actor_state) = self.store.get_channel_actor_state(&channel_id) {
                            if actor_state.reestablishing {
                                continue;
                            }

                            for tlc in actor_state.tlc_state.received_tlcs.get_committed_tlcs() {
                                // skip if tlc amount is not fulfilled invoice
                                // this may happened if payment is mpp
                                if let Some(invoice) = self.store.get_invoice(&tlc.payment_hash) {
                                    if let Some(amount) = invoice.amount() {
                                        if tlc.amount < amount {
                                            continue;
                                        }
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
                                        "Failed to remove tlc {:?} for channel {:?}: {}",
                                        tlc.id(),
                                        channel_id,
                                        err
                                    );
                                }
                            }

                            if actor_state
                                .tlc_state
                                .offered_tlcs
                                .get_committed_tlcs()
                                .iter()
                                .any(|tlc| tlc.expiry < now)
                            {
                                debug!(
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
                                                close_script: Script::default(),
                                                fee_rate: FeeRate::default(),
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
                    }
                }
            }
            NetworkActorCommand::SettleMPPTlcSet(payment_hash) => {
                // load hold tlcs
                let tlcs: Vec<_> = self
                    .store
                    .get_hold_tlc_set(payment_hash)
                    .iter()
                    .filter_map(|hold_tlc| {
                        let state = self.store.get_channel_actor_state(&hold_tlc.channel_id)?;
                        let tlc_id = TLCId::Received(hold_tlc.tlc_id);
                        state.get_received_tlc(tlc_id).cloned()
                    })
                    .collect();

                let Some(first_tlc) = tlcs.first() else {
                    // no tlcs to settle
                    return Ok(());
                };

                let mut tlc_fail = None;

                // check if all tlcs have the same total amount
                // if yes, check if tlc set are fulfilled
                if tlcs
                    .iter()
                    .any(|t| t.total_amount != first_tlc.total_amount)
                {
                    error!("one tlc total_amount is not equal to current tlc total_amount");
                    tlc_fail = Some(TlcErr::new(TlcErrorCode::IncorrectOrUnknownPaymentDetails));
                } else {
                    let total_amount = first_tlc.total_amount.unwrap_or(first_tlc.amount);
                    let total_tlc_amount = tlcs.iter().map(|tlc| tlc.amount).sum::<u128>();
                    debug!(
                        "checking total_tlc_amount: {}, total_amount: {}",
                        total_tlc_amount, total_amount
                    );
                    let is_fulfilled = total_tlc_amount >= total_amount;
                    // wait for all tlcs to be fulfilled
                    if !is_fulfilled {
                        return Ok(());
                    }
                }

                let Some(preimage) = self.store.get_preimage(&payment_hash) else {
                    return Ok(());
                };

                // remove tlcs
                for tlc in tlcs {
                    let (send, _recv) = oneshot::channel();
                    let rpc_reply = RpcReplyPort::from(send);
                    let remove_reason = match tlc_fail.clone() {
                        Some(tlc_fail) => RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                            tlc_fail,
                            &tlc.shared_secret,
                        )),
                        None => RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage,
                        }),
                    };
                    match state
                        .send_command_to_channel(
                            tlc.channel_id,
                            ChannelCommand::RemoveTlc(
                                RemoveTlcCommand {
                                    id: tlc.id(),
                                    reason: remove_reason,
                                },
                                rpc_reply,
                            ),
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(err) => {
                            error!(
                                "Failed to remove tlc {:?} for channel {:?}: {}",
                                tlc.id(),
                                tlc.channel_id,
                                err
                            );
                        }
                    }
                }

                // remove settled hold tlcs
                self.store.remove_hold_tlc_set(&payment_hash);
            }
            NetworkActorCommand::TimeoutHoldTlc(payment_hash, channel_id, tlc_id) => {
                debug!(
                    "Remove timeout hold tlc payment hash {:?} tlc id {:?}",
                    payment_hash, tlc_id
                );
                let channel_actor_state = self.store.get_channel_actor_state(&channel_id);
                let tlc = channel_actor_state
                    .as_ref()
                    .and_then(|state| state.tlc_state.get(&TLCId::Received(tlc_id)));
                let Some(tlc) = tlc else {
                    debug!(
                        "Timeout tlc {:?} (payment hash {:?}) for channel {:?}: tlc is settled or not found, just unhold it",
                        tlc_id, payment_hash, channel_id
                    );
                    // remove hold tlc from store
                    self.store
                        .remove_hold_tlc(&payment_hash, &channel_id, tlc_id);
                    return Ok(());
                };

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
                        // remove hold tlc from store
                        self.store
                            .remove_hold_tlc(&payment_hash, &channel_id, tlc_id);
                    }
                    Err(err) => {
                        error!(
                            "Failed to remove tlc {:?} for channel {:?}: {}",
                            tlc.id(),
                            channel_id,
                            err
                        );
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
            NetworkActorCommand::SendPaymentOnionPacket(command) => {
                if let Err(err) = self
                    .handle_send_onion_packet_command(state, command.clone())
                    .await
                {
                    self.on_add_tlc_result_event(
                        myself,
                        command.payment_hash,
                        command.attempt_id,
                        Some((ProcessingChannelError::TlcForwardingError(err.clone()), err)),
                        command.previous_tlc,
                    )
                    .await;
                }
            }
            NetworkActorCommand::PeelPaymentOnionPacket(onion_packet, payment_hash, reply) => {
                let response = onion_packet
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
                        error!("Failed to fund channel: {}", err);
                        state.abort_funding(Either::Left(channel_id)).await;
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
                let msg = match partial_witnesses {
                    Some(partial_witnesses) => {
                        debug!(
                            "Received SignFudningTx request with for transaction {:?} and partial witnesses {:?}",
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
                                }),
                            ),
                        }
                    }
                    None => {
                        debug!(
                                    "Received SignFundingTx request with for transaction {:?} without partial witnesses, so start signing it now",
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
                                }),
                            ),
                        }
                    }
                };

                state
                    .trace_tx(tx_hash, InFlightCkbTxKind::Funding(*channel_id))
                    .await?;

                // TODO: before sending the signatures to the peer, start tracing the tx
                // It should be the first time to trace the tx

                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(msg),
                    ))
                    .expect("network actor alive");
            }
            NetworkActorCommand::BroadcastMessages(message) => {
                state
                    .gossip_actor
                    .send_message(GossipActorMessage::TryBroadcastMessages(message))
                    .expect(ASSUME_GOSSIP_ACTOR_ALIVE);
            }
            NetworkActorCommand::SignMessage(message, reply) => {
                debug!(
                    "Signing message with node private key: message {:?}, public key {:?}",
                    message,
                    state.get_public_key()
                );
                let signature = state.private_key.sign(message);
                let _ = reply.send(signature);
            }
            NetworkActorCommand::SendPayment(payment_request, reply) => {
                match self.on_send_payment(myself, state, payment_request).await {
                    Ok(payment) => {
                        let _ = reply.send(Ok(payment));
                    }
                    Err(e) => {
                        error!("Failed to send payment: {:?}", e);
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            NetworkActorCommand::SendPaymentWithRouter(payment_request, reply) => {
                match self
                    .on_send_payment_with_router(myself, state, payment_request)
                    .await
                {
                    Ok(payment) => {
                        let _ = reply.send(Ok(payment));
                    }
                    Err(e) => {
                        error!("Failed to send payment: {:?}", e);
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
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
            NetworkActorCommand::ListPeers(_, rpc) => {
                let peers = state
                    .peer_session_map
                    .keys()
                    .map(|peer_id| PeerInfo {
                        peer_id: peer_id.clone(),
                        pubkey: state
                            .state_to_be_persisted
                            .get_peer_pubkey(peer_id)
                            .expect("pubkey not found"),
                        addresses: state.state_to_be_persisted.get_peer_addresses(peer_id),
                    })
                    .collect::<Vec<_>>();
                let _ = rpc.send(Ok(peers));
            }
        };
        Ok(())
    }

    async fn handle_send_onion_packet_command(
        &self,
        state: &mut NetworkActorState<S>,
        command: SendOnionPacketCommand,
    ) -> Result<(), TlcErr> {
        let SendOnionPacketCommand {
            peeled_onion_packet,
            previous_tlc,
            payment_hash,
            attempt_id,
        } = command;

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
                previous_tlc,
            },
            rpc_reply,
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

    fn get_tlc_error(
        &self,
        state: &mut NetworkActorState<S>,
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

    fn get_payment_session_with_attempt(
        &self,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
    ) -> (Option<PaymentSession>, Option<Attempt>) {
        let payment_session = self.store.get_payment_session(payment_hash);
        let attempt =
            attempt_id.and_then(|attempt_id| self.store.get_attempt(payment_hash, attempt_id));

        (payment_session, attempt)
    }

    async fn on_remove_tlc_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
        reason: RemoveTlcReason,
    ) {
        let (Some(mut session), Some(mut attempt)) =
            self.get_payment_session_with_attempt(payment_hash, attempt_id)
        else {
            error!(
                "Payment session or attempt not found for payment hash: {:?}, attempt id: {:?}",
                payment_hash, attempt_id
            );
            return;
        };

        match reason {
            RemoveTlcReason::RemoveTlcFulfill(_) => {
                dbg!("record attempt fulfilled", &reason);
                self.network_graph
                    .write()
                    .await
                    .record_attempt_success(&attempt);
                attempt.set_success_status();
                self.store.insert_attempt(attempt.clone());

                // the payment session status maybe changed into Success
                session.update_with_attempt(attempt);
                if !session.is_dry_run() {
                    self.store.insert_payment_session(session);
                }
            }
            RemoveTlcReason::RemoveTlcFail(reason) => {
                let error_detail = reason
                    .decode(&attempt.session_key, attempt.hops_public_keys())
                    .unwrap_or_else(|| {
                        debug_event!(myself, "InvalidOnionError");
                        TlcErr::new(TlcErrorCode::InvalidOnionError)
                    });
                debug!("on_remove_tlc: {:?}", error_detail.error_code);
                let need_to_retry = self.network_graph.write().await.record_attempt_fail(
                    &attempt,
                    error_detail.clone(),
                    false,
                );
                dbg!("set attempt failed to ", error_detail.error_code.as_ref());

                self.set_attempt_fail_with_error(
                    &mut session,
                    &mut attempt,
                    error_detail.error_code.as_ref(),
                    need_to_retry,
                );

                if need_to_retry {
                    self.register_payment_retry(myself, payment_hash, Some(attempt.id));
                }
            }
        }
    }

    async fn update_graph_with_tlc_fail(
        &self,
        network: &ActorRef<NetworkActorMessage>,
        tcl_error_detail: &TlcErr,
    ) {
        let error_code = tcl_error_detail.error_code();
        // https://github.com/lightning/bolts/blob/master/04-onion-routing.md#rationale-6
        // we now still update the graph, maybe we need to remove it later?
        if error_code.is_update() {
            if let Some(TlcErrData::ChannelFailed {
                channel_update: Some(channel_update),
                ..
            }) = &tcl_error_detail.extra_data
            {
                network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::BroadcastMessages(vec![
                            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update.clone()),
                        ]),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }
        match tcl_error_detail.error_code() {
            TlcErrorCode::PermanentChannelFailure
            | TlcErrorCode::ChannelDisabled
            | TlcErrorCode::UnknownNextPeer => {
                let channel_outpoint = tcl_error_detail
                    .error_channel_outpoint()
                    .expect("expect channel outpoint");
                let mut graph = self.network_graph.write().await;
                debug!("debug mark channel failed: {:?}", channel_outpoint);
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
            Some(session_state) => Ok(session_state.into()),
            None => Err(Error::InvalidParameter(format!(
                "Payment session not found: {:?}",
                payment_hash
            ))),
        }
    }

    async fn resend_payment_attempt(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
    ) -> Result<(), Error> {
        assert!(attempt.is_retrying());
        let graph = self.network_graph.read().await;
        // `session.remain_amount()` do not contains this part of amount,
        // so we need to add the receiver amount to it, so we may make fewer
        // attempts to send the payment.
        let amount = session.remain_amount() + attempt.route.receiver_amount();
        let max_fee = session.remain_fee_amount();

        session.request.channel_stats = GraphChannelStat::new(Some(graph.channel_stats()));
        match graph.build_route(amount, None, max_fee, &session.request) {
            Err(e) => {
                let error = format!("Failed to build route, {}", e);
                return Err(Error::BuildPaymentRouteError(error));
            }
            Ok(hops) => {
                let source = graph.get_source_pubkey();
                attempt.route = SessionRoute::new(source, session.request.target_pubkey, &hops);
                assert_ne!(hops[0].funding_tx_hash, Hash256::default());
                self.send_attempt(myself, state, session, attempt, hops)
                    .await?;
            }
        };
        Ok(())
    }

    async fn build_payment_routes(
        &self,
        session: &mut PaymentSession,
    ) -> Result<Vec<(Attempt, Vec<PaymentHopData>)>, Error> {
        let graph = self.network_graph.read().await;
        let source = graph.get_source_pubkey();
        let active_parts = session.attempts().filter(|a| a.is_active()).count();
        let mut remain_amount = session.remain_amount();
        let mut max_fee = session.remain_fee_amount();
        let mut result = vec![];
        let minimal_amount = session.request.allow_minimal_amount();

        if remain_amount < minimal_amount {
            let error = format!(
                "Send amount {} is less than minimal amount {}",
                remain_amount, minimal_amount
            );
            self.set_payment_fail_with_error(session, &error);
            return Err(Error::SendPaymentError(error));
        }

        session.request.channel_stats = GraphChannelStat::new(Some(graph.channel_stats()));
        let mut attempt_id = session.attempts_count() as u64;
        let mut target_amount = remain_amount;
        let mut amount_low_bound = Some(minimal_amount);
        let mut iteration = 0;

        while (result.len() < session.max_parts() - active_parts) && remain_amount > 0 {
            iteration += 1;
            dbg!(
                "build route iteration {}",
                iteration,
                target_amount,
                amount_low_bound,
            );
            match graph.build_route(target_amount, amount_low_bound, max_fee, &session.request) {
                Err(e) => {
                    let error = format!("Failed to build route, {}", e);
                    self.set_payment_fail_with_error(session, &error);
                    return Err(Error::SendPaymentError(error));
                }
                Ok(hops) => {
                    assert_ne!(hops[0].funding_tx_hash, Hash256::default());
                    let route = SessionRoute::new(source, session.request.target_pubkey, &hops);

                    let left_amount = remain_amount - route.receiver_amount();
                    dbg!(
                        "left amount: {}, target amount: {}",
                        left_amount,
                        target_amount,
                        route.receiver_amount()
                    );
                    if left_amount < minimal_amount && left_amount > 0 {
                        if remain_amount >= 2 * minimal_amount {
                            target_amount -= minimal_amount;
                            amount_low_bound = Some(minimal_amount);
                        } else {
                            target_amount = remain_amount;
                            amount_low_bound = None;
                        }
                        continue;
                    }

                    for (from, channel_outpoint, amount) in route.channel_outpoints() {
                        if let Some(sent_node) = graph.get_channel_sent_node(channel_outpoint, from)
                        {
                            session.request.channel_stats.add_channel(
                                channel_outpoint,
                                sent_node,
                                amount,
                            );
                        }
                    }
                    remain_amount -= route.receiver_amount();
                    target_amount = remain_amount;
                    if let Some(fee) = max_fee {
                        max_fee = Some(fee - route.fee());
                    }

                    let new_attempt_id = if session.is_dry_run() {
                        0
                    } else {
                        attempt_id += 1;
                        attempt_id
                    };

                    let attempt = session.new_attempt(new_attempt_id, route);
                    result.push((attempt, hops));
                }
            };
        }

        for (attempt, _) in &result {
            session.append_attempt(attempt.clone());
        }

        return Ok(result);
    }

    async fn send_payment_onion_packet(
        &self,
        state: &mut NetworkActorState<S>,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
        hops: Vec<PaymentHopData>,
    ) -> Result<(), Error> {
        let session_key = Privkey::from_slice(KeyPair::generate_random_key().as_ref());
        assert_ne!(hops[0].funding_tx_hash, Hash256::default());

        attempt.session_key.copy_from_slice(session_key.as_ref());

        let peeled_onion_packet = match PeeledPaymentOnionPacket::create(
            session_key,
            hops,
            Some(attempt.hash.as_ref().to_vec()),
            &Secp256k1::signing_only(),
        ) {
            Ok(packet) => packet,
            Err(e) => {
                let err = format!(
                    "Failed to create onion packet: {:?}, error: {:?}",
                    attempt.hash, e
                );
                dbg!("set attempt failed to ", &err);
                self.set_attempt_fail_with_error(session, attempt, &err, false);
                return Err(Error::SendPaymentFirstHopError(err, false));
            }
        };

        match self
            .handle_send_onion_packet_command(
                state,
                SendOnionPacketCommand {
                    peeled_onion_packet,
                    previous_tlc: None,
                    payment_hash: attempt.payment_hash,
                    attempt_id: Some(attempt.id),
                },
            )
            .await
        {
            Err(error_detail) => {
                self.update_graph_with_tlc_fail(&state.network, &error_detail)
                    .await;
                let need_to_retry = self.network_graph.write().await.record_attempt_fail(
                    attempt,
                    error_detail.clone(),
                    true,
                );
                let err = format!(
                    "Failed to send onion packet with error {}",
                    error_detail.error_code_as_str()
                );
                self.set_attempt_fail_with_error(session, attempt, &err, need_to_retry);
                return Err(Error::SendPaymentFirstHopError(err, need_to_retry));
            }
            Ok(_) => {
                self.store.insert_attempt(attempt.clone());
                return Ok(());
            }
        }
    }

    async fn on_add_tlc_result_event(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
        error_info: Option<(ProcessingChannelError, TlcErr)>,
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
                        command: ChannelCommand::ForwardTlcResult(ForwardTlcResult {
                            payment_hash,
                            channel_id,
                            tlc_id,
                            error_info: error_info.clone(),
                        }),
                    }),
                ))
                .expect("network actor alive");
            return;
        }

        let (Some(mut session), Some(mut attempt)) =
            self.get_payment_session_with_attempt(payment_hash, attempt_id)
        else {
            return;
        };

        match error_info {
            None => {
                // attempt is inflight
                attempt.set_inflight_status();
                self.network_graph
                    .write()
                    .await
                    .track_attempt_router(&attempt);
                self.store.insert_attempt(attempt);
            }
            Some((ProcessingChannelError::RepeatedProcessing(_), _)) => {
                // do nothing
            }
            Some((error, tlc_err)) => {
                self.update_graph_with_tlc_fail(&myself, &tlc_err).await;
                let (error, need_to_retry) =
                    if matches!(error, ProcessingChannelError::WaitingTlcAck) {
                        ("WaitingTlcAck".to_string(), true)
                    } else {
                        let need_to_retry = self.network_graph.write().await.record_attempt_fail(
                            &attempt,
                            tlc_err.clone(),
                            true,
                        );
                        (error.to_string(), need_to_retry)
                    };

                self.set_attempt_fail_with_error(&mut session, &mut attempt, &error, need_to_retry);
                // retry the current attempt if it is retryable
                self.register_payment_retry(myself, payment_hash, Some(attempt.id));
            }
        }
    }

    fn set_payment_fail_with_error(&self, session: &mut PaymentSession, error: &str) {
        session.set_failed_status(error);
        if !session.is_dry_run() {
            self.store.insert_payment_session(session.clone());
        }
    }

    fn set_attempt_fail_with_error(
        &self,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
        error: &str,
        retryable: bool,
    ) {
        if !retryable && !session.allow_more_attempts() {
            // if mpp is not allowed, or mpp is allowed but attempt is not retryable
            // we will set the session status to failed
            self.set_payment_fail_with_error(session, error);
        }

        attempt.set_failed_status(error, retryable);
        if !session.is_dry_run() {
            self.store.insert_attempt(attempt.clone());
        }
    }

    async fn send_attempt(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        session: &mut PaymentSession,
        attempt: &mut Attempt,
        route: Vec<PaymentHopData>,
    ) -> Result<(), Error> {
        if let Err(err) = self
            .send_payment_onion_packet(state, session, attempt, route)
            .await
        {
            let need_retry = matches!(err, Error::SendPaymentFirstHopError(_, true));
            if need_retry {
                // If this is the first hop error, such as the WaitingTlcAck error,
                // we will just retry later, return Ok here for letting endpoint user
                // know payment session is created successfully
                self.register_payment_retry(myself, session.payment_hash(), Some(attempt.id));
                return Ok(());
            } else {
                self.set_attempt_fail_with_error(session, attempt, &err.to_string(), false);
                return Err(err);
            }
        }
        Ok(())
    }

    /// Resume the payment session
    async fn resume_payment_session(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
    ) -> Result<(), Error> {
        self.update_graph().await;
        let Some(mut session) = self.store.get_payment_session(payment_hash) else {
            return Err(Error::InvalidParameter(payment_hash.to_string()));
        };

        if session.status.is_final() {
            return Ok(());
        }

        self.retry_payment_attempt(myself.clone(), state, &mut session, attempt_id)
            .await?;

        if !self.payment_need_more_retry(&mut session)? {
            return Ok(());
        }

        // here we begin to create attempts and routes for the payment session,
        // it depends on the path finding algorithm to create how many of attempts,
        // if a payment can not be met in the network graph, an build path error will be returned
        // and no attempts be stored in the payment session and db.
        let attempts_with_routes =
            self.build_payment_routes(&mut session)
                .await
                .inspect_err(|e| {
                    self.set_payment_fail_with_error(&mut session, &e.to_string());
                })?;

        for (mut attempt, route) in attempts_with_routes {
            self.send_attempt(myself.clone(), state, &mut session, &mut attempt, route)
                .await?;
        }

        if let Ok(true) = self.payment_need_more_retry(&mut session) {
            self.register_payment_retry(myself, payment_hash, None);
        }

        Ok(())
    }

    async fn retry_payment_attempt(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        session: &mut PaymentSession,
        attempt_id: Option<u64>,
    ) -> Result<(), Error> {
        let Some(attempt_id) = attempt_id else {
            return Ok(());
        };

        match self.store.get_attempt(session.payment_hash(), attempt_id) {
            Some(mut attempt) if attempt.is_retrying() => {
                match self
                    .resend_payment_attempt(myself, state, session, &mut attempt)
                    .await
                {
                    Err(err) if session.allow_mpp() => {
                        // usually `resend_payment_route` will only try build a route with same amount,
                        // because most of the time, resend payment caused by the first hop
                        // error with WaitingTlcAck, if resend failed we should try more attempts in MPP,
                        // so we may create more attempts with different split amounts
                        attempt.set_failed_status(&err.to_string(), false);
                        self.store.insert_attempt(attempt);
                    }
                    Err(err) => {
                        self.set_attempt_fail_with_error(
                            session,
                            &mut attempt,
                            &err.to_string(),
                            false,
                        );
                        return Err(err);
                    }
                    _ => {}
                }
            }
            Some(_) => {
                // no retry for non-retryable attempts
            }
            None => {
                return Err(Error::InvalidParameter(format!(
                    "Attempt with id {:?} not found for payment hash: {:?}",
                    attempt_id,
                    session.payment_hash()
                )));
            }
        }

        Ok(())
    }

    pub fn payment_need_more_retry(&self, session: &mut PaymentSession) -> Result<bool, Error> {
        session.flush_attempts(&self.store);
        let more_attempt = session.allow_more_attempts();
        if !more_attempt && session.remain_amount() > 0 {
            let err = "Can not send payment with limited attempts";
            self.set_payment_fail_with_error(session, err);
            return Err(Error::SendPaymentError(err.to_string()));
        }
        Ok(more_attempt)
    }

    fn register_payment_retry(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        payment_hash: Hash256,
        attempt_id: Option<u64>,
    ) {
        myself.send_after(Duration::from_millis(500), move || {
            NetworkActorMessage::new_event(NetworkActorEvent::RetrySendPayment(
                payment_hash,
                attempt_id,
            ))
        });
    }

    async fn on_send_payment(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        payment_request: SendPaymentCommand,
    ) -> Result<SendPaymentResponse, Error> {
        let payment_data = SendPaymentData::new(payment_request.clone()).map_err(|e| {
            error!("Failed to validate payment request: {:?}", e);
            Error::InvalidParameter(format!("Failed to validate payment request: {:?}", e))
        })?;

        self.send_payment_with_payment_data(myself, state, payment_data)
            .await
    }

    async fn on_send_payment_with_router(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        command: SendPaymentWithRouterCommand,
    ) -> Result<SendPaymentResponse, Error> {
        // Only proceed if we have at least one hop requirement
        let Some(last_edge) = command.router.last() else {
            return Err(Error::InvalidParameter(
                "No hop requirements provided".to_string(),
            ));
        };

        let source = self.network_graph.read().await.get_source_pubkey();
        let target = last_edge.target;
        let amount = last_edge.amount_received;

        // Create payment command with defaults from the last hop
        let payment_command = SendPaymentCommand {
            target_pubkey: Some(target),
            payment_hash: command.payment_hash,
            invoice: command.invoice,
            allow_self_payment: target == source,
            dry_run: command.dry_run,
            amount: Some(amount),
            keysend: command.keysend,
            udt_type_script: command.udt_type_script.clone(),
            ..Default::default()
        };

        let mut payment_data = SendPaymentData::new(payment_command).map_err(|e| {
            error!("Failed to validate payment request: {:?}", e);
            Error::InvalidParameter(format!("Failed to validate payment request: {:?}", e))
        })?;

        // specify the router to be used
        payment_data.router = command.router.clone();
        self.send_payment_with_payment_data(myself, state, payment_data)
            .await
    }

    async fn send_payment_with_payment_data(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState<S>,
        payment_data: SendPaymentData,
    ) -> Result<SendPaymentResponse, Error> {
        // for dry run, we only build the route and return the hops info,
        // will not store the payment session and send the onion packet
        if payment_data.dry_run {
            let mut payment_session = PaymentSession::new(&self.store, payment_data, 0);
            self.build_payment_routes(&mut payment_session).await?;
            return Ok(payment_session.into());
        }

        // initialize the payment session in db and begin the payment process lifecycle
        if let Some(payment_session) = self.store.get_payment_session(payment_data.payment_hash) {
            // we only allow retrying payment session with status failed
            if payment_session.status != PaymentStatus::Failed {
                return Err(Error::InvalidParameter(format!(
                    "Payment session already exists: {} with payment session status: {:?}",
                    payment_data.payment_hash, payment_session.status
                )));
            } else {
                // even if the payment session is failed, we still need to check whether
                // some attempts are still flight state, this means some middle hops
                // haven't send back the result of the onion packet, so we can not retry the payment session
                // otherwise, we are sure it's safe to cleanup all the previous attempts
                if payment_session.attempts().any(|a| a.is_inflight()) {
                    return Err(Error::InvalidParameter(format!(
                        "Payment session {} has attempts that are in flight state, can not retry",
                        payment_data.payment_hash
                    )));
                }
                self.store.delete_attempts(payment_data.payment_hash);
            }
        }

        let try_limit = if payment_data.allow_mpp() {
            payment_data.max_parts() as u32 * DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT
        } else {
            DEFAULT_PAYMENT_TRY_LIMIT
        };
        let mut payment_session = PaymentSession::new(&self.store, payment_data, try_limit);
        assert!(payment_session.attempts_count() == 0);
        self.store.insert_payment_session(payment_session.clone());

        self.resume_payment_session(myself, state, payment_session.payment_hash(), None)
            .await?;
        payment_session.flush_attempts(&self.store);
        return Ok(payment_session.into());
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
    peer_session_map: HashMap<PeerId, Peer>,
    session_channels_map: HashMap<SessionId, HashSet<Hash256>>,
    channels: HashMap<Hash256, ActorRef<ChannelActorMessage>>,
    ckb_txs_in_flight: HashMap<Hash256, ActorRef<InFlightCkbTxActorMessage>>,
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
    // The default amount of CKB to be funded when auto accepting a channel.
    auto_accept_channel_ckb_funding_amount: u64,
    // The default expiry delta to forward tlcs.
    tlc_expiry_delta: u64,
    // The default tlc min and max value of tlcs to be accepted.
    tlc_min_value: u128,
    // The default tlc fee proportional millionths to be used when auto accepting a channel.
    tlc_fee_proportional_millionths: u128,
    // The gossip messages actor to process and send gossip messages.
    gossip_actor: ActorRef<GossipActorMessage>,
    channel_subscribers: ChannelSubscribers,
    max_inbound_peers: usize,
    min_outbound_peers: usize,

    // The features of the node, used to indicate the capabilities of the node.
    features: FeatureVector,
}

pub(crate) struct Peer {
    pub session_id: SessionId,
    pub session_type: SessionType,
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

fn generate_in_flight_tx_actor_name(supervisor: ActorCell, tx_hash: Hash256) -> String {
    let supervisor_name = supervisor.get_name();
    format!(
        "{}/InFlightCkbTx-{}",
        supervisor_name.as_deref().unwrap_or_default(),
        tx_hash
    )
}

impl<S> NetworkActorState<S>
where
    S: NetworkActorStateStore
        + ChannelActorStateStore
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
        + InvoiceStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn get_or_create_new_node_announcement_message(&mut self) -> NodeAnnouncement {
        let now = now_timestamp_as_millis_u64();
        match self.last_node_announcement_message {
            // If the last node announcement message is still relatively new, we don't need to create a new one.
            // Because otherwise the receiving node may be confused by the multiple announcements,
            // and falsely believe we updated the node announcement, and then forward this message to other nodes.
            // This is undesirable because we don't want to flood the network with the same message.
            // On the other hand, if the message is too old, we need to create a new one.
            Some(ref message) if now - message.timestamp < 3600 * 1000 => {
                debug!("Returning old node announcement message as it is still valid");
            }
            _ => {
                let node_name = self.node_name.unwrap_or_default();
                let addresses = self.announced_addrs.clone();
                let announcement = NodeAnnouncement::new(
                    node_name,
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
            tlc_fee_proportional_millionths,
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
        } = open_channel;

        if let Some(Peer {
            features: Some(peer_features),
            ..
        }) = self.peer_session_map.get(&peer_id)
        {
            // check peer features
            if !self.features.compatible_with(peer_features) {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Peer {:?} features {:?} are not compatible with our features {:?}",
                    &peer_id, peer_features, self.features
                )));
            }
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Peer {:?}'s feature not found, waiting for peer to send Init message",
                &peer_id
            )));
        }

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
                tlc_info: ChannelTlcInfo::new(
                    tlc_min_value.unwrap_or(self.tlc_min_value),
                    tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta),
                    tlc_fee_proportional_millionths.unwrap_or(self.tlc_fee_proportional_millionths),
                ),
                public_channel_info: public.then_some(PublicChannelInfo::new()),
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
            network.clone().get_cell(),
        )
        .await
        .map_err(|e| ProcessingChannelError::SpawnErr(e.to_string()))?
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
                tlc_info: ChannelTlcInfo::new(
                    min_tlc_value.unwrap_or(self.tlc_min_value),
                    tlc_expiry_delta.unwrap_or(self.tlc_expiry_delta),
                    tlc_fee_proportional_millionths.unwrap_or(self.tlc_fee_proportional_millionths),
                ),
                public_channel_info: open_channel.is_public().then_some(PublicChannelInfo::new()),
                seed,
                open_channel,
                shutdown_script,
                channel_id_sender: Some(tx),
                max_tlc_number_in_flight: max_tlc_number_in_flight
                    .unwrap_or(MAX_TLC_NUMBER_IN_FLIGHT),
                max_tlc_value_in_flight: max_tlc_value_in_flight.unwrap_or(u128::MAX),
            }),
            network.clone().get_cell(),
        )
        .await
        .map_err(|e| ProcessingChannelError::SpawnErr(e.to_string()))?
        .0;
        let new_id = rx.await.expect("msg received");
        self.on_channel_created(new_id, &peer_id, channel.clone());
        Ok((channel, temp_channel_id, new_id))
    }

    pub async fn trace_tx(
        &mut self,
        tx_hash: Hash256,
        tx_kind: InFlightCkbTxKind,
    ) -> crate::Result<()> {
        if let Entry::Vacant(entry) = self.ckb_txs_in_flight.entry(tx_hash) {
            let handler = InFlightCkbTxActor {
                chain_actor: self.chain_actor.clone(),
                network_actor: self.network.clone(),
                tx_hash,
                tx_kind,
                confirmations: CKB_TX_TRACING_CONFIRMATIONS,
            };

            let (task, _) = Actor::spawn_linked(
                Some(generate_in_flight_tx_actor_name(
                    self.network.get_cell(),
                    tx_hash,
                )),
                handler,
                InFlightCkbTxActorArguments { transaction: None },
                self.network.get_cell(),
            )
            .await?;

            entry.insert(task);
        }
        Ok(())
    }

    pub async fn send_tx(
        &mut self,
        tx: TransactionView,
        tx_kind: InFlightCkbTxKind,
    ) -> crate::Result<()> {
        let tx_hash: Hash256 = tx.hash().into();
        match self.ckb_txs_in_flight.entry(tx_hash) {
            Entry::Vacant(vacant) => {
                let handler = InFlightCkbTxActor {
                    chain_actor: self.chain_actor.clone(),
                    network_actor: self.network.clone(),
                    tx_hash,
                    tx_kind,
                    confirmations: CKB_TX_TRACING_CONFIRMATIONS,
                };

                let (task, _) = Actor::spawn_linked(
                    Some(generate_in_flight_tx_actor_name(
                        self.network.get_cell(),
                        tx_hash,
                    )),
                    handler,
                    InFlightCkbTxActorArguments {
                        transaction: Some(tx),
                    },
                    self.network.get_cell(),
                )
                .await?;

                vacant.insert(task);
            }
            Entry::Occupied(occupied) => {
                occupied
                    .get()
                    .send_message(InFlightCkbTxActorMessage::SendTx(tx))?;
            }
        }
        Ok(())
    }

    pub fn remove_in_flight_tx(&mut self, tx_hash: Hash256) {
        if let Some(task) = self.ckb_txs_in_flight.remove(&tx_hash) {
            task.stop(Some("cleanup in flight tx".to_string()));
        }
    }

    pub async fn abort_funding(&mut self, channel_id_or_outpoint: Either<Hash256, OutPoint>) {
        let channel_id = match channel_id_or_outpoint {
            Either::Left(channel_id) => channel_id,
            Either::Right(outpoint) => {
                self.remove_in_flight_tx(outpoint.tx_hash().into());
                match self.pending_channels.remove(&outpoint) {
                    Some(channel_id) => channel_id,
                    None => {
                        warn!(
                            "Funding transaction failed for outpoint {:?} but no channel found",
                            &outpoint
                        );
                        return;
                    }
                }
            }
        };

        self.send_message_to_channel_actor(
            channel_id,
            None,
            ChannelActorMessage::Event(ChannelEvent::Stop(StopReason::AbortFunding)),
        )
        .await;
    }

    pub async fn abandon_channel(
        &mut self,
        channel_id: Hash256,
    ) -> Result<(), ProcessingChannelError> {
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
                "Commitment fee {} which calculated by commitment fee rate {} is larger than half of reserved fee {}",
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
            // Need to handle the force shutdown command specially because the ChannelActor may not exist when remote peer is disconnected.
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

                            let transaction = match state.get_latest_commitment_transaction() {
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
        self.peer_session_map.insert(
            remote_peer_id.clone(),
            Peer {
                session_id: session.id,
                session_type: session.ty,
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
    }

    fn on_peer_disconnected(&mut self, id: &PeerId) {
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
    ) {
        self.send_message_to_channel_actor(
            *channel_id,
            None,
            ChannelActorMessage::Event(ChannelEvent::ClosingTransactionConfirmed(force)),
        )
        .await;
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

        self.remove_in_flight_tx(tx_hash.into());
    }

    async fn on_channel_actor_stopped(&mut self, channel_id: Hash256, reason: StopReason) {
        // all check passed, now begin to remove from memory and DB
        self.channels.remove(&channel_id);
        for (_peer_id, Peer { session_id, .. }) in self.peer_session_map.iter() {
            if let Some(session_channels) = self.session_channels_map.get_mut(session_id) {
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
    ) -> Result<(), ProcessingChannelError> {
        if !self.is_connected(&peer_id) {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Peer {:?} is not connected",
                &peer_id
            )));
        }

        check_chain_hash(&init_msg.chain_hash).map_err(|e| {
            self.network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::DisconnectPeer(peer_id.clone()),
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
        self.remove_in_flight_tx(outpoint.tx_hash().into());
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
        + GossipMessageStore
        + PreimageStore
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
        let my_peer_id: PeerId = PeerId::from(secio_pk);
        let handle = NetworkServiceHandle::new(myself.clone());
        let fiber_handle = FiberProtocolHandle::from(&handle);
        let mut gossip_config = GossipConfig::from(&config);
        gossip_config.peer_id = Some(my_peer_id.clone());
        let (gossip_service, gossip_handle) = GossipService::start(
            gossip_config,
            self.store.clone(),
            self.chain_actor.clone(),
            myself.get_cell(),
        )
        .await;
        let mut graph = self.network_graph.write().await;
        let graph_subscribing_cursor = graph
            .get_latest_cursor()
            .go_back_for_some_time(MAX_GRAPH_MISSING_BROADCAST_MESSAGE_TIMESTAMP_DRIFT);

        gossip_service
            .get_subscriber()
            .subscribe(graph_subscribing_cursor, myself.clone(), |m| {
                Some(NetworkActorMessage::new_event(
                    NetworkActorEvent::GossipMessageUpdates(m),
                ))
            })
            .await
            .expect("subscribe to gossip store updates");
        let gossip_actor = gossip_handle.actor().clone();
        let mut service = ServiceBuilder::default()
            .insert_protocol(fiber_handle.create_meta())
            .insert_protocol(gossip_handle.create_meta())
            .handshake_type(secio_kp.into())
            .build(handle);
        let mut listening_addr = service
            .listen(
                MultiAddr::from_str(config.listening_addr())
                    .expect("valid tentacle listening address"),
            )
            .await
            .expect("listen tentacle");

        listening_addr.push(Protocol::P2P(Cow::Owned(my_peer_id.clone().into_bytes())));
        let mut announced_addrs = Vec::with_capacity(config.announced_addrs.len() + 1);
        if config.announce_listening_addr() {
            announced_addrs.push(listening_addr.clone());
        }
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
            ckb_txs_in_flight: Default::default(),
            outpoint_channel_map: Default::default(),
            to_be_accepted_channels: Default::default(),
            pending_channels: Default::default(),
            chain_actor,
            open_channel_auto_accept_min_ckb_funding_amount: config
                .open_channel_auto_accept_min_ckb_funding_amount(),
            auto_accept_channel_ckb_funding_amount: config.auto_accept_channel_ckb_funding_amount(),
            tlc_expiry_delta: config.tlc_expiry_delta(),
            tlc_min_value: config.tlc_min_value(),
            tlc_fee_proportional_millionths: config.tlc_fee_proportional_millionths(),
            gossip_actor,
            channel_subscribers,
            max_inbound_peers: config.max_inbound_peers(),
            min_outbound_peers: config.min_outbound_peers(),
            features,
        };

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
                "Reconnecting channel {:x} peers {:?} in state {:?} with addresses {:?}",
                &channel_id, &peer_id, &channel_state, &addresses
            );
            for addr in addresses {
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::ConnectPeer(addr),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
        }
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

        // Trigger mmp tlc set fulfill check and hold tlc timeout
        let now = now_timestamp_as_millis_u64();
        for (payment_hash, hold_tlcs) in self.store.list_all_hold_tlcs() {
            // timeout hold tlc
            let already_timeout = hold_tlcs
                .iter()
                .map(|hold_tlc| hold_tlc.hold_expire_at)
                .min()
                .is_some_and(|expire_at| expire_at <= now);

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

            // try settle mpp tlc set
            if !already_timeout {
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SettleMPPTlcSet(payment_hash),
                    ))
                    .expect(ASSUME_NETWORK_MYSELF_ALIVE);
            }
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
        trace!("Service error: {:?}", error);
        // TODO
        // ServiceError::DialerError => remove address from peer store
        // ServiceError::ProtocolError => ban peer
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        trace!("Service event: {:?}", event);
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
        + NetworkGraphStateStore
        + GossipMessageStore
        + PreimageStore
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
