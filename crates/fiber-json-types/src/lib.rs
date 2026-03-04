//! RPC JSON request/response types for the Fiber Network.
//!
//! This crate contains the pure data types (request/response structs and enums)
//! used by the Fiber Network's JSON-RPC API. These types are separated from the
//! RPC server implementations to allow external tools and clients to depend on
//! the type definitions without pulling in the full node implementation.
//!
//! The types are organized by RPC module:
//! - `channel`: Channel management types
//! - `payment`: Payment types
//! - `graph`: Network graph types
//! - `invoice`: Invoice types
//! - `info`: Node info types
//! - `peer`: Peer management types
//! - `cch`: Cross-chain hub types (feature-gated)
//! - `dev`: Development/debug types
//! - `watchtower`: Watchtower types
//! - `prof`: Profiling types

// Re-export commonly used types from fiber-types for convenience
pub use fiber_types;

use ckb_jsonrpc_types::OutPoint as OutPointWrapper;
use ckb_jsonrpc_types::{DepType, EpochNumberWithFraction, JsonBytes, Script, ScriptHashType};
use ckb_types::packed::OutPoint;
use ckb_types::H256;
use fiber_types::{
    duration_hex, Attribute as InternalAttribute, AwaitingChannelReadyFlags,
    AwaitingTxSignaturesFlags, ChannelState as RawChannelState, ChannelUpdateInfo,
    CkbInvoice as InternalCkbInvoice, CkbInvoiceStatus, CkbScript, CloseFlags,
    CollaboratingFundingTxFlags, Currency, EntityHex, Hash256, HashAlgorithm,
    HopHint as NetworkHopHint, HopRequire, InvoiceData as InternalInvoiceData, InvoiceSignature,
    NegotiatingFundingFlags, NodeId, PaymentCustomRecords as InternalPaymentCustomRecords,
    PaymentStatus, Privkey, Pubkey, RevocationData, RouterHop, SettlementData, ShuttingDownFlags,
    SigningCommitmentFlags, TlcStatus, U128Hex, U32Hex, U64Hex,
};
use secp256k1::PublicKey;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;
use tentacle_multiaddr::MultiAddr;

#[cfg(debug_assertions)]
use fiber_types::SessionRoute;

#[cfg(feature = "cch")]
use fiber_types::{CchInvoice, CchOrder, CchOrderStatus};

/// Parameters for opening a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct OpenChannelParams {
    /// The public key of the peer to open a channel with.
    /// The peer must be connected through the [connect_peer](#peer-connect_peer) rpc first.
    pub pubkey: Pubkey,

    /// The amount of CKB or UDT to fund the channel with.
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,

    /// Whether this is a public channel (will be broadcasted to network, and can be used to forward TLCs),
    /// an optional parameter, default value is true.
    pub public: Option<bool>,

    /// Whether this is a one-way channel (will not be broadcasted to network, and can only be used to send payment one way),
    /// an optional parameter, default value is false.
    pub one_way: Option<bool>,

    /// The type script of the UDT to fund the channel with, an optional parameter.
    pub funding_udt_type_script: Option<Script>,

    /// The script used to receive the channel balance, an optional parameter,
    /// default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key.
    pub shutdown_script: Option<Script>,

    /// The delay time for the commitment transaction, must be an
    /// [EpochNumberWithFraction](https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0017-tx-valid-since/e-i-l-encoding.png)
    /// in u64 format, an optional parameter, default value is 1 epoch, which is 4 hours.
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,

    /// The fee rate for the commitment transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub commitment_fee_rate: Option<u64>,

    /// The fee rate for the funding transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub funding_fee_rate: Option<u64>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 4 hours, which is 4 * 60 * 60 * 1000 milliseconds
    /// Expect it >= 2/3 commitment_delay_epoch.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_delta: Option<u64>,

    /// The minimum value for a TLC our side can send,
    /// an optional parameter, default is 0, which means we can send any TLC is larger than 0.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_min_value: Option<u128>,

    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// The maximum value in flight for TLCs, an optional parameter.
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_tlc_value_in_flight: Option<u128>,

    /// The maximum number of TLCs that can be accepted, an optional parameter, default is 125
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U64Hex>")]
    pub max_tlc_number_in_flight: Option<u64>,
}

/// Result of opening a channel.
#[derive(Clone, Serialize, Deserialize)]
pub struct OpenChannelResult {
    /// The temporary channel ID of the channel being opened
    pub temporary_channel_id: Hash256,
}

/// Parameters for abandoning a channel.
#[derive(Serialize, Deserialize, Debug)]
pub struct AbandonChannelParams {
    /// The temporary channel ID or real channel ID of the channel being abandoned
    pub channel_id: Hash256,
}

/// Parameters for accepting a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct AcceptChannelParams {
    /// The temporary channel ID of the channel to accept
    pub temporary_channel_id: Hash256,

    /// The amount of CKB or UDT to fund the channel with
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,

    /// The script used to receive the channel balance, an optional parameter,
    /// default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key
    pub shutdown_script: Option<Script>,

    /// The max tlc sum value in flight for the channel, default is u128::MAX
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_tlc_value_in_flight: Option<u128>,

    /// The max tlc number in flight send from our side, default is 125
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U64Hex>")]
    pub max_tlc_number_in_flight: Option<u64>,

    /// The minimum value for a TLC our side can send,
    /// an optional parameter, default is 0, which means we can send any TLC is larger than 0.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_min_value: Option<u128>,

    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 1 day, which is 24 * 60 * 60 * 1000 milliseconds
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_delta: Option<u64>,
}

