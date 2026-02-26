//! Payment-related types.

use crate::gen::fiber as molecule_fiber;
use ckb_types::prelude::{Pack, Unpack};
use molecule::prelude::{Builder, Entity};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use strum::{AsRefStr, EnumString};

// ============================================================
// Payment status
// ============================================================

/// The status of a payment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaymentStatus {
    Created,
    Inflight,
    Success,
    Failed,
}

impl PaymentStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, PaymentStatus::Success | PaymentStatus::Failed)
    }
}

// ============================================================
// Payment custom records
// ============================================================

/// 0 ~ 65535 is reserved for endpoint usage, index above 65535 is reserved for internal usage
pub const USER_CUSTOM_RECORDS_MAX_INDEX: u32 = 65535;

/// The custom records to be included in the payment.
/// The key is hex encoded of `u32`, and the value is hex encoded of `Vec<u8>` with `0x` as prefix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct PaymentCustomRecords {
    /// The custom records to be included in the payment.
    pub data: HashMap<u32, Vec<u8>>,
}

// ============================================================
// Conversions with molecule types
// ============================================================

impl From<PaymentCustomRecords> for molecule_fiber::CustomRecords {
    fn from(custom_records: PaymentCustomRecords) -> Self {
        molecule_fiber::CustomRecords::new_builder()
            .data(
                custom_records
                    .data
                    .into_iter()
                    .map(|(key, val)| {
                        molecule_fiber::CustomRecordDataPairBuilder::default()
                            .key(key.pack())
                            .value(val.pack())
                            .build()
                    })
                    .collect(),
            )
            .build()
    }
}

impl From<molecule_fiber::CustomRecords> for PaymentCustomRecords {
    fn from(custom_records: molecule_fiber::CustomRecords) -> Self {
        PaymentCustomRecords {
            data: custom_records
                .data()
                .into_iter()
                .map(|pair| (pair.key().unpack(), pair.value().unpack()))
                .collect(),
        }
    }
}

// ============================================================
// AttemptStatus
// ============================================================

/// The status of a payment attempt.
///
/// State transitions:
/// ```text
///     Created -> Inflight -> Success
///                         -> Failed -> Retrying -> Inflight
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptStatus {
    /// Initial status, a payment attempt is created, no HTLC is sent.
    Created,
    /// The first hop AddTlc is sent successfully and waiting for the response.
    Inflight,
    /// The attempt is retrying after failed.
    Retrying,
    /// Related HTLC is successfully settled.
    Success,
    /// Related HTLC is failed.
    Failed,
}

impl AttemptStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, AttemptStatus::Success | AttemptStatus::Failed)
    }
}

// ============================================================
// TimedResult
// ============================================================

/// A result that tracks timing information for success and failure.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedResult {
    pub fail_time: u64,
    pub fail_amount: u128,
    pub success_time: u64,
    pub success_amount: u128,
}

// ============================================================
// TlcErrPacket
// ============================================================

/// An encrypted error packet for TLC failures.
/// The sender should decode it and then decide what to do with the error.
/// Note: this is supposed to be only accessible by the sender, and it's not reliable since it
/// is not placed on-chain due to the possibility of hop failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TlcErrPacket {
    pub onion_packet: Vec<u8>,
}

/// Shared secret indicating no encryption (for origin node).
pub const NO_SHARED_SECRET: [u8; 32] = [0u8; 32];
const NO_ERROR_PACKET_HMAC: [u8; 32] = [0u8; 32];

use fiber_sphinx::OnionErrorPacket;

impl TlcErrPacket {
    /// Create a new TlcErrPacket from raw payload bytes.
    /// Erring node creates the error packet using the shared secret used in forwarding onion packet.
    /// Use all zeros (NO_SHARED_SECRET) for the origin node.
    pub fn from_payload(payload: Vec<u8>, shared_secret: &[u8; 32]) -> Self {
        let onion_packet = if shared_secret != &NO_SHARED_SECRET {
            OnionErrorPacket::create(shared_secret, payload)
        } else {
            OnionErrorPacket::concat(NO_ERROR_PACKET_HMAC, payload)
        }
        .into_bytes();
        TlcErrPacket { onion_packet }
    }

    /// Check if this packet is plaintext (not encrypted).
    pub fn is_plaintext(&self) -> bool {
        self.onion_packet.len() >= 32 && self.onion_packet[0..32] == NO_ERROR_PACKET_HMAC
    }

