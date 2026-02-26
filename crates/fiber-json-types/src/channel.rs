//! Channel-related types: state flags, TLC status, channel state enum.

use crate::serde_utils::{EntityHex, U128Hex, U64Hex};
use crate::Hash256;

use ckb_jsonrpc_types::{EpochNumberWithFraction, Script};
use ckb_types::packed::OutPoint;
use ckb_types::H256;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::secio::PeerId;

pub use fiber_types::channel::{
    AppliedFlags, AwaitingChannelReadyFlags, AwaitingTxSignaturesFlags, ChannelFlags,
    ChannelUpdateChannelFlags, ChannelUpdateMessageFlags, CloseFlags, CollaboratingFundingTxFlags,
    InboundTlcStatus, NegotiatingFundingFlags, OutboundTlcStatus, ShuttingDownFlags,
    SigningCommitmentFlags, TLCId, TlcStatus,
};

// ============================================================
// Channel state (RPC version with serde tags)
// ============================================================

/// The state of a channel.
/// This version uses `#[serde(tag = "state_name", content = "state_flags")]` for JSON compatibility.
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
    /// We've successfully negotiated a `closing_signed` dance.
    ShuttingDown(ShuttingDownFlags),
    /// This channel is closed.
    Closed(CloseFlags),
}

// ============================================================
// RPC param/result types
// ============================================================

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct OpenChannelParams {
    /// The peer ID to open a channel with.
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    /// The amount of CKB or UDT to fund the channel with.
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,
    /// Whether this is a public channel.
    pub public: Option<bool>,
    /// Whether this is a one-way channel.
    pub one_way: Option<bool>,
    /// The type script of the UDT to fund the channel with.
    pub funding_udt_type_script: Option<Script>,
    /// The script used to receive the channel balance.
    pub shutdown_script: Option<Script>,
    /// The delay time for the commitment transaction.
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,
    /// The fee rate for the commitment transaction.
    #[serde_as(as = "Option<U64Hex>")]
    pub commitment_fee_rate: Option<u64>,
    /// The fee rate for the funding transaction.
    #[serde_as(as = "Option<U64Hex>")]
    pub funding_fee_rate: Option<u64>,
    /// The expiry delta to forward a TLC, in milliseconds.
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_delta: Option<u64>,
    /// The minimum value for a TLC.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_min_value: Option<u128>,
    /// The fee proportional millionths for a TLC.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,
    /// The maximum value in flight for TLCs.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_tlc_value_in_flight: Option<u128>,
    /// The maximum number of TLCs that can be accepted.
    #[serde_as(as = "Option<U64Hex>")]
    pub max_tlc_number_in_flight: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OpenChannelResult {
    /// The temporary channel ID of the channel being opened
    pub temporary_channel_id: Hash256,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AbandonChannelParams {
    /// The temporary channel ID or real channel ID of the channel being abandoned
    pub channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct AcceptChannelParams {
    /// The temporary channel ID of the channel to accept
    pub temporary_channel_id: Hash256,
    /// The amount of CKB or UDT to fund the channel with
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,
    /// The script used to receive the channel balance.
    pub shutdown_script: Option<Script>,
    /// The max TLC sum value in flight for the channel.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_tlc_value_in_flight: Option<u128>,
    /// The max TLC number in flight send from our side.
    #[serde_as(as = "Option<U64Hex>")]
    pub max_tlc_number_in_flight: Option<u64>,
    /// The minimum value for a TLC.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_min_value: Option<u128>,
    /// The fee proportional millionths for a TLC.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,
    /// The expiry delta to forward a TLC, in milliseconds.
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_delta: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AcceptChannelResult {
    /// The final ID of the channel that was accepted
    pub channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ListChannelsParams {
    /// The peer ID to list channels for.
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub peer_id: Option<PeerId>,
    /// Whether to include closed channels in the list.
    pub include_closed: Option<bool>,
    /// When set to true, only return channels that are still being opened.
    pub only_pending: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ListChannelsResult {
    /// The list of channels
    pub channels: Vec<Channel>,
}

/// The channel data structure
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Channel {
    /// The channel ID
    pub channel_id: Hash256,
    /// Whether the channel is public
    pub is_public: bool,
    /// Is this channel initially inbound?
    pub is_acceptor: bool,
    /// Is this channel one-way?
    pub is_one_way: bool,
    #[serde_as(as = "Option<EntityHex>")]
    /// The outpoint of the channel
    pub channel_outpoint: Option<OutPoint>,
    /// The peer ID of the channel
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
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
    /// The list of pending TLCs
    pub pending_tlcs: Vec<Htlc>,
    /// The hash of the latest commitment transaction
    pub latest_commitment_transaction_hash: Option<H256>,
    /// The time the channel was created at
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Whether the channel is enabled
    pub enabled: bool,
    /// The expiry delta to forward a TLC, in milliseconds
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
    /// The fee proportional millionths for a TLC
    #[serde_as(as = "U128Hex")]
    pub tlc_fee_proportional_millionths: u128,
    /// The hash of the shutdown transaction
    pub shutdown_transaction_hash: Option<H256>,
    /// Human-readable reason why the channel opening failed.
    pub failure_detail: Option<String>,
}

/// The HTLC data structure
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Htlc {
    /// The id of the HTLC
    #[serde_as(as = "U64Hex")]
    pub id: u64,
    /// The amount of the HTLC
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The payment hash of the HTLC
    pub payment_hash: Hash256,
    /// The expiry of the HTLC
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// The forwarding channel ID
    pub forwarding_channel_id: Option<Hash256>,
    /// The forwarding TLC id
    #[serde_as(as = "Option<U64Hex>")]
    pub forwarding_tlc_id: Option<u64>,
    /// The status of the HTLC
    pub status: TlcStatus,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownChannelParams {
    /// The channel ID of the channel to shut down
    pub channel_id: Hash256,
    /// The script used to receive the channel balance
    pub close_script: Option<Script>,
    /// The fee rate for the closing transaction
    #[serde_as(as = "Option<U64Hex>")]
    pub fee_rate: Option<u64>,
    /// Whether to force the channel to close
    pub force: Option<bool>,
}

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
