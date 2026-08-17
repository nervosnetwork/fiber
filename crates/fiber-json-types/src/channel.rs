//! Channel management types for the Fiber Network JSON-RPC API.

use crate::define_rpc_flags;
use crate::schema_helpers::*;
use crate::serde_utils::{EntityHex, Hash256, Pubkey, SliceHex, U128Hex, U64Hex};
use ckb_jsonrpc_types::{CellDep, EpochNumberWithFraction, Script, Transaction};
use ckb_types::packed::CellOutput;
use ckb_types::packed::OutPoint;
use ckb_types::H256;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Serde default function returning `Some(true)`.
/// Used for `Option<bool>` fields whose server-side default is `true`.
fn default_true() -> Option<bool> {
    Some(true)
}

define_rpc_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct NegotiatingFundingFlags(u32) {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = 1 | (1 << 1);
        const AWAITING_EXTERNAL_FUNDING = 1 << 2;
    }
}

define_rpc_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct CollaboratingFundingTxFlags(u32) {
        const AWAITING_REMOTE_TX_COLLABORATION_MSG = 1;
        const PREPARING_LOCAL_TX_COLLABORATION_MSG = 1 << 1;
        const OUR_TX_COMPLETE_SENT = 1 << 2;
        const THEIR_TX_COMPLETE_SENT = 1 << 3;
        const COLLABORATION_COMPLETED = (1 << 2) | (1 << 3);
    }
}

define_rpc_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct SigningCommitmentFlags(u32) {
        const OUR_COMMITMENT_SIGNED_SENT = 1;
        const THEIR_COMMITMENT_SIGNED_SENT = 1 << 1;
        const COMMITMENT_SIGNED_SENT = 1 | (1 << 1);
    }
}

define_rpc_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct AwaitingTxSignaturesFlags(u32) {
        const OUR_TX_SIGNATURES_SENT = 1;
        const THEIR_TX_SIGNATURES_SENT = 1 << 1;
        const TX_SIGNATURES_SENT = 1 | (1 << 1);
    }
}

define_rpc_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct AwaitingChannelReadyFlags(u32) {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = 1 | (1 << 1);
    }
}

define_rpc_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct ShuttingDownFlags(u32) {
        const OUR_SHUTDOWN_SENT = 1;
        const THEIR_SHUTDOWN_SENT = 1 << 1;
        const AWAITING_PENDING_TLCS = 1 | (1 << 1);
        const DROPPING_PENDING = 1 << 2;
        const WAITING_COMMITMENT_CONFIRMATION = 1 << 3;
    }
}

define_rpc_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct CloseFlags(u32) {
        const COOPERATIVE = 1;
        const UNCOOPERATIVE_LOCAL = 1 << 1;
        const ABANDONED = 1 << 2;
        const FUNDING_ABORTED = 1 << 3;
        const UNCOOPERATIVE_REMOTE = 1 << 4;
        const WAITING_ONCHAIN_SETTLEMENT = 1 << 5;
    }
}

/// Parameters for opening a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
pub struct OpenChannelParams {
    /// The public key of the peer to open a channel with.
    /// The peer must be connected through the [connect_peer](#peer-connect_peer) rpc first.
    pub pubkey: Pubkey,

    /// The amount of CKB or UDT to fund the channel with.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub funding_amount: u128,

    /// Whether this is a public channel (will be broadcasted to network, and can be used to forward TLCs),
    /// an optional parameter, default value is true.
    #[serde(default = "default_true")]
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
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub commitment_fee_rate: Option<u64>,

    /// The fee rate for the funding transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub funding_fee_rate: Option<u64>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 4 hours, which is 4 * 60 * 60 * 1000 milliseconds
    /// Expect it >= 2/3 commitment_delay_epoch.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_expiry_delta: Option<u64>,

    /// The minimum value for a TLC our side can send,
    /// an optional parameter, default is 0, which means we can send any TLC is larger than 0.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_min_value: Option<u128>,

    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// The maximum total value of in-flight TLCs our side will accept from the peer, an optional parameter.
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub max_tlc_value_in_flight: Option<u128>,