/// Result of accepting a channel.
#[derive(Clone, Serialize, Deserialize)]
pub struct AcceptChannelResult {
    /// The final ID of the channel that was accepted, it's different from the temporary channel ID
    pub channel_id: Hash256,
}

/// Parameters for listing channels.
#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ListChannelsParams {
    /// The public key to list channels for.
    /// An optional parameter, if not provided, all channels will be listed.
    pub pubkey: Option<Pubkey>,
    /// Whether to include closed channels in the list, an optional parameter, default value is false
    pub include_closed: Option<bool>,
    /// When set to true, only return channels that are still being opened (non-final states:
    /// negotiating, collaborating on funding tx, signing, awaiting tx signatures, awaiting channel
    /// ready) as well as channels whose opening attempt failed. Default is false.
    /// Mutually exclusive with `include_closed`.
    pub only_pending: Option<bool>,
}

/// Result of listing channels.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ListChannelsResult {
    /// The list of channels
    pub channels: Vec<Channel>,
}

/// The state of a channel.
// `ChannelState` is a copy of `ChannelState` with `#[serde(...)]` attributes for compatibility
// `bincode` does not support deserialize_identifier
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "SCREAMING_SNAKE_CASE",
    tag = "state_name",
    content = "state_flags"
)]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    NegotiatingFunding(NegotiatingFundingFlags),
    /// We're collaborating with the other party on the funding transaction.
    CollaboratingFundingTx(CollaboratingFundingTxFlags),
    /// We have collaborated over the funding and are now waiting for CommitmentSigned messages.
    SigningCommitment(SigningCommitmentFlags),
    /// We've received and sent `commitment_signed` and are now waiting for both
    /// party to collaborate on creating a valid funding transaction.
    AwaitingTxSignatures(AwaitingTxSignaturesFlags),
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady,
    /// We've successfully negotiated a `closing_signed` dance. At this point, the `ChannelManager`
    ShuttingDown(ShuttingDownFlags),
    /// This channel is closed.
    Closed(CloseFlags),
}

impl ChannelState {
    /// Returns true if this channel is in a "pending" state — i.e. the channel-opening
    /// flow has started but has not yet reached `ChannelReady` (and has not been closed
    /// cooperatively or uncooperatively). This includes `NegotiatingFunding`,
    /// `CollaboratingFundingTx`, `SigningCommitment`, `AwaitingTxSignatures`, and
    /// `AwaitingChannelReady`, as well as `Closed(ABANDONED)` / `Closed(FUNDING_ABORTED)`.
    pub fn is_pending(&self) -> bool {
        matches!(
            self,
            ChannelState::NegotiatingFunding(_)
                | ChannelState::CollaboratingFundingTx(_)
                | ChannelState::SigningCommitment(_)
                | ChannelState::AwaitingTxSignatures(_)
                | ChannelState::AwaitingChannelReady(_)
                | ChannelState::Closed(CloseFlags::ABANDONED)
                | ChannelState::Closed(CloseFlags::FUNDING_ABORTED)
        )
    }
}

impl From<RawChannelState> for ChannelState {
    fn from(state: RawChannelState) -> Self {
        match state {
            RawChannelState::NegotiatingFunding(flags) => ChannelState::NegotiatingFunding(flags),
            RawChannelState::CollaboratingFundingTx(flags) => {
                ChannelState::CollaboratingFundingTx(flags)
            }
            RawChannelState::SigningCommitment(flags) => ChannelState::SigningCommitment(flags),
            RawChannelState::AwaitingTxSignatures(flags) => {
                ChannelState::AwaitingTxSignatures(flags)
            }
            RawChannelState::AwaitingChannelReady(flags) => {
                ChannelState::AwaitingChannelReady(flags)
            }
            RawChannelState::ChannelReady => ChannelState::ChannelReady,
            RawChannelState::ShuttingDown(flags) => ChannelState::ShuttingDown(flags),
            RawChannelState::Closed(flags) => ChannelState::Closed(flags),
        }
    }
}

/// The channel data structure.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Channel {
    /// The channel ID
    pub channel_id: Hash256,
    /// Whether the channel is public
    pub is_public: bool,
    /// Is this channel initially inbound?
    /// An inbound channel is one where the counterparty is the funder of the channel.
    pub is_acceptor: bool,
    /// Is this channel one-way?
    /// Combines with is_acceptor to determine if the channel able to send payment to the counterparty or not.
    pub is_one_way: bool,
    #[serde_as(as = "Option<EntityHex>")]
    /// The outpoint of the channel
    pub channel_outpoint: Option<OutPoint>,
    /// The public key of the channel counterparty.
    pub pubkey: Pubkey,
    /// The UDT type script of the channel
    pub funding_udt_type_script: Option<Script>,
    /// The state of the channel
    pub state: ChannelState,
    /// The local balance of the channel
    #[serde_as(as = "U128Hex")]
    pub local_balance: u128,
    /// The offered balance of the channel
    #[serde_as(as = "U128Hex")]
    pub offered_tlc_balance: u128,
    /// The remote balance of the channel
    #[serde_as(as = "U128Hex")]
    pub remote_balance: u128,
    /// The received balance of the channel
    #[serde_as(as = "U128Hex")]
    pub received_tlc_balance: u128,
    /// The list of pending tlcs
    pub pending_tlcs: Vec<Htlc>,
    /// The hash of the latest commitment transaction
    pub latest_commitment_transaction_hash: Option<H256>,
    /// The time the channel was created at, in milliseconds from UNIX epoch
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Whether the channel is enabled
    pub enabled: bool,
    /// The expiry delta to forward a tlc, in milliseconds, default to 1 day, which is 24 * 60 * 60 * 1000 milliseconds
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "U128Hex")]
    pub tlc_fee_proportional_millionths: u128,
    /// The hash of the shutdown transaction
    pub shutdown_transaction_hash: Option<H256>,
    /// Human-readable reason why the channel opening failed.
    /// Only present when the channel is in a failed state (e.g. abandoned or funding aborted).
    pub failure_detail: Option<String>,
}