    /// Intermediate node backwards the error to the previous hop using the shared secret
    /// used in forwarding the onion packet.
    pub fn backward(self, shared_secret: &[u8; 32]) -> Self {
        if !self.is_plaintext() {
            let onion_packet = OnionErrorPacket::from_bytes(self.onion_packet)
                .xor_cipher_stream(shared_secret)
                .into_bytes();
            TlcErrPacket { onion_packet }
        } else {
            // If it is not encrypted, just send back as it is.
            self
        }
    }
}

impl From<TlcErrPacket> for molecule_fiber::TlcErrPacket {
    fn from(tlc_err_packet: TlcErrPacket) -> Self {
        molecule_fiber::TlcErrPacket::new_builder()
            .onion_packet(tlc_err_packet.onion_packet.pack())
            .build()
    }
}

impl From<molecule_fiber::TlcErrPacket> for TlcErrPacket {
    fn from(tlc_err_packet: molecule_fiber::TlcErrPacket) -> Self {
        TlcErrPacket {
            onion_packet: tlc_err_packet.onion_packet().unpack(),
        }
    }
}

impl std::fmt::Display for TlcErrPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TlcErrPacket")
    }
}

// ============================================================
// PaymentOnionPacket
// ============================================================

/// An encrypted onion packet for payment routing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaymentOnionPacket {
    /// The encrypted packet data.
    data: Vec<u8>,
}

impl PaymentOnionPacket {
    /// Create a new PaymentOnionPacket from raw data.
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// Get the raw packet data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get the raw packet data as bytes slice (alias for `data()`).
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consume self and return the raw data.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Consume self and return the raw data as bytes (alias for `into_data()`).
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

// ============================================================
// RemoveTlcFulfill
// ============================================================

use crate::Hash256;

/// The fulfillment of a TLC removal.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct RemoveTlcFulfill {
    pub payment_preimage: Hash256,
}

// ============================================================
// SessionRouteNode
// ============================================================

use crate::{EntityHex, Pubkey, U128Hex};
use ckb_types::packed::OutPoint;
use serde_with::serde_as;

/// The node and channel information in a payment route hop.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    /// The public key of the node.
    pub pubkey: Pubkey,
    /// The amount for this hop.
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The channel outpoint for this hop.
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

// ============================================================
// RouterHop
// ============================================================

use crate::U64Hex;

/// A router hop information for a payment.
/// A payment router is an array of RouterHop.
/// A router hop generally implies hop `target` will receive `amount_received` with `channel_outpoint` of channel.
/// Improper hop hint may make payment fail, for example the specified channel does not have enough capacity.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouterHop {
    /// The node that is sending the TLC to the next node.
    pub target: Pubkey,
    /// The channel of this hop used to receive TLC.
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// The amount that the source node will transfer to the target node.
    /// We have already added up all the fees along the path, so this amount can be used directly for the TLC.
    #[serde_as(as = "U128Hex")]
    pub amount_received: u128,
    /// The expiry for the TLC that the source node sends to the target node.
    /// We have already added up all the expiry deltas along the path,
    /// the only thing missing is current time. So the expiry is the current time plus the expiry delta.
    #[serde_as(as = "U64Hex")]
    pub incoming_tlc_expiry: u64,
}

// ============================================================
// HopHint
// ============================================================

/// A hop hint is a hint for a node to use a specific channel,
/// usually used for the last hop to the target node.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HopHint {
    /// The public key of the node.
    pub pubkey: Pubkey,
    /// The outpoint for the channel.
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// The fee rate to use this hop to forward the payment.
    pub fee_rate: u64,
    /// The TLC expiry delta to use this hop to forward the payment.
    pub tlc_expiry_delta: u64,
}

// ============================================================
// RemoveTlcReason
// ============================================================

use std::fmt::{self, Debug, Formatter};

/// The reason for removing a TLC.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum RemoveTlcReason {
    RemoveTlcFulfill(RemoveTlcFulfill),
    RemoveTlcFail(TlcErrPacket),
}

impl Debug for RemoveTlcReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RemoveTlcReason::RemoveTlcFulfill(_fulfill) => {
                write!(f, "RemoveTlcFulfill")
            }
            RemoveTlcReason::RemoveTlcFail(_fail) => {
                write!(f, "RemoveTlcFail")
            }
        }
    }
}

