//! Channel management types for the Fiber Network JSON-RPC API.

use crate::serde_utils::{EntityHex, Hash256, Pubkey, U128Hex, U64Hex};
use ckb_jsonrpc_types::{EpochNumberWithFraction, Script};
use ckb_types::packed::OutPoint;
use ckb_types::H256;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Convert SCREAMING_SNAKE_CASE to PascalCase.
/// Example: "OUR_INIT_SENT" -> "OurInitSent"
fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
            }
        })
        .collect()
}

/// Convert PascalCase to SCREAMING_SNAKE_CASE.
/// Example: "OurInitSent" -> "OUR_INIT_SENT"
fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(c.to_ascii_uppercase());
    }
    result
}

/// Macro to define flags types that serialize to PascalCase strings.
/// For single flag, returns the flag name in PascalCase (e.g., "OurInitSent").
/// For multiple flags, returns comma-separated names (e.g., "OurInitSent,TheirInitSent").
macro_rules! define_flags {
    (
        $(#[$struct_meta:meta])*
        pub struct $name:ident($ty:ty) {
            $($(#[$flag_meta:meta])* const $flag_name:ident = $flag_value:expr;)*
        }
    ) => {
        $(#[$struct_meta])*
        pub struct $name(pub $ty);

        impl $name {
            $(pub const $flag_name: $ty = $flag_value;)*

            #[allow(clippy::wrong_self_convention)]
            fn to_strings(self) -> Vec<String> {
                let mut names = Vec::new();
                $(
                    if self.0 & Self::$flag_name != 0 {
                        names.push(to_pascal_case(stringify!($flag_name)));
                    }
                )*
                names
            }

            fn from_string(s: &str) -> Option<Self> {
                let mut flags: $ty = 0;
                for name in s.split(',') {
                    let name = name.trim();
                    let snake_name = to_snake_case(name);
                    match snake_name.as_str() {
                        $(stringify!($flag_name) => flags |= Self::$flag_name,)*
                        _ => return None,
                    }
                }
                Some($name(flags))
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let names = self.clone().to_strings();
                if names.is_empty() {
                    serializer.serialize_str("")
                } else {
                    serializer.serialize_str(&names.join(","))
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                if s.is_empty() {
                    Ok($name(0))
                } else {
                    $name::from_string(&s)
                        .ok_or_else(|| serde::de::Error::custom(format!("Invalid {}: {}", stringify!($name), s)))
                }
            }
        }

        impl From<$ty> for $name {
            fn from(v: $ty) -> Self {
                $name(v)
            }
        }

        impl From<$name> for $ty {
            fn from(v: $name) -> Self {
                v.0
            }
        }
    };
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct ChannelFlags(u8) {
        const PUBLIC = 1;
        const ONE_WAY = 1 << 1;
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct ChannelUpdateChannelFlags(u32) {
        const DISABLED = 1;
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct ChannelUpdateMessageFlags(u32) {
        const UPDATE_OF_NODE1 = 0;
        const UPDATE_OF_NODE2 = 1;
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct NegotiatingFundingFlags(u32) {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = 1 | (1 << 1);
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct CollaboratingFundingTxFlags(u32) {
        const AWAITING_REMOTE_TX_COLLABORATION_MSG = 1;
        const PREPARING_LOCAL_TX_COLLABORATION_MSG = 1 << 1;
        const OUR_TX_COMPLETE_SENT = 1 << 2;
        const THEIR_TX_COMPLETE_SENT = 1 << 3;
        const COLLABORATION_COMPLETED = (1 << 2) | (1 << 3);
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct SigningCommitmentFlags(u32) {
        const OUR_COMMITMENT_SIGNED_SENT = 1;
        const THEIR_COMMITMENT_SIGNED_SENT = 1 << 1;
        const COMMITMENT_SIGNED_SENT = 1 | (1 << 1);
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct AwaitingTxSignaturesFlags(u32) {
        const OUR_TX_SIGNATURES_SENT = 1;
        const THEIR_TX_SIGNATURES_SENT = 1 << 1;
        const TX_SIGNATURES_SENT = 1 | (1 << 1);
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct AwaitingChannelReadyFlags(u32) {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = 1 | (1 << 1);
    }
}

define_flags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct ShuttingDownFlags(u32) {
        const OUR_SHUTDOWN_SENT = 1;
        const THEIR_SHUTDOWN_SENT = 1 << 1;
        const AWAITING_PENDING_TLCS = 1 | (1 << 1);
        const DROPPING_PENDING = 1 << 2;
        const WAITING_COMMITMENT_CONFIRMATION = 1 << 3;
    }
}

define_flags! {
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

define_flags! {
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct AppliedFlags(u8) {
        const ADD = 1;
        const REMOVE = 1 << 1;
    }
}

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
///
/// Serialized with adjacently-tagged representation using PascalCase variant names and flags.
/// This is different from the internal `ChannelState` in fiber-types which uses
/// default serde for bincode compatibility.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state_name", content = "state_flags")]
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
    /// The outpoint of the channel
    #[serde_as(as = "Option<EntityHex>")]
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

/// The status of a tlc.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum TlcStatus {
    /// Outbound tlc
    Outbound(OutboundTlcStatus),
    /// Inbound tlc
    Inbound(InboundTlcStatus),
}

/// The status of an outbound tlc.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
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