/// The htlc data structure.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Htlc {
    /// The id of the htlc
    #[serde_as(as = "U64Hex")]
    pub id: u64,
    /// The amount of the htlc
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The payment hash of the htlc
    pub payment_hash: Hash256,
    /// The expiry of the htlc
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// If this HTLC is involved in a forwarding operation, this field indicates the forwarding channel.
    /// For an outbound htlc, it is the inbound channel. For an inbound htlc, it is the outbound channel.
    pub forwarding_channel_id: Option<Hash256>,
    /// If this HTLC is involved in a forwarding operation, this field indicates the forwarding tlc id.
    #[serde_as(as = "Option<U64Hex>")]
    pub forwarding_tlc_id: Option<u64>,
    /// The status of the htlc
    pub status: TlcStatus,
}

/// Parameters for shutting down a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownChannelParams {
    /// The channel ID of the channel to shut down
    pub channel_id: Hash256,
    /// The script used to receive the channel balance, only support secp256k1_blake160_sighash_all script for now
    /// default is `default_funding_lock_script` in `CkbConfig`
    pub close_script: Option<Script>,
    /// The fee rate for the closing transaction, the fee will be deducted from the closing initiator's channel balance
    /// default is 1000 shannons/KW
    #[serde_as(as = "Option<U64Hex>")]
    pub fee_rate: Option<u64>,
    /// Whether to force the channel to close, when set to false, `close_script` and `fee_rate` should be set, default is false.
    /// When set to true, `close_script` and `fee_rate` will be ignored and will use the default value when opening the channel.
    pub force: Option<bool>,
}

/// Parameters for updating a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateChannelParams {
    /// The channel ID of the channel to update
    pub channel_id: Hash256,
    /// Whether the channel is enabled
    pub enabled: Option<bool>,
    /// The expiry delta for the TLC locktime
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_delta: Option<u64>,
    /// The minimum value for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_minimum_value: Option<u128>,
    /// The fee proportional millionths for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,
}

/// Parameters for getting a payment.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct GetPaymentCommandParams {
    /// The payment hash of the payment to retrieve
    pub payment_hash: Hash256,
}

/// The result of a get_payment command, which includes the payment hash, status, timestamps,
/// error message if failed, fee paid, and custom records.
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GetPaymentCommandResult {
    /// The payment hash of the payment
    pub payment_hash: Hash256,
    /// The status of the payment
    pub status: PaymentStatus,
    #[serde_as(as = "U64Hex")]
    /// The time the payment was created at, in milliseconds from UNIX epoch
    pub created_at: u64,
    #[serde_as(as = "U64Hex")]
    /// The time the payment was last updated at, in milliseconds from UNIX epoch
    pub last_updated_at: u64,
    /// The error message if the payment failed
    pub failed_error: Option<String>,
    /// fee paid for the payment
    #[serde_as(as = "U128Hex")]
    pub fee: u128,

    /// The custom records to be included in the payment.
    pub custom_records: Option<PaymentCustomRecords>,

    #[cfg(debug_assertions)]
    /// The router is a list of nodes that the payment will go through.
    /// We store in the payment session and then will use it to track the payment history.
    /// The router is a list of nodes that the payment will go through.
    /// If the payment adapted MPP (multi-part payment), the routers will be a list of nodes
    /// For example:
    ///    `A(amount, channel) -> B -> C -> D`
    /// means A will send `amount` with `channel` to B.
    pub routers: Vec<SessionRoute>,
}

/// Parameters for listing payments.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct ListPaymentsParams {
    /// Filter payments by status. If not set, all payments are returned.
    pub status: Option<PaymentStatus>,
    /// The maximum number of payments to return. Default is 15.
    #[serde_as(as = "Option<U64Hex>")]
    pub limit: Option<u64>,
    /// The payment hash to start returning payments after (exclusive cursor for pagination).
    pub after: Option<Hash256>,
}

/// Result of listing payments.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ListPaymentsResult {
    /// The list of payments.
    pub payments: Vec<GetPaymentCommandResult>,
    /// The last cursor for pagination. Use this as `after` in the next request to get more results.
    pub last_cursor: Option<Hash256>,
}

/// The custom records to be included in the payment.
/// The key is hex encoded of `u32`, it's range limited in 0 ~ 65535, and the value is hex encoded of `Vec<u8>` with `0x` as prefix.
/// For example:
/// ```json
/// "custom_records": {
///    "0x1": "0x01020304",
///    "0x2": "0x05060708",
///    "0x3": "0x090a0b0c",
///    "0x4": "0x0d0e0f10010d090a0b0c"
///  }
/// ```
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    #[serde(flatten)]
    #[serde_as(as = "HashMap<U32Hex, fiber_types::SliceHex>")]
    pub data: HashMap<u32, Vec<u8>>,
}