impl RemoveTlcReason {
    /// Intermediate node backwards the error to the previous hop using the shared secret
    /// used in forwarding the onion packet.
    pub fn backward(self, shared_secret: &[u8; 32]) -> Self {
        match self {
            RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill) => {
                RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill)
            }
            RemoveTlcReason::RemoveTlcFail(remove_tlc_fail) => {
                RemoveTlcReason::RemoveTlcFail(remove_tlc_fail.backward(shared_secret))
            }
        }
    }
}

impl From<RemoveTlcReason> for molecule_fiber::RemoveTlcReasonUnion {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        match remove_tlc_reason {
            RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill) => {
                molecule_fiber::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill.into())
            }
            RemoveTlcReason::RemoveTlcFail(remove_tlc_fail) => {
                molecule_fiber::RemoveTlcReasonUnion::TlcErrPacket(remove_tlc_fail.into())
            }
        }
    }
}

impl From<RemoveTlcReason> for molecule_fiber::RemoveTlcReason {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        molecule_fiber::RemoveTlcReason::new_builder()
            .set(remove_tlc_reason)
            .build()
    }
}

impl From<molecule_fiber::RemoveTlcReason> for RemoveTlcReason {
    fn from(remove_tlc_reason: molecule_fiber::RemoveTlcReason) -> Self {
        match remove_tlc_reason.to_enum() {
            molecule_fiber::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill) => {
                RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill.into())
            }
            molecule_fiber::RemoveTlcReasonUnion::TlcErrPacket(remove_tlc_fail) => {
                RemoveTlcReason::RemoveTlcFail(remove_tlc_fail.into())
            }
        }
    }
}

// ============================================================
// SessionRoute
// ============================================================

/// The router is a list of nodes that the payment will go through.
/// We store in the payment session and then will use it to track the payment history.
/// For example:
///    `A(amount, channel) -> B -> C -> D`
/// means A will send `amount` with `channel` to B.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    /// The nodes in the route.
    pub nodes: Vec<SessionRouteNode>,
}

// ============================================================
// PaymentHopData
// ============================================================

use crate::invoice::HashAlgorithm;

/// The data for a single hop in a payment route.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaymentHopData {
    /// The amount of the tlc, <= total amount.
    pub amount: u128,
    pub expiry: u64,
    pub payment_preimage: Option<Hash256>,
    pub hash_algorithm: HashAlgorithm,
    pub funding_tx_hash: Hash256,
    pub next_hop: Option<Pubkey>,
    pub custom_records: Option<PaymentCustomRecords>,
}

impl PaymentHopData {
    pub fn next_hop(&self) -> Option<Pubkey> {
        self.next_hop
    }
}

// ============================================================
// Attempt
// ============================================================

/// A payment attempt.
#[derive(Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: u64,
    pub try_limit: u32,
    pub tried_times: u32,
    pub hash: Hash256,
    pub status: AttemptStatus,
    pub payment_hash: Hash256,
    pub route: SessionRoute,
    pub route_hops: Vec<PaymentHopData>,
    pub session_key: [u8; 32],
    pub preimage: Option<Hash256>,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub last_error: Option<String>,
}

impl std::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempt")
            .field("id", &self.id)
            .field("try_limit", &self.try_limit)
            .field("tried_times", &self.tried_times)
            .field("hash", &self.hash)
            .field("status", &self.status)
            .field("payment_hash", &self.payment_hash)
            .field("route", &self.route)
            .field("route_hops", &self.route_hops)
            .field("session_key", &"[REDACTED]")
            .field("preimage", &self.preimage.as_ref().map(|_| "[REDACTED]"))
            .field("created_at", &self.created_at)
            .field("last_updated_at", &self.last_updated_at)
            .field("last_error", &self.last_error)
            .finish()
    }
}

// ============================================================
// TrampolineContext
// ============================================================

use crate::channel::PrevTlcInfo;

/// Context for trampoline routing payments.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TrampolineContext {
    /// Optional final trampoline onion packet.
    ///
    /// When provided, this onion packet will be attached to the payload of the next hop
    /// (which in this context is the next trampoline node).
    pub remaining_trampoline_onion: Vec<u8>,
    /// Previous TLCs information for the payment session.
    /// This is used to associate the outgoing payment with the incoming payment.
    pub previous_tlcs: Vec<PrevTlcInfo>,
    /// Hash algorithm used for the payment.
    pub hash_algorithm: HashAlgorithm,
}