    /// The maximum number of in-flight TLCs our side will accept from the peer, an optional parameter, default is 125
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub max_tlc_number_in_flight: Option<u64>,
}

/// Result of opening a channel.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenChannelResult {
    /// The temporary channel ID of the channel being opened
    pub temporary_channel_id: Hash256,
}

/// Parameters for opening a channel with external funding.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct OpenChannelWithExternalFundingParams {
    /// The identity public key of the peer to open a channel with.
    /// The peer must already be connected through the [connect_peer](#peer-connect_peer) rpc first.
    pub pubkey: Pubkey,

    /// The amount of CKB or UDT to fund the channel with.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub funding_amount: u128,

    /// Whether this is a public channel (will be broadcasted to network, and can be used to forward TLCs), an optional parameter, default value is true.
    pub public: Option<bool>,

    /// The type script of the UDT to fund the channel with, an optional parameter.
    pub funding_udt_type_script: Option<Script>,

    /// The script used to receive the channel balance when the channel is closed. This is REQUIRED for external funding.
    pub shutdown_script: Script,

    /// The lock script that controls the funding cells. The node will collect cells with this lock script
    /// to build the funding transaction. The user must be able to sign for this lock script.
    pub funding_lock_script: Script,

    /// Optional extra cell deps required by `funding_lock_script`.
    /// This is useful for custom wallet lock scripts whose deps are not part of the genesis defaults.
    pub funding_lock_script_cell_deps: Option<Vec<CellDep>>,

    /// The delay time for the commitment transaction, must be an
    /// [EpochNumberWithFraction](https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0017-tx-valid-since/e-i-l-encoding.png)
    /// in u64 format, an optional parameter, default value is 1 epoch, which is 4 hours.
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,

    /// The fee rate for the commitment transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub commitment_fee_rate: Option<u64>,

    /// The fee rate for the funding transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub funding_fee_rate: Option<u64>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 4 hours, which is 4 * 60 * 60 * 1000 milliseconds
    /// Expect it >= 2/3 commitment_delay_epoch.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_expiry_delta: Option<u64>,

    /// The minimum value for a TLC our side can send,
    /// an optional parameter, default is 0, which means we can send any TLC is larger than 0.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_min_value: Option<u128>,

    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// The maximum total value of in-flight TLCs our side will accept from the peer, an optional parameter.
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub max_tlc_value_in_flight: Option<u128>,

    /// The maximum number of in-flight TLCs our side will accept from the peer, an optional parameter, default is 125
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub max_tlc_number_in_flight: Option<u64>,

    /// Optional public channel-signer material. When present, the node treats the
    /// channel as externally signed and never holds or falls back to local channel keys.
    pub external_channel_signer: Option<ChannelOpenSignerMaterial>,
}

/// Result of opening a channel with external funding.
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct OpenChannelWithExternalFundingResult {
    /// The channel ID of the channel being opened.
    pub channel_id: Hash256,

    /// The final unsigned funding transaction that needs to be signed.
    pub unsigned_funding_tx: Transaction,
}

/// Parameters for submitting a signed funding transaction.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SubmitSignedFundingTxParams {
    /// The channel ID returned from `open_channel_with_external_funding`.
    pub channel_id: Hash256,

    /// The signed funding transaction.
    pub signed_funding_tx: Transaction,
}

/// Result of submitting a signed funding transaction.
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct SubmitSignedFundingTxResult {
    /// The channel ID.
    pub channel_id: Hash256,

    /// The hash of the funding transaction that was submitted.
    pub funding_tx_hash: Hash256,
}

/// Parameters for abandoning a channel.
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
pub struct AbandonChannelParams {
    /// The temporary channel ID or real channel ID of the channel being abandoned
    pub channel_id: Hash256,
}