/// Parameters for sending a payment.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendPaymentCommandParams {
    /// The public key (`Pubkey`) of the payment target node, serialized as a hex string.
    /// You can obtain a node's pubkey via the `node_info` or `graph_nodes` RPC.
    pub target_pubkey: Option<Pubkey>,

    /// the amount of the payment, the unit is Shannons for non UDT payment
    /// If not set and there is a invoice, the amount will be set to the invoice amount
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,

    /// the hash to use within the payment's HTLC.
    /// If not set and `keysend` is set to true, a random hash will be generated.
    /// If not set and there is a `payment_hash` in the invoice, it will be used.
    /// Otherwise, `payment_hash` need to be set.
    pub payment_hash: Option<Hash256>,

    /// the TLC expiry delta should be used to set the timelock for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,

    /// the TLC expiry limit for the whole payment, in milliseconds, each hop is with a default tlc delta of 1 day
    /// suppose the payment router is with N hops, the total tlc expiry limit is at least (N-1) days
    /// this is also the default value for the payment if this parameter is not provided
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_limit: Option<u64>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// the payment timeout in seconds, if the payment is not completed within this time, it will be cancelled
    #[serde_as(as = "Option<U64Hex>")]
    pub timeout: Option<u64>,

    /// the maximum fee amounts in shannons that the sender is willing to pay.
    /// Note: In trampoline routing mode, the sender will use the max_fee_amount as the total fee as much as possible.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_fee_amount: Option<u128>,

    /// the maximum fee rate per thousand (‰), default is 5 (0.5%)
    #[serde_as(as = "Option<U64Hex>")]
    pub max_fee_rate: Option<u64>,

    /// max parts for the payment, only used for multi-part payments
    #[serde_as(as = "Option<U64Hex>")]
    pub max_parts: Option<u64>,

    /// Optional explicit trampoline hops.
    ///
    /// When set to a non-empty list `[t1, t2, ...]`, routing will only find a path from the
    /// payer to `t1`, and the inner trampoline onion will encode `t1 -> t2 -> ... -> final`.
    pub trampoline_hops: Option<Vec<Pubkey>>,

    /// keysend payment
    pub keysend: Option<bool>,

    /// udt type script for the payment
    pub udt_type_script: Option<Script>,

    /// Allow paying yourself through a circular route, default is false.
    /// This is useful for **channel rebalancing**: the payment flows out of one channel and
    /// back through another, shifting liquidity between your channels without changing your
    /// total balance (only routing fees are deducted).
    /// Set `target_pubkey` to your own node pubkey and `keysend` to `true` to perform a rebalance.
    /// Note: `allow_self_payment` is not compatible with trampoline routing.
    pub allow_self_payment: Option<bool>,

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

    /// Optional route hints to reach the destination through private channels.
    /// Note:
    ///    1. this is only used for the private channels with the last hop.
    ///    2. `hop_hints` is only a `hint` for routing algorithm,
    ///       it is not a guarantee that the payment will be routed through the specified channels,
    ///       it is up to the routing algorithm to decide whether to use the hints or not.
    ///
    /// For example `(pubkey, channel_outpoint, fee_rate, tlc_expiry_delta)` suggest path router
    /// to use the channel of `channel_outpoint` at hop with `pubkey` to forward the payment
    /// and the fee rate is `fee_rate` and tlc_expiry_delta is `tlc_expiry_delta`.
    pub hop_hints: Option<Vec<HopHint>>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
    pub dry_run: Option<bool>,
}

/// A hop hint is a hint for a node to use a specific channel.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopHint {
    /// The public key of the node
    pub pubkey: Pubkey,
    /// The outpoint of the channel
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,

    /// The fee rate to use this hop to forward the payment.
    #[serde_as(as = "U64Hex")]
    pub fee_rate: u64,
    /// The TLC expiry delta to use this hop to forward the payment.
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
}

impl From<HopHint> for NetworkHopHint {
    fn from(hop_hint: HopHint) -> Self {
        NetworkHopHint {
            pubkey: hop_hint.pubkey,
            channel_outpoint: hop_hint.channel_outpoint,
            fee_rate: hop_hint.fee_rate,
            tlc_expiry_delta: hop_hint.tlc_expiry_delta,
        }
    }
}

/// Parameters for building a payment router.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildRouterParams {
    /// the amount of the payment, the unit is Shannons for non UDT payment
    /// If not set, the minimum routable amount `1` is used
    #[serde_as(as = "Option<U128Hex>")]
    pub amount: Option<u128>,

    /// udt type script for the payment router
    pub udt_type_script: Option<Script>,

    /// A list of hops that defines the route. This does not include the source hop pubkey.
    /// A hop info is a tuple of pubkey and the channel(specified by channel funding tx) will be used.
    /// This is a strong restriction given on payment router, which means these specified hops and channels
    /// must be adapted in the router. This is different from hop hints, which maybe ignored by find path.
    /// If channel is not specified, find path algorithm will pick a channel within these two peers.
    ///
    /// An error will be returned if there is no router could be build from given hops and channels
    pub hops_info: Vec<HopRequire>,

    /// the TLC expiry delta should be used to set the timelock for the final hop, in milliseconds
    #[serde_as(as = "Option<U64Hex>")]
    pub final_tlc_expiry_delta: Option<u64>,
}

/// The router returned by build_router.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BuildPaymentRouterResult {
    /// The hops information for router
    pub router_hops: Vec<RouterHop>,
}