// ============================================================
// TlcErrorCode
// ============================================================

// The onion packet is invalid
const BADONION: u16 = 0x8000;
// Permanent errors (otherwise transient)
const PERM: u16 = 0x4000;
// Node related errors (otherwise channels)
const NODE: u16 = 0x2000;
// Channel forwarding parameter was violated
const UPDATE: u16 = 0x1000;

/// Error codes for TLC (Time-Locked Contract) failures.
#[repr(u16)]
#[derive(
    Debug,
    Copy,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    AsRefStr,
    EnumString,
    TryFromPrimitive,
    IntoPrimitive,
)]
pub enum TlcErrorCode {
    TemporaryNodeFailure = NODE | 2,
    PermanentNodeFailure = PERM | NODE | 2,
    RequiredNodeFeatureMissing = PERM | NODE | 3,
    InvalidOnionVersion = BADONION | PERM | 4,
    InvalidOnionHmac = BADONION | PERM | 5,
    InvalidOnionKey = BADONION | PERM | 6,
    TemporaryChannelFailure = UPDATE | 7,
    PermanentChannelFailure = PERM | 8,
    RequiredChannelFeatureMissing = PERM | 9,
    UnknownNextPeer = PERM | 10,
    AmountBelowMinimum = UPDATE | 11,
    FeeInsufficient = UPDATE | 12,
    IncorrectTlcExpiry = UPDATE | 13,
    ExpiryTooSoon = PERM | 14,
    IncorrectOrUnknownPaymentDetails = PERM | 15,
    InvoiceExpired = PERM | 16,
    InvoiceCancelled = PERM | 17,
    FinalIncorrectExpiryDelta = 18,
    FinalIncorrectTlcAmount = 19,
    ChannelDisabled = UPDATE | 20,
    ExpiryTooFar = PERM | 21,
    InvalidOnionPayload = PERM | 22,
    HoldTlcTimeout = PERM | 23,
    InvalidOnionError = BADONION | PERM | 25,
    IncorrectTlcDirection = PERM | 26,
}

impl TlcErrorCode {
    pub fn is_node(&self) -> bool {
        *self as u16 & NODE != 0
    }

    pub fn is_bad_onion(&self) -> bool {
        *self as u16 & BADONION != 0
    }

    pub fn is_perm(&self) -> bool {
        *self as u16 & PERM != 0
    }

    pub fn is_update(&self) -> bool {
        *self as u16 & UPDATE != 0
    }
}

// ============================================================
// SendPaymentData
// ============================================================

use ckb_types::packed::Script;

/// Data for sending a payment.
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
    /// This flag indicates the invoice whether to allow multi-path payment.
    pub allow_mpp: bool,
    pub dry_run: bool,
    /// Optional explicit trampoline hops.
    ///
    /// When set to a non-empty list `[t1, t2, ...]`, routing will only find a path from the
    /// payer to `t1`, and the inner trampoline onion will encode `t1 -> t2 -> ... -> final`.
    pub trampoline_hops: Option<Vec<Pubkey>>,
    #[serde(default)]
    pub trampoline_context: Option<TrampolineContext>,
}

// ============================================================
// PaymentSession
// ============================================================

/// A payment session tracking the state of a payment attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentSession {
    pub request: SendPaymentData,
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_code: Option<TlcErrorCode>,
    /// For non-MPP, this is the maximum number of single attempt retry limit.
    /// For MPP, this is the sum limit of all parts' retry times.
    pub try_limit: u32,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
}

// ============================================================
// Molecule conversions
// ============================================================

impl From<RemoveTlcFulfill> for molecule_fiber::RemoveTlcFulfill {
    fn from(remove_tlc_fulfill: RemoveTlcFulfill) -> Self {
        molecule_fiber::RemoveTlcFulfill::new_builder()
            .payment_preimage(remove_tlc_fulfill.payment_preimage.into())
            .build()
    }
}

impl From<molecule_fiber::RemoveTlcFulfill> for RemoveTlcFulfill {
    fn from(remove_tlc_fulfill: molecule_fiber::RemoveTlcFulfill) -> Self {
        RemoveTlcFulfill {
            payment_preimage: remove_tlc_fulfill.payment_preimage().into(),
        }
    }
}