/// Parameters for accepting a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
pub struct AcceptChannelParams {
    /// The temporary channel ID of the channel to accept
    pub temporary_channel_id: Hash256,

    /// The amount of CKB or UDT to fund the channel with
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub funding_amount: u128,

    /// The script used to receive the channel balance, an optional parameter,
    /// default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key
    pub shutdown_script: Option<Script>,

    /// The maximum total value of in-flight TLCs our side will accept from the peer, default is u128::MAX
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub max_tlc_value_in_flight: Option<u128>,

    /// The maximum number of in-flight TLCs our side will accept from the peer, default is 125
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub max_tlc_number_in_flight: Option<u64>,

    /// The minimum value for a TLC our side can send,
    /// an optional parameter, default is 0, which means we can send any TLC is larger than 0.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_min_value: Option<u128>,

    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 1 day, which is 24 * 60 * 60 * 1000 milliseconds
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_expiry_delta: Option<u64>,
}

/// Result of accepting a channel.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct AcceptChannelResult {
    /// The final ID of the channel that was accepted, it's different from the temporary channel ID
    pub channel_id: Hash256,
}

/// Parameters for listing channels.
#[serde_as]
#[derive(Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct ListChannelsResult {
    /// The list of channels
    pub channels: Vec<Channel>,
}