/// Parameters for sending a payment with a specified router.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct SendPaymentWithRouterParams {
    /// the hash to use within the payment's HTLC.
    /// If not set and `keysend` is set to true, a random hash will be generated.
    /// If not set and there is a `payment_hash` in the invoice, it will be used.
    /// Otherwise, `payment_hash` need to be set.
    pub payment_hash: Option<Hash256>,

    /// The router to use for the payment
    pub router: Vec<RouterHop>,

    /// the encoded invoice to send to the recipient
    pub invoice: Option<String>,

    /// Some custom records for the payment which contains a map of u32 to Vec<u8>
    /// The key is the record type, and the value is the serialized data.
    /// Limits: the sum size of values can not exceed 2048 bytes.
    ///
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
    pub udt_type_script: Option<Script>,

    /// dry_run for payment, used for check whether we can build valid router and the fee for this payment,
    /// it's useful for the sender to double check the payment before sending it to the network,
    /// default is false
    pub dry_run: Option<bool>,
}

/// Parameters for querying graph nodes.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphNodesParams {
    #[serde_as(as = "Option<U64Hex>")]
    /// The maximum number of nodes to return.
    pub limit: Option<u64>,
    /// The cursor to start returning nodes from.
    pub after: Option<JsonBytes>,
}

/// The UDT script which is used to identify the UDT configuration for a Fiber Node.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtScript {
    /// The code hash of the script.
    pub code_hash: H256,
    /// The hash type of the script.
    pub hash_type: ScriptHashType,
    /// The arguments of the script.
    pub args: String,
}

impl From<fiber_types::UdtScript> for UdtScript {
    fn from(cfg: fiber_types::UdtScript) -> Self {
        UdtScript {
            code_hash: cfg.code_hash,
            hash_type: cfg.hash_type.into(),
            args: cfg.args,
        }
    }
}

/// Udt script on-chain dependencies.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtDep {
    /// cell dep described by out_point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_dep: Option<UdtCellDep>,
    /// cell dep described by type ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<Script>,
}

impl From<fiber_types::UdtDep> for UdtDep {
    fn from(cfg: fiber_types::UdtDep) -> Self {
        UdtDep {
            cell_dep: cfg.cell_dep.map(Into::into),
            type_id: cfg.type_id,
        }
    }
}

/// The UDT cell dep which is used to identify the UDT configuration for a Fiber Node.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCellDep {
    /// The out point of the cell dep.
    pub out_point: OutPointWrapper,
    /// The type of the cell dep.
    pub dep_type: DepType,
}

impl From<fiber_types::UdtCellDep> for UdtCellDep {
    fn from(cfg: fiber_types::UdtCellDep) -> Self {
        UdtCellDep {
            dep_type: cfg.dep_type.into(),
            out_point: cfg.out_point,
        }
    }
}

/// The UDT argument info which is used to identify the UDT configuration.
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtArgInfo {
    /// The name of the UDT.
    pub name: String,
    /// The script of the UDT.
    pub script: UdtScript,
    #[serde_as(as = "Option<U128Hex>")]
    /// The minimum amount of the UDT that can be automatically accepted.
    pub auto_accept_amount: Option<u128>,
    /// The cell deps of the UDT.
    pub cell_deps: Vec<UdtDep>,
}

impl From<fiber_types::UdtArgInfo> for UdtArgInfo {
    fn from(cfg: fiber_types::UdtArgInfo) -> Self {
        UdtArgInfo {
            name: cfg.name,
            script: UdtScript {
                code_hash: cfg.script.code_hash,
                hash_type: cfg.script.hash_type.into(),
                args: cfg.script.args,
            },
            cell_deps: cfg.cell_deps.into_iter().map(Into::into).collect(),
            auto_accept_amount: cfg.auto_accept_amount,
        }
    }
}

/// A list of UDT configuration infos.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCfgInfos(
    /// The list of UDT configuration infos.
    pub Vec<UdtArgInfo>,
);

impl From<fiber_types::UdtCfgInfos> for UdtCfgInfos {
    fn from(cfg: fiber_types::UdtCfgInfos) -> Self {
        UdtCfgInfos(cfg.0.into_iter().map(Into::into).collect())
    }
}

/// The Node information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    /// The name of the node.
    pub node_name: String,
    /// The version of the node.
    pub version: String,
    /// The addresses of the node.
    pub addresses: Vec<MultiAddr>,
    /// The node features supported by the node.
    pub features: Vec<String>,
    /// The identity public key of the node (secp256k1 compressed, hex string), same as `pubkey` in `list_peers`.
    pub pubkey: Pubkey,
    #[serde_as(as = "U64Hex")]
    /// The latest timestamp set by the owner for the node announcement.
    /// When a Node is online this timestamp will be updated to the latest value.
    pub timestamp: u64,
    /// The chain hash of the node.
    pub chain_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    /// The minimum CKB funding amount for automatically accepting open channel requests.
    pub auto_accept_min_ckb_funding_amount: u64,
    /// The UDT configuration infos of the node.
    pub udt_cfg_infos: UdtCfgInfos,
}

/// Result of querying graph nodes.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphNodesResult {
    /// The list of nodes.
    pub nodes: Vec<NodeInfo>,
    /// The last cursor.
    pub last_cursor: JsonBytes,
}

/// Parameters for querying graph channels.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphChannelsParams {
    /// The maximum number of channels to return.
    #[serde_as(as = "Option<U64Hex>")]
    pub limit: Option<u64>,
    /// The cursor to start returning channels from.
    pub after: Option<JsonBytes>,
}