/// The state of a channel.
///
/// Serialized with adjacently-tagged representation using PascalCase variant names and flags.
/// This is different from the internal `ChannelState` in fiber-types which uses
/// default serde for bincode compatibility.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state_name", content = "state_flags")]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    /// For channels opened with external funding, this state is also used together with
    /// `NegotiatingFundingFlags::AWAITING_EXTERNAL_FUNDING` to indicate that we are waiting
    /// for the user to sign and submit the funding transaction externally.
    NegotiatingFunding(#[schemars(schema_with = "schema_as_string")] NegotiatingFundingFlags),
    /// We're collaborating with the other party on the funding transaction.
    CollaboratingFundingTx(
        #[schemars(schema_with = "schema_as_string")] CollaboratingFundingTxFlags,
    ),
    /// We have collaborated over the funding and are now waiting for CommitmentSigned messages.
    SigningCommitment(#[schemars(schema_with = "schema_as_string")] SigningCommitmentFlags),
    /// We've received and sent `commitment_signed` and are now waiting for both
    /// party to collaborate on creating a valid funding transaction.
    AwaitingTxSignatures(#[schemars(schema_with = "schema_as_string")] AwaitingTxSignaturesFlags),
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(#[schemars(schema_with = "schema_as_string")] AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady,
    /// We've successfully negotiated a `closing_signed` dance. At this point, the `ChannelManager`
    ShuttingDown(#[schemars(schema_with = "schema_as_string")] ShuttingDownFlags),
    /// This channel is closed.
    Closed(#[schemars(schema_with = "schema_as_string")] CloseFlags),
    /// The channel state is potentially outdated (e.g., after a database restore).
    /// We must perform a passive audit with the peer before resuming operations.
    Stale,
}

/// The channel data structure.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
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
    /// The outpoint of the channel
    #[serde_as(as = "Option<EntityHex>")]
    #[schemars(schema_with = "schema_as_hex_bytes_optional")]
    pub channel_outpoint: Option<OutPoint>,
    /// The public key of the channel counterparty.
    pub pubkey: Pubkey,
    /// The UDT type script of the channel
    pub funding_udt_type_script: Option<Script>,
    /// The state of the channel
    pub state: ChannelState,
    /// The local balance of the channel
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub local_balance: u128,
    /// The offered balance of the channel
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub offered_tlc_balance: u128,
    /// The remote balance of the channel
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub remote_balance: u128,
    /// The received balance of the channel
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub received_tlc_balance: u128,
    /// The list of pending tlcs
    pub pending_tlcs: Vec<Htlc>,
    /// The hash of the latest commitment transaction
    pub latest_commitment_transaction_hash: Option<H256>,
    /// The time the channel was created at, in milliseconds from UNIX epoch
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_at: u64,
    /// Whether the channel is enabled
    pub enabled: bool,
    /// The expiry delta to forward a tlc, in milliseconds, default to 1 day, which is 24 * 60 * 60 * 1000 milliseconds
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_expiry_delta: u64,
    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_fee_proportional_millionths: u128,
    /// The hash of the shutdown transaction
    pub shutdown_transaction_hash: Option<H256>,
    /// Human-readable reason why the channel opening failed.
    /// Only present when the channel is in a failed state (e.g. abandoned or funding aborted).
    pub failure_detail: Option<String>,
}

/// The status of a tlc.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub enum TlcStatus {
    /// Outbound tlc
    Outbound(OutboundTlcStatus),
    /// Inbound tlc
    Inbound(InboundTlcStatus),
}

/// The status of an outbound tlc.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub enum OutboundTlcStatus {
    /// Offered tlc created and sent to remote party
    LocalAnnounced,
    /// Received ACK from remote party for this offered tlc
    Committed,
    /// Remote party removed this tlc
    RemoteRemoved,
    /// We received another RemoveTlc message from peer when we are waiting for the ack of the last one.
    RemoveWaitPrevAck,
    /// We have sent commitment signed to peer and waiting ACK for confirming this RemoveTlc
    RemoveWaitAck,
    /// We have received the ACK for the RemoveTlc, it's safe to remove this tlc
    RemoveAckConfirmed,
}

/// The status of an inbound tlc.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub enum InboundTlcStatus {
    /// Received tlc from remote party, but not committed yet
    RemoteAnnounced,
    /// We received another AddTlc peer message when we are waiting for the ack of the last one.
    AnnounceWaitPrevAck,
    /// We have sent commitment signed to peer and waiting ACK for confirming this AddTlc
    AnnounceWaitAck,
    /// We have received ACK from peer and Committed this tlc
    Committed,
    /// We have removed this tlc, but haven't received ACK from peer
    LocalRemoved,
    /// We have received the ACK for the RemoveTlc, it's safe to remove this tlc
    RemoveAckConfirmed,
}

/// The htlc data structure.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct Htlc {
    /// The id of the htlc
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub id: u64,
    /// The amount of the htlc
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// The payment hash of the htlc
    pub payment_hash: Hash256,
    /// The expiry of the htlc
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expiry: u64,
    /// If this HTLC is involved in a forwarding operation, this field indicates the forwarding channel.
    /// For an outbound htlc, it is the inbound channel. For an inbound htlc, it is the outbound channel.
    pub forwarding_channel_id: Option<Hash256>,
    /// If this HTLC is involved in a forwarding operation, this field indicates the forwarding tlc id.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub forwarding_tlc_id: Option<u64>,
    /// The status of the htlc
    pub status: TlcStatus,
}

/// Parameters for shutting down a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct ShutdownChannelParams {
    /// The channel ID of the channel to shut down
    pub channel_id: Hash256,
    /// The script used to receive the channel balance, only support secp256k1_blake160_sighash_all script for now
    /// default is `default_funding_lock_script` in `CkbConfig`
    pub close_script: Option<Script>,
    /// The fee rate for the closing transaction, the fee will be deducted from the closing initiator's channel balance
    /// default is 1000 shannons/KW
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub fee_rate: Option<u64>,
    /// Whether to force the channel to close, when set to false, `close_script` and `fee_rate` should be set, default is false.
    /// When set to true, `close_script` and `fee_rate` will be ignored and will use the default value when opening the channel.
    pub force: Option<bool>,
}

/// Parameters for updating a channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct UpdateChannelParams {
    /// The channel ID of the channel to update
    pub channel_id: Hash256,
    /// Whether the channel is enabled, default value is true
    #[serde(default = "default_true")]
    pub enabled: Option<bool>,
    /// The expiry delta for the TLC locktime
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_expiry_delta: Option<u64>,
    /// The minimum value for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_minimum_value: Option<u128>,
    /// The fee proportional millionths for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub tlc_fee_proportional_millionths: Option<u128>,
}