/// The Channel information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelInfo {
    /// The outpoint of the channel.
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// The identity public key of the first node (secp256k1 compressed, hex string).
    pub node1: Pubkey,
    /// The identity public key of the second node (secp256k1 compressed, hex string).
    pub node2: Pubkey,
    /// The created timestamp of the channel, which is the block header timestamp of the block
    /// that contains the channel funding transaction.
    #[serde_as(as = "U64Hex")]
    pub created_timestamp: u64,

    /// The update info from node1 to node2, e.g. timestamp, fee_rate, tlc_expiry_delta, tlc_minimum_value
    pub update_info_of_node1: Option<ChannelUpdateInfo>,

    /// The update info from node2 to node1, e.g. timestamp, fee_rate, tlc_expiry_delta, tlc_minimum_value
    pub update_info_of_node2: Option<ChannelUpdateInfo>,

    /// The capacity of the channel.
    #[serde_as(as = "U128Hex")]
    pub capacity: u128,
    /// The chain hash of the channel.
    pub chain_hash: Hash256,
    /// The UDT type script of the channel.
    pub udt_type_script: Option<Script>,
}

/// Result of querying graph channels.
#[derive(Serialize, Deserialize, Clone)]
pub struct GraphChannelsResult {
    /// A list of channels.
    pub channels: Vec<ChannelInfo>,
    /// The last cursor for pagination.
    pub last_cursor: JsonBytes,
}

/// The attributes of the invoice.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribute {
    #[serde(with = "U64Hex")]
    /// This attribute is deprecated since v0.6.0, The final tlc time out, in milliseconds
    FinalHtlcTimeout(u64),
    #[serde(with = "U64Hex")]
    /// The final tlc minimum expiry delta, in milliseconds, default is 1 day
    FinalHtlcMinimumExpiryDelta(u64),
    #[serde(with = "duration_hex")]
    /// The expiry time of the invoice, in seconds
    ExpiryTime(std::time::Duration),
    /// The description of the invoice
    Description(String),
    /// The fallback address of the invoice
    FallbackAddr(String),
    /// The udt type script of the invoice
    UdtScript(CkbScript),
    /// The payee public key of the invoice
    PayeePublicKey(PublicKey),
    /// The hash algorithm of the invoice
    HashAlgorithm(HashAlgorithm),
    /// The feature flags of the invoice
    Feature(Vec<String>),
    /// The payment secret of the invoice
    PaymentSecret(Hash256),
}

/// The metadata of the invoice.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvoiceData {
    /// The timestamp of the invoice
    #[serde_as(as = "U128Hex")]
    pub timestamp: u128,
    /// The payment hash of the invoice
    pub payment_hash: Hash256,
    /// The attributes of the invoice, e.g. description, expiry time, etc.
    pub attrs: Vec<Attribute>,
}

/// Represents a syntactically and semantically correct lightning BOLT11 invoice.
///
/// There are three ways to construct a `CkbInvoice`:
///  1. using [`CkbInvoiceBuilder`]
///  2. using `str::parse::<CkbInvoice>(&str)` (see [`CkbInvoice::from_str`])
///
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CkbInvoice {
    /// The currency of the invoice
    pub currency: Currency,
    #[serde_as(as = "Option<U128Hex>")]
    /// The amount of the invoice
    pub amount: Option<u128>,
    /// The signature of the invoice
    pub signature: Option<InvoiceSignature>,
    /// The invoice data, including the payment hash, timestamp and other attributes
    pub data: InvoiceData,
}

/// The parameter struct for generating a new invoice.
#[serde_as]
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct NewInvoiceParams {
    /// The amount of the invoice.
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The description of the invoice.
    pub description: Option<String>,
    /// The currency of the invoice.
    pub currency: Currency,
    /// The preimage to settle an incoming TLC payable to this invoice. If preimage is set, hash must be absent.
    /// If both preimage and hash are absent, a random preimage is generated.
    pub payment_preimage: Option<Hash256>,
    /// The hash of the preimage. If hash is set, preimage must be absent. This condition indicates a 'hold invoice'
    /// for which the tlc must be accepted and held until the preimage becomes known.
    pub payment_hash: Option<Hash256>,
    /// The expiry time of the invoice, in seconds.
    #[serde_as(as = "Option<U64Hex>")]
    pub expiry: Option<u64>,
    /// The fallback address of the invoice.
    pub fallback_address: Option<String>,
    /// The final HTLC timeout of the invoice, in milliseconds.
    /// Minimal value is 16 hours, and maximal value is 14 days.
    #[serde_as(as = "Option<U64Hex>")]
    pub final_expiry_delta: Option<u64>,
    /// The UDT type script of the invoice.
    pub udt_type_script: Option<Script>,
    /// The hash algorithm of the invoice.
    pub hash_algorithm: Option<HashAlgorithm>,
    /// Whether allow payment to use MPP
    pub allow_mpp: Option<bool>,
    /// Whether allow payment to use trampoline routing
    pub allow_trampoline_routing: Option<bool>,
}

/// Result of creating a new invoice.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct InvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
}

/// Parameters for parsing an invoice.
#[derive(Serialize, Deserialize)]
pub struct ParseInvoiceParams {
    /// The encoded invoice address.
    pub invoice: String,
}

/// Result of parsing an invoice.
#[derive(Clone, Serialize, Deserialize)]
pub struct ParseInvoiceResult {
    /// The invoice.
    pub invoice: CkbInvoice,
}

/// Parameters for getting an invoice by payment hash.
#[derive(Serialize, Deserialize, Debug)]
pub struct InvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
}

/// Parameters for settling an invoice.
#[derive(Serialize, Deserialize, Debug)]
pub struct SettleInvoiceParams {
    /// The payment hash of the invoice.
    pub payment_hash: Hash256,
    /// The payment preimage of the invoice.
    pub payment_preimage: Hash256,
}

/// Result of settling an invoice.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SettleInvoiceResult {}

/// The status of the invoice.
#[derive(Clone, Serialize, Deserialize)]
pub struct GetInvoiceResult {
    /// The encoded invoice address.
    pub invoice_address: String,
    /// The invoice.
    pub invoice: CkbInvoice,
    /// The invoice status
    pub status: CkbInvoiceStatus,
}

/// Node information result.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct NodeInfoResult {
    /// The version of the node software.
    pub version: String,

    /// The commit hash of the node software.
    pub commit_hash: String,

    /// The identity public key of this node (secp256k1 compressed, hex string).
    pub pubkey: Pubkey,

    /// The features supported by the node.
    pub features: Vec<String>,

    /// The optional name of the node.
    pub node_name: Option<String>,

    /// A list of multi-addresses associated with the node.
    pub addresses: Vec<MultiAddr>,

    /// The hash of the blockchain that the node is connected to.
    pub chain_hash: Hash256,

    /// The minimum CKB funding amount for automatically accepting open channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    pub open_channel_auto_accept_min_ckb_funding_amount: u64,

    /// The CKB funding amount for automatically accepting channel requests, serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    pub auto_accept_channel_ckb_funding_amount: u64,

    /// The default funding lock script for the node.
    pub default_funding_lock_script: Script,

    /// The locktime expiry delta for Time-Locked Contracts (TLC), serialized as a hexadecimal string.
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,

    /// The minimum value for Time-Locked Contracts (TLC) we can send, serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    pub tlc_min_value: u128,

    /// The fee (to forward payments) proportional to the value of Time-Locked Contracts (TLC),
    /// expressed in millionths and serialized as a hexadecimal string.
    #[serde_as(as = "U128Hex")]
    pub tlc_fee_proportional_millionths: u128,

    /// The number of channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    pub channel_count: u32,

    /// The number of pending channels associated with the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    pub pending_channel_count: u32,

    /// The number of peers connected to the node, serialized as a hexadecimal string.
    #[serde_as(as = "U32Hex")]
    pub peers_count: u32,

    /// Configuration information for User-Defined Tokens (UDT) associated with the node.
    pub udt_cfg_infos: UdtCfgInfos,
}

/// Parameters for connecting to a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConnectPeerParams {
    /// The address of the peer to connect to.
    /// Either `address` or `pubkey` must be provided.
    pub address: Option<MultiAddr>,
    /// The public key of the peer to connect to.
    /// The node resolves the address from locally synced graph data.
    pub pubkey: Option<Pubkey>,
    /// Whether to save the peer address to the peer store.
    pub save: Option<bool>,
}

/// Parameters for disconnecting from a peer.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct DisconnectPeerParams {
    /// The public key of the peer to disconnect.
    pub pubkey: Pubkey,
}

/// The information about a peer connected to the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// The identity public key of the peer.
    pub pubkey: Pubkey,

    /// The multi-address associated with the connecting peer.
    /// Note: this is only the address which used for connecting to the peer, not all addresses of the peer.
    /// The `graph_nodes` in Graph rpc module will return all addresses of the peer.
    pub address: MultiAddr,
}

/// The result of the `list_peers` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPeersResult {
    /// A list of connected peers.
    pub peers: Vec<PeerInfo>,
}

/// Parameters for sending BTC via cross-chain hub.
#[cfg(feature = "cch")]
#[derive(Serialize, Deserialize)]
pub struct SendBTCParams {
    /// Payment request string for the BTC Lightning payee.
    ///
    /// The invoice should not be expired soon. The remaining expiry time should be greater than the CCH config
    /// `min_incoming_invoice_expiry_delta_seconds`.
    pub btc_pay_req: String,
    /// Request currency
    pub currency: Currency,
}

/// Cross-chain hub order response.
#[cfg(feature = "cch")]
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrderResponse {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    /// Relative expiry time in seconds from `created_at` that the order expires
    #[serde_as(as = "U64Hex")]
    pub expiry_delta_seconds: u64,

    /// Wrapped BTC type script
    pub wrapped_btc_type_script: Script,

    /// Generated invoice for the incoming payment
    pub incoming_invoice: CchInvoice,
    /// The final payee to accept the payment. It has the different network with incoming invoice.
    pub outgoing_pay_req: String,
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
    /// Amount required to pay in Satoshis, including fee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    /// Order status
    pub status: CchOrderStatus,
}

/// Parameters for receiving BTC via cross-chain hub.
#[cfg(feature = "cch")]
#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ReceiveBTCParams {
    /// Payment request string for the CKB Fiber payee.
    ///
    /// The invoice should not be expired soon. The remaining expiry time should be greater than the CCH config
    /// `min_incoming_invoice_expiry_delta_seconds`.
    pub fiber_pay_req: String,
}

/// Parameters for getting a CCH order.
#[cfg(feature = "cch")]
#[derive(Serialize, Deserialize)]
pub struct GetCchOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
}

#[cfg(feature = "cch")]
impl From<CchOrder> for CchOrderResponse {
    fn from(value: CchOrder) -> Self {
        Self {
            timestamp: value.created_at,
            expiry_delta_seconds: value.expiry_delta_seconds,
            wrapped_btc_type_script: value.wrapped_btc_type_script,
            outgoing_pay_req: value.outgoing_pay_req,
            incoming_invoice: value.incoming_invoice,
            payment_hash: value.payment_hash,
            amount_sats: value.amount_sats,
            fee_sats: value.fee_sats,
            status: value.status,
        }
    }
}