/// Parameters for querying a channel's external signing status.
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct GetChannelSigningStatusParams {
    /// The channel whose signer state should be read.
    pub channel_id: Hash256,
}

/// Result of querying a channel's external signing status.
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct GetChannelSigningStatusResult {
    /// The channel whose signer state was read.
    pub channel_id: Hash256,
    /// Current signer status for this channel.
    pub status: ChannelSigningStatus,
}

/// Read-only projection of a channel's signer sub-state.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum ChannelSigningStatus {
    /// This channel uses the node's local signer.
    Internal,
    /// This channel uses an external signer, but no signature is currently required.
    NoSignatureRequired,
    /// Channel processing is paused until this exact signature is submitted.
    SignatureRequired {
        /// Identifier of the outstanding signature request.
        request_id: Hash256,
        /// Semantic channel transition that produced this request.
        transition: ChannelSigningTransition,
        /// Structured MuSig2 plaintext independently hashed by the external signer.
        content: Musig2SigningContent,
    },
}

/// Public semantic label for a channel signing transition.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub enum ChannelSigningTransition {
    /// Sign and then send our `CommitmentSigned` message.
    SendCommitmentSigned,
    /// Complete processing a peer `CommitmentSigned` after our signature is supplied.
    CompleteReceivedCommitment,
    /// Sign and then send our `RevokeAndAck` message.
    SendRevokeAndAck,
    /// Complete processing a peer `RevokeAndAck` after our signature is supplied.
    CompleteReceivedRevokeAndAck,
    /// Sign and then send our `ClosingSigned` message.
    SendClosingSigned,
    /// Sign the public channel announcement.
    SignChannelAnnouncement,
}

/// Matches Fiber's native MuSig2 nonce contexts.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub enum NoncePurpose {
    /// Fiber's commitment nonce derivation.
    Commitment,
    /// Fiber's revocation nonce derivation.
    Revocation,
    /// The one-off public-channel announcement signature.
    ChannelAnnouncement,
}

/// Unique deterministic MuSig2 nonce location within one channel signer.
#[serde_as]
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub struct NonceSlot {
    /// Signing domain for the nonce.
    pub purpose: NoncePurpose,
    /// Commitment number for commitment/revocation slots; zero for announcement.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub commitment_number: u64,
}

/// Selects which Fiber commitment counter supplies a signing request's number.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub enum CommitmentCounter {
    /// The local commitment counter.
    Local,
    /// The remote commitment counter.
    Remote,
}

/// Plaintext MuSig2 payload from which the signer computes the signing digest.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum Musig2SignableContent {
    /// An unsigned commitment transaction.
    CommitmentTransaction {
        /// Unsigned commitment transaction.
        transaction: Transaction,
    },
    /// An unsigned cooperative close transaction.
    CooperativeCloseTransaction {
        /// Unsigned cooperative close transaction.
        transaction: Transaction,
    },
    /// The exact byte preimage used by Fiber's revocation signature.
    Revocation {
        /// Settlement output committed by the revocation signature.
        #[serde_as(as = "EntityHex")]
        #[schemars(schema_with = "schema_as_hex_bytes")]
        output: CellOutput,
        /// Settlement output data committed by the revocation signature.
        #[serde_as(as = "SliceHex")]
        #[schemars(schema_with = "schema_as_hex_bytes")]
        output_data: Vec<u8>,
        /// Commitment-lock arguments committed by the revocation signature.
        #[serde_as(as = "SliceHex")]
        #[schemars(schema_with = "schema_as_hex_bytes")]
        commitment_lock_script_args: Vec<u8>,
    },
    /// Unsigned fields of a public channel announcement, encoded as molecule bytes.
    ChannelAnnouncement {
        /// Canonical unsigned announcement bytes independently hashed by the signer.
        #[serde_as(as = "SliceHex")]
        #[schemars(schema_with = "schema_as_hex_bytes")]
        unsigned_announcement: Vec<u8>,
    },
}