/// Parameters for sending a commitment_signed message.
// TODO @quake remove this unnecessary pub(crate) struct and rpc after refactoring
#[derive(Serialize, Deserialize, Debug)]
pub struct CommitmentSignedParams {
    /// The channel ID of the channel to send the commitment_signed message to
    pub channel_id: Hash256,
}

/// Parameters for adding a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct AddTlcParams {
    /// The channel ID of the channel to add the TLC to
    pub channel_id: Hash256,
    /// The amount of the TLC
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The payment hash of the TLC
    pub payment_hash: Hash256,
    /// The expiry of the TLC
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// The hash algorithm of the TLC
    pub hash_algorithm: Option<HashAlgorithm>,
}

/// Result of adding a TLC.
#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct AddTlcResult {
    /// The ID of the TLC
    #[serde_as(as = "U64Hex")]
    pub tlc_id: u64,
}

/// Parameters for removing a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoveTlcParams {
    /// The channel ID of the channel to remove the TLC from
    pub channel_id: Hash256,
    #[serde_as(as = "U64Hex")]
    /// The ID of the TLC to remove
    pub tlc_id: u64,
    /// The reason for removing the TLC, either a 32-byte hash for preimage fulfillment or an u32 error code for removal
    pub reason: RemoveTlcReason,
}

/// The reason for removing a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum RemoveTlcReason {
    /// The reason for removing the TLC is that it was fulfilled
    RemoveTlcFulfill { payment_preimage: Hash256 },
    /// The reason for removing the TLC is that it failed
    RemoveTlcFail { error_code: String },
}

/// Parameters for submitting a commitment transaction.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubmitCommitmentTransactionParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Commitment number
    #[serde_as(as = "U64Hex")]
    pub commitment_number: u64,
}

/// Result of submitting a commitment transaction.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubmitCommitmentTransactionResult {
    /// Submitted commitment transaction hash
    pub tx_hash: Hash256,
}

/// Parameters for checking channel shutdown.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckChannelShutdownParams {
    /// Channel ID
    pub channel_id: Hash256,
}

/// Parameters for creating a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Funding UDT type script
    pub funding_udt_type_script: Option<Script>,
    /// The local party's private key used to settle the commitment transaction
    pub local_settlement_key: Privkey,
    /// The remote party's public key used to settle the commitment transaction
    pub remote_settlement_key: Pubkey,
    /// The local party's funding public key
    pub local_funding_pubkey: Pubkey,
    /// The remote party's funding public key
    pub remote_funding_pubkey: Pubkey,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for removing a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoveWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
}

/// Parameters for updating revocation.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateRevocationParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Revocation data
    pub revocation_data: RevocationData,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for updating pending remote settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdatePendingRemoteSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for updating local settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateLocalSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for creating a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreatePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
    /// Preimage
    pub preimage: Hash256,
}

/// Parameters for removing a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemovePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
}

/// Parameters for profiling.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PprofParams {
    /// Duration to profile in seconds. Defaults 10s.
    #[serde_as(as = "Option<U64Hex>")]
    pub duration_secs: Option<u64>,
}

/// Result of profiling.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PprofResult {
    /// Path of the generated flamegraph SVG.
    pub path: String,
}

/// RPC context for watchtower operations.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcContext {
    /// Node ID, read from user RPC biscuit token
    pub node_id: NodeId,
}

impl From<InternalAttribute> for Attribute {
    fn from(attr: InternalAttribute) -> Self {
        match attr {
            InternalAttribute::FinalHtlcTimeout(timeout) => Attribute::FinalHtlcTimeout(timeout),
            InternalAttribute::FinalHtlcMinimumExpiryDelta(delta) => {
                Attribute::FinalHtlcMinimumExpiryDelta(delta)
            }
            InternalAttribute::ExpiryTime(duration) => Attribute::ExpiryTime(duration),
            InternalAttribute::Description(desc) => Attribute::Description(desc),
            InternalAttribute::FallbackAddr(addr) => Attribute::FallbackAddr(addr),
            InternalAttribute::UdtScript(script) => Attribute::UdtScript(script),
            InternalAttribute::PayeePublicKey(pubkey) => Attribute::PayeePublicKey(pubkey),
            InternalAttribute::HashAlgorithm(alg) => Attribute::HashAlgorithm(alg),
            InternalAttribute::Feature(feature) => {
                Attribute::Feature(feature.enabled_features_names())
            }
            InternalAttribute::PaymentSecret(secret) => Attribute::PaymentSecret(secret),
        }
    }
}

impl From<InternalInvoiceData> for InvoiceData {
    fn from(data: InternalInvoiceData) -> Self {
        InvoiceData {
            timestamp: data.timestamp,
            payment_hash: data.payment_hash,
            attrs: data.attrs.into_iter().map(|a| a.into()).collect(),
        }
    }
}

impl From<InternalCkbInvoice> for CkbInvoice {
    fn from(inv: InternalCkbInvoice) -> Self {
        CkbInvoice {
            currency: inv.currency,
            amount: inv.amount,
            signature: inv.signature,
            data: inv.data.into(),
        }
    }
}

impl From<PaymentCustomRecords> for InternalPaymentCustomRecords {
    fn from(records: PaymentCustomRecords) -> Self {
        InternalPaymentCustomRecords { data: records.data }
    }
}