/// MuSig2 plaintext and session context signed by a channel signer.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Musig2SigningContent {
    /// Domain-separated nonce slot.
    pub slot: NonceSlot,
    /// Counter whose value was used for `slot`; absent for announcements.
    pub commitment_counter: Option<CommitmentCounter>,
    /// Ordered MuSig2 key aggregation context, encoded as `0x`-prefixed hex.
    #[serde_as(as = "SliceHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub key_agg_ctx: Vec<u8>,
    /// Aggregate of both participants' public nonces, encoded as `0x`-prefixed hex.
    #[serde_as(as = "SliceHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub agg_nonce: Vec<u8>,
    /// Plaintext object from which the signer computes the digest.
    pub content: Musig2SignableContent,
}

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub struct ChannelBasePublicKeys {
    /// The public key used to sign commitment transactions, as it appears in the
    /// on-chain 2-of-2 MuSig2 funding output.
    pub funding_pubkey: Pubkey,
    /// The base point used to derive per-commitment TLC public keys.
    pub tlc_base_key: Pubkey,
}

/// Public channel-signer material required to send Fiber's `OpenChannel` message.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChannelOpenSignerMaterial {
    /// Static funding and TLC public keys for this channel.
    pub base_public_keys: ChannelBasePublicKeys,
    /// Per-commitment point for commitment number 1.
    pub first_commitment_point: Pubkey,
    /// Per-commitment point for commitment number 2.
    pub second_commitment_point: Pubkey,
    /// Commitment public nonce at the initial local commitment number, encoded as `0x`-prefixed hex.
    #[serde_as(as = "SliceHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub commitment_nonce: Vec<u8>,
    /// Commitment public nonce published in `TxComplete`, encoded as `0x`-prefixed hex.
    #[serde_as(as = "SliceHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub next_commitment_nonce: Vec<u8>,
    /// Revocation public nonce published with `OpenChannel`, encoded as `0x`-prefixed hex.
    #[serde_as(as = "SliceHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub revocation_nonce: Vec<u8>,
    /// Channel-announcement public nonce; required for public channels and forbidden for private ones.
    #[serde_as(as = "Option<SliceHex>")]
    #[schemars(schema_with = "schema_as_hex_bytes_optional")]
    pub channel_announcement_nonce: Option<Vec<u8>>,
}

/// Follow-up public signer material submitted together with a channel signature.
#[serde_as]
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct NextChannelSignerMaterial {
    /// Next local per-commitment point the node will need.
    pub next_commitment_point: Option<Pubkey>,
    /// Next commitment public nonce, encoded as `0x`-prefixed hex.
    #[serde_as(as = "Option<SliceHex>")]
    #[schemars(schema_with = "schema_as_hex_bytes_optional")]
    pub next_commitment_nonce: Option<Vec<u8>>,
    /// Next revocation public nonce, encoded as `0x`-prefixed hex.
    #[serde_as(as = "Option<SliceHex>")]
    #[schemars(schema_with = "schema_as_hex_bytes_optional")]
    pub next_revocation_nonce: Option<Vec<u8>>,
}

/// Parameters for submitting an external channel signature.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct SubmitChannelSignatureParams {
    /// The channel that produced the outstanding signature request.
    pub channel_id: Hash256,
    /// Identifier of the outstanding signature request.
    pub request_id: Hash256,
    /// MuSig2 partial signature over the persisted plaintext (32 bytes, `0x`-prefixed hex).
    #[serde_as(as = "SliceHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub partial_signature: [u8; 32],
    /// Optional next-round public commitment point and nonces.
    pub next_material: Option<NextChannelSignerMaterial>,
}

/// Result of submitting an external channel signature.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(tag = "type")]
pub enum SubmitChannelSignatureResult {
    /// The signature was verified and the channel state machine resumed.
    Applied,
    /// The same signature was already applied for this request.
    AlreadyApplied,
}
