//! Payment-related types.

use crate::gen::fiber as molecule_fiber;
use crate::Hash256;
use crate::{EntityHex, Pubkey, SliceHex};
use ckb_types::prelude::{Pack, Unpack};
use molecule::prelude::{Builder, Byte, Entity};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;
use strum::{AsRefStr, EnumString};

// ============================================================
// Payment status
// ============================================================

/// The status of a payment, will update as the payment progresses.
/// The transfer path for payment status is `Created -> Inflight -> Success | Failed`.
///
/// **MPP Behavior**: A single session may involve multiple attempts (HTLCs) to fulfill the total amount.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaymentStatus {
    /// Initial status. A payment session is created, but no HTLC has been dispatched.
    Created,
    /// The first hop AddTlc is sent successfully and waiting for the response.
    ///
    /// > **MPP Logic**: Status `Inflight` means at least one attempt is still not in `Success`, payment needs more retrying or waiting for HTLC settlement.
    Inflight,
    /// The payment is finished. All related HTLCs are successfully settled,
    /// and the aggregate amount equals the total requested amount.
    Success,
    /// The payment session has terminated. HTLCs have failed and the target
    /// amount cannot be fulfilled after exhausting all retries.
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
// BasicMppPaymentData
// ============================================================

/// Bolt04 basic MPP payment data record
#[derive(Eq, PartialEq, Debug)]
pub struct BasicMppPaymentData {
    pub payment_secret: crate::Hash256,
    pub total_amount: u128,
}

impl BasicMppPaymentData {
    // record type for payment data record in bolt04
    // custom records key from 65536 is reserved for internal usage
    pub const CUSTOM_RECORD_KEY: u32 = USER_CUSTOM_RECORDS_MAX_INDEX + 1;

    pub fn new(payment_secret: crate::Hash256, total_amount: u128) -> Self {
        Self {
            payment_secret,
            total_amount,
        }
    }

    fn to_vec(&self) -> Vec<u8> {
        let mut vec = Vec::new();
        vec.extend_from_slice(self.payment_secret.as_ref());
        vec.extend_from_slice(&self.total_amount.to_le_bytes());
        vec
    }

    pub fn write(&self, custom_records: &mut PaymentCustomRecords) {
        custom_records
            .data
            .insert(Self::CUSTOM_RECORD_KEY, self.to_vec());
    }

    pub fn read(custom_records: &PaymentCustomRecords) -> Option<Self> {
        custom_records
            .data
            .get(&Self::CUSTOM_RECORD_KEY)
            .and_then(|data| {
                if data.len() != 32 + 16 {
                    return None;
                }
                let secret: [u8; 32] = data[..32].try_into().unwrap();
                let payment_secret = crate::Hash256::from(secret);
                let total_amount = u128::from_le_bytes(data[32..].try_into().unwrap());
                Some(Self::new(payment_secret, total_amount))
            })
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
// TlcErr / TlcErrData
// ============================================================

use crate::protocol::ChannelUpdate;
use ckb_types::packed::OutPoint;
use std::fmt::Display;

/// Extra data attached to a TLC error.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TlcErrData {
    ChannelFailed {
        #[serde_as(as = "EntityHex")]
        channel_outpoint: OutPoint,
        channel_update: Option<ChannelUpdate>,
        node_id: Pubkey,
    },
    NodeFailed {
        node_id: Pubkey,
    },
    TrampolineFailed {
        node_id: Pubkey,
        #[serde_as(as = "SliceHex")]
        inner_error_packet: Vec<u8>,
    },
}

/// Structured TLC error with error code and optional extra data.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TlcErr {
    pub error_code: TlcErrorCode,
    pub extra_data: Option<TlcErrData>,
}

impl Display for TlcErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.extra_data {
            Some(TlcErrData::TrampolineFailed {
                node_id,
                inner_error_packet,
            }) => write!(
                f,
                "{} (TrampolineFailed node_id={:?} inner_error_packet_len={})",
                self.error_code_as_str(),
                node_id,
                inner_error_packet.len()
            ),
            _ => write!(f, "{}", self.error_code_as_str()),
        }
    }
}

impl TlcErr {
    pub fn new(error_code: TlcErrorCode) -> Self {
        TlcErr {
            error_code,
            extra_data: None,
        }
    }

    pub fn new_node_fail(error_code: TlcErrorCode, node_id: Pubkey) -> Self {
        TlcErr {
            error_code,
            extra_data: Some(TlcErrData::NodeFailed { node_id }),
        }
    }

    pub fn new_channel_fail(
        error_code: TlcErrorCode,
        node_id: Pubkey,
        channel_outpoint: OutPoint,
        channel_update: Option<ChannelUpdate>,
    ) -> Self {
        TlcErr {
            error_code,
            extra_data: Some(TlcErrData::ChannelFailed {
                node_id,
                channel_outpoint,
                channel_update,
            }),
        }
    }

    pub fn error_node_id(&self) -> Option<Pubkey> {
        match &self.extra_data {
            Some(TlcErrData::NodeFailed { node_id }) => Some(*node_id),
            Some(TlcErrData::ChannelFailed { node_id, .. }) => Some(*node_id),
            Some(TlcErrData::TrampolineFailed { node_id, .. }) => Some(*node_id),
            _ => None,
        }
    }

    pub fn error_channel_outpoint(&self) -> Option<OutPoint> {
        match &self.extra_data {
            Some(TlcErrData::ChannelFailed {
                channel_outpoint, ..
            }) => Some(channel_outpoint.clone()),
            _ => None,
        }
    }

    pub fn error_code(&self) -> TlcErrorCode {
        self.error_code
    }

    pub fn error_code_as_str(&self) -> String {
        let error_code: TlcErrorCode = self.error_code;
        error_code.as_ref().to_string()
    }

    pub fn error_code_as_u16(&self) -> u16 {
        self.error_code.into()
    }

    pub fn set_extra_data(&mut self, extra_data: TlcErrData) {
        self.extra_data = Some(extra_data);
    }

    pub fn serialize(&self) -> Vec<u8> {
        molecule_fiber::TlcErr::try_from(self.clone())
            .expect("TlcErr serialization should not fail for valid TlcErr")
            .as_slice()
            .to_vec()
    }

    pub fn deserialize(data: &[u8]) -> Option<Self> {
        molecule_fiber::TlcErr::from_slice(data)
            .ok()
            .and_then(|e| TlcErr::try_from(e).ok())
    }
}

impl TryFrom<TlcErrData> for molecule_fiber::TlcErrData {
    type Error = anyhow::Error;

    fn try_from(tlc_err_data: TlcErrData) -> Result<Self, Self::Error> {
        match tlc_err_data {
            TlcErrData::ChannelFailed {
                channel_outpoint,
                channel_update,
                node_id,
            } => Ok(molecule_fiber::ChannelFailed::new_builder()
                .channel_outpoint(channel_outpoint)
                .channel_update(
                    molecule_fiber::ChannelUpdateOpt::new_builder()
                        .set(channel_update.map(|x| x.into()))
                        .build(),
                )
                .node_id(node_id.into())
                .build()
                .into()),
            TlcErrData::NodeFailed { node_id } => Ok(molecule_fiber::NodeFailed::new_builder()
                .node_id(node_id.into())
                .build()
                .into()),
            TlcErrData::TrampolineFailed {
                node_id,
                inner_error_packet,
            } => Ok(molecule_fiber::TrampolineFailed::new_builder()
                .node_id(node_id.into())
                .inner_error_packet(inner_error_packet.pack())
                .build()
                .into()),
        }
    }
}

impl TryFrom<molecule_fiber::TlcErrData> for TlcErrData {
    type Error = anyhow::Error;

    fn try_from(tlc_err_data: molecule_fiber::TlcErrData) -> Result<Self, Self::Error> {
        match tlc_err_data.to_enum() {
            molecule_fiber::TlcErrDataUnion::ChannelFailed(channel_failed) => {
                Ok(TlcErrData::ChannelFailed {
                    channel_outpoint: channel_failed.channel_outpoint(),
                    channel_update: channel_failed
                        .channel_update()
                        .to_opt()
                        .map(|x| x.try_into())
                        .transpose()?,
                    node_id: channel_failed.node_id().try_into()?,
                })
            }
            molecule_fiber::TlcErrDataUnion::NodeFailed(node_failed) => {
                Ok(TlcErrData::NodeFailed {
                    node_id: node_failed.node_id().try_into()?,
                })
            }
            molecule_fiber::TlcErrDataUnion::TrampolineFailed(trampoline_failed) => {
                Ok(TlcErrData::TrampolineFailed {
                    node_id: trampoline_failed.node_id().try_into()?,
                    inner_error_packet: trampoline_failed.inner_error_packet().unpack(),
                })
            }
        }
    }
}

impl TryFrom<TlcErr> for molecule_fiber::TlcErr {
    type Error = anyhow::Error;

    fn try_from(tlc_err: TlcErr) -> Result<Self, Self::Error> {
        Ok(molecule_fiber::TlcErr::new_builder()
            .error_code(tlc_err.error_code_as_u16().into())
            .extra_data(
                molecule_fiber::TlcErrDataOpt::new_builder()
                    .set(tlc_err.extra_data.map(|data| data.try_into()).transpose()?)
                    .build(),
            )
            .build())
    }
}

impl TryFrom<molecule_fiber::TlcErr> for TlcErr {
    type Error = anyhow::Error;

    fn try_from(tlc_err: molecule_fiber::TlcErr) -> Result<Self, Self::Error> {
        let code: u16 = tlc_err.error_code().into();
        let error_code = TlcErrorCode::try_from(code)
            .map_err(|_| anyhow::anyhow!("Invalid TLC error code: {}", code))?;
        let extra_data = tlc_err
            .extra_data()
            .to_opt()
            .map(|data| data.try_into())
            .transpose()?;
        Ok(TlcErr {
            error_code,
            extra_data,
        })
    }
}

// ============================================================
// CurrentPaymentHopData
// ============================================================

use crate::invoice::HashAlgorithm;

/// The decrypted hop data for the current hop, without the `next_hop` field.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentPaymentHopData {
    pub amount: u128,
    pub expiry: u64,
    pub payment_preimage: Option<crate::Hash256>,
    pub hash_algorithm: HashAlgorithm,
    pub funding_tx_hash: crate::Hash256,
    pub custom_records: Option<PaymentCustomRecords>,
}

impl From<PaymentHopData> for CurrentPaymentHopData {
    fn from(hop: PaymentHopData) -> Self {
        CurrentPaymentHopData {
            amount: hop.amount,
            expiry: hop.expiry,
            payment_preimage: hop.payment_preimage,
            hash_algorithm: hop.hash_algorithm,
            funding_tx_hash: hop.funding_tx_hash,
            custom_records: hop.custom_records,
        }
    }
}

impl From<CurrentPaymentHopData> for PaymentHopData {
    fn from(hop: CurrentPaymentHopData) -> Self {
        PaymentHopData {
            amount: hop.amount,
            expiry: hop.expiry,
            payment_preimage: hop.payment_preimage,
            hash_algorithm: hop.hash_algorithm,
            funding_tx_hash: hop.funding_tx_hash,
            custom_records: hop.custom_records,
            next_hop: None,
        }
    }
}

impl CurrentPaymentHopData {
    /// Returns the trampoline onion bytes embedded in `custom_records`, if present.
    pub fn trampoline_onion(&self) -> Option<Vec<u8>> {
        self.custom_records
            .as_ref()
            .and_then(TrampolineOnionData::read)
    }

    /// Embeds a trampoline onion packet into `custom_records`.
    pub fn set_trampoline_onion(&mut self, data: Vec<u8>) {
        let cr = self.custom_records.get_or_insert_with(Default::default);
        TrampolineOnionData::write(data, cr);
    }
}

// ============================================================
// SessionRouteNode
// ============================================================

use crate::U128Hex;

/// The node and channel information in a payment route hop
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    /// the public key of the node
    pub pubkey: Pubkey,
    /// the amount for this hop
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// the channel outpoint for this hop
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

// ============================================================
// RouterHop
// ============================================================

use crate::U64Hex;

/// A router hop information for a payment, a paymenter router is an array of RouterHop,
/// a router hop generally implies hop `target` will receive `amount_received` with `channel_outpoint` of channel.
/// Improper hop hint may make payment fail, for example the specified channel do not have enough capacity.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouterHop {
    /// The node that is sending the TLC to the next node.
    pub target: Pubkey,
    /// The channel of this hop used to receive TLC
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
// SessionRoute
// ============================================================

/// The router is a list of nodes that the payment will go through.
/// We store in the payment session and then will use it to track the payment history.
/// The router is a list of nodes that the payment will go through.
/// For example:
///    `A(amount, channel) -> B -> C -> D`
/// means A will send `amount` with `channel` to B.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    /// the nodes in the route
    pub nodes: Vec<SessionRouteNode>,
}

impl SessionRoute {
    // Create a new route from the source to the target with the given payment hops.
    // The payment hops are the hops that the payment will go through.
    // for a payment route A -> B -> C -> D
    // the `payment_hops` is [B, C, D], which is a convenient way for onion routing.
    // here we need to create a session route with source, which is A -> B -> C -> D
    pub fn new(source: Pubkey, target: Pubkey, payment_hops: &[PaymentHopData]) -> Self {
        let nodes = std::iter::once(source)
            .chain(
                payment_hops
                    .iter()
                    .map(|hop| hop.next_hop.unwrap_or(target)),
            )
            .zip(payment_hops)
            .map(|(pubkey, hop)| SessionRouteNode {
                pubkey,
                channel_outpoint: OutPoint::new(
                    if hop.funding_tx_hash != Hash256::default() {
                        hop.funding_tx_hash.into()
                    } else {
                        Hash256::default().into()
                    },
                    0,
                ),
                amount: hop.amount,
            })
            .collect();
        Self { nodes }
    }

    pub fn receiver_amount(&self) -> u128 {
        self.nodes.last().map_or(0, |s| s.amount)
    }

    pub fn fee(&self) -> u128 {
        let first_amount = self.nodes.first().map_or(0, |s| s.amount);
        let last_amount = self.receiver_amount();
        debug_assert!(first_amount >= last_amount);
        first_amount - last_amount
    }

    pub fn channel_outpoints(&self) -> impl Iterator<Item = (Pubkey, &OutPoint, u128)> {
        self.nodes
            .iter()
            .map(|x| (x.pubkey, &x.channel_outpoint, x.amount))
    }
}

// ============================================================
// PaymentHopData
// ============================================================

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

// TrampolineOnionData is in crate::onion.
use crate::onion::TrampolineOnionData;

impl PaymentHopData {
    pub fn next_hop(&self) -> Option<Pubkey> {
        self.next_hop
    }

    /// Returns the trampoline onion bytes embedded in `custom_records`, if present.
    pub fn trampoline_onion(&self) -> Option<Vec<u8>> {
        self.custom_records
            .as_ref()
            .and_then(TrampolineOnionData::read)
    }

    /// Embeds a trampoline onion packet into `custom_records`.
    pub fn set_trampoline_onion(&mut self, data: Vec<u8>) {
        let cr = self.custom_records.get_or_insert_with(Default::default);
        TrampolineOnionData::write(data, cr);
    }

    pub fn serialize(&self) -> Vec<u8> {
        molecule_fiber::PaymentHopData::from(self.clone())
            .as_bytes()
            .to_vec()
    }

    pub fn deserialize(data: &[u8]) -> Option<Self> {
        molecule_fiber::PaymentHopData::from_slice(data)
            .ok()
            .map(|x| x.into())
    }
}

impl From<PaymentHopData> for molecule_fiber::PaymentHopData {
    fn from(payment_hop_data: PaymentHopData) -> Self {
        molecule_fiber::PaymentHopData::new_builder()
            .amount(payment_hop_data.amount.pack())
            .expiry(payment_hop_data.expiry.pack())
            .payment_preimage(
                molecule_fiber::PaymentPreimageOpt::new_builder()
                    .set(payment_hop_data.payment_preimage.map(|x| x.into()))
                    .build(),
            )
            .hash_algorithm(Byte::new(payment_hop_data.hash_algorithm as u8))
            .funding_tx_hash(payment_hop_data.funding_tx_hash.into())
            .next_hop(
                molecule_fiber::PubkeyOpt::new_builder()
                    .set(payment_hop_data.next_hop.map(|x| x.into()))
                    .build(),
            )
            .custom_records(
                molecule_fiber::CustomRecordsOpt::new_builder()
                    .set(payment_hop_data.custom_records.map(|x| x.into()))
                    .build(),
            )
            .build()
    }
}

impl From<molecule_fiber::PaymentHopData> for PaymentHopData {
    fn from(payment_hop_data: molecule_fiber::PaymentHopData) -> Self {
        let custom_records: Option<PaymentCustomRecords> =
            payment_hop_data.custom_records().to_opt().map(|x| x.into());

        PaymentHopData {
            amount: payment_hop_data.amount().unpack(),
            expiry: payment_hop_data.expiry().unpack(),
            payment_preimage: payment_hop_data
                .payment_preimage()
                .to_opt()
                .map(|x| x.into()),
            hash_algorithm: payment_hop_data
                .hash_algorithm()
                .try_into()
                .unwrap_or_default(),
            funding_tx_hash: payment_hop_data.funding_tx_hash().into(),
            next_hop: payment_hop_data
                .next_hop()
                .to_opt()
                .map(|x| x.try_into())
                .and_then(Result::ok),
            custom_records,
        }
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

impl Attempt {
    pub fn set_inflight_status(&mut self) {
        self.status = AttemptStatus::Inflight;
        self.last_error = None;
    }

    pub fn set_success_status(&mut self) {
        self.status = AttemptStatus::Success;
        self.last_error = None;
    }

    pub fn set_failed_status(&mut self, error: &str, retryable: bool) {
        self.last_error = Some(error.to_string());
        self.last_updated_at = crate::now_timestamp_as_millis_u64();
        if !retryable || self.tried_times > self.try_limit {
            self.status = AttemptStatus::Failed;
        } else {
            self.status = AttemptStatus::Retrying;
            self.tried_times += 1;
        }
    }

    pub fn update_route(&mut self, new_route_hops: Vec<PaymentHopData>) {
        self.route_hops = new_route_hops;
        let sender = self.route.nodes[0].pubkey;
        let receiver = self.route.nodes.last().unwrap().pubkey;
        self.route = SessionRoute::new(sender, receiver, &self.route_hops);
    }

    pub fn is_success(&self) -> bool {
        self.status == AttemptStatus::Success
    }

    pub fn is_inflight(&self) -> bool {
        self.status == AttemptStatus::Inflight
    }

    pub fn is_failed(&self) -> bool {
        self.status == AttemptStatus::Failed
    }

    pub fn is_active(&self) -> bool {
        self.status != AttemptStatus::Failed
    }

    pub fn is_retrying(&self) -> bool {
        self.status == AttemptStatus::Retrying
    }

    pub fn first_hop_channel_outpoint_eq(&self, out_point: &OutPoint) -> bool {
        self.first_hop_channel_outpoint()
            .map(|x| x.eq(out_point))
            .unwrap_or_default()
    }

    pub fn first_hop_channel_outpoint(&self) -> Option<&OutPoint> {
        self.route.nodes.first().map(|x| &x.channel_outpoint)
    }

    pub fn channel_outpoints(&self) -> impl Iterator<Item = (Pubkey, &OutPoint, u128)> {
        self.route.channel_outpoints()
    }

    pub fn hops_public_keys(&self) -> Vec<Pubkey> {
        // Skip the first node, which is the sender.
        self.route.nodes.iter().skip(1).map(|x| x.pubkey).collect()
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
    /// Runtime cache for attempts; not persisted.
    #[serde(skip)]
    pub cached_attempts: Vec<Attempt>,
}

// ============================================================
// Payment constants
// ============================================================

/// Default maximum number of parts for a multi-part payment.
pub const DEFAULT_MAX_PARTS: u64 = 12;

/// Default retry limit per attempt in MPP mode.
pub const DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT: u32 = 3;

// ============================================================
// SendPaymentData methods
// ============================================================
impl SendPaymentData {
    /// Maximum number of parallel parts for this payment.
    pub fn max_parts(&self) -> usize {
        self.max_parts.unwrap_or(DEFAULT_MAX_PARTS) as usize
    }

    /// Whether this payment allows multi-path payment (MPP).
    pub fn allow_mpp(&self) -> bool {
        // only allow mpp if max_parts is greater than 1 and not keysend
        self.allow_mpp && self.max_parts() > 1 && !self.keysend
    }

    /// Whether this payment uses trampoline routing.
    pub fn use_trampoline_routing(&self) -> bool {
        self.trampoline_hops
            .as_ref()
            .is_some_and(|hops| !hops.is_empty())
    }
}

// ============================================================
// PaymentSession methods
// ============================================================
impl PaymentSession {
    pub fn update_with_attempt(&mut self, attempt: Attempt) {
        if let Some(a) = self.cached_attempts.iter_mut().find(|a| a.id == attempt.id) {
            *a = attempt;
        }
        self.status = self.calc_payment_status();
    }

    pub fn retry_times(&self) -> u32 {
        self.attempts().map(|a| a.tried_times).sum()
    }

    pub fn allow_mpp(&self) -> bool {
        self.request.allow_mpp()
    }

    pub fn payment_hash(&self) -> crate::Hash256 {
        self.request.payment_hash
    }

    pub fn is_payment_with_router(&self) -> bool {
        !self.request.router.is_empty()
    }

    pub fn is_dry_run(&self) -> bool {
        self.request.dry_run
    }

    pub fn attempts(&self) -> std::slice::Iter<'_, Attempt> {
        self.cached_attempts.iter()
    }

    #[cfg(test)]
    pub fn all_attempts_with_status(&self) -> Vec<(u64, AttemptStatus, Option<String>, u32, u128)> {
        self.cached_attempts
            .iter()
            .map(|a| {
                (
                    a.id,
                    a.status,
                    a.last_error.clone(),
                    a.tried_times,
                    a.route.receiver_amount(),
                )
            })
            .collect()
    }

    pub fn attempts_count(&self) -> usize {
        self.cached_attempts.len()
    }

    pub fn max_parts(&self) -> usize {
        if self.allow_mpp() && !self.request.use_trampoline_routing() {
            self.request.max_parts()
        } else {
            1
        }
    }

    pub fn active_attempts(&self) -> Vec<&Attempt> {
        self.attempts().filter(|a| a.is_active()).collect()
    }

    pub fn fee_paid(&self) -> u128 {
        if self.request.use_trampoline_routing() {
            let total_sent: u128 = self
                .active_attempts()
                .iter()
                .map(|a| a.route.nodes.first().map_or(0, |n| n.amount))
                .sum();
            let total_received = self.request.amount;
            total_sent.saturating_sub(total_received)
        } else {
            self.active_attempts().iter().map(|a| a.route.fee()).sum()
        }
    }

    pub fn remain_fee_amount(&self) -> Option<u128> {
        let max_fee_amount = self.request.max_fee_amount?;
        let remain_fee = max_fee_amount.saturating_sub(self.fee_paid());
        Some(remain_fee)
    }

    pub fn remain_amount(&self) -> u128 {
        let sent_amount = self
            .active_attempts()
            .iter()
            .map(|a| a.route.receiver_amount())
            .sum::<u128>();
        self.request.amount.saturating_sub(sent_amount)
    }

    pub fn new_attempt(
        &self,
        attempt_id: u64,
        source: Pubkey,
        target: Pubkey,
        route_hops: Vec<PaymentHopData>,
    ) -> Attempt {
        let now = crate::now_timestamp_as_millis_u64();
        let payment_hash = self.payment_hash();
        let hash = payment_hash;
        let try_limit = if self.allow_mpp() {
            DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT
        } else {
            self.try_limit
        };

        let route = SessionRoute::new(source, target, &route_hops);

        Attempt {
            id: attempt_id,
            hash,
            try_limit,
            tried_times: 1,
            payment_hash,
            route,
            route_hops,
            session_key: [0; 32],
            preimage: None,
            created_at: now,
            last_updated_at: now,
            last_error: None,
            status: AttemptStatus::Created,
        }
    }

    pub fn append_attempt(&mut self, attempt: Attempt) {
        self.cached_attempts.push(attempt);
    }

    pub fn allow_more_attempts(&self) -> bool {
        if self.status.is_final() {
            return false;
        }

        if self.remain_amount() == 0 {
            return false;
        }

        if self.retry_times() >= self.try_limit {
            return false;
        }

        if self.active_attempts().len() >= self.max_parts() {
            return false;
        }

        true
    }

    pub fn calc_payment_status(&self) -> PaymentStatus {
        if self.cached_attempts.is_empty() || self.status.is_final() {
            return self.status;
        }

        if self.attempts().any(|a| a.is_inflight()) {
            return PaymentStatus::Inflight;
        }

        if self.attempts().all(|a| a.is_failed()) && !self.allow_more_attempts() {
            return PaymentStatus::Failed;
        }

        if self.success_attempts_amount_is_enough() {
            return PaymentStatus::Success;
        }

        PaymentStatus::Created
    }

    pub fn set_inflight_status(&mut self) {
        self.status = PaymentStatus::Inflight;
        self.last_updated_at = crate::now_timestamp_as_millis_u64();
    }

    pub fn set_success_status(&mut self) {
        self.status = PaymentStatus::Success;
        self.last_updated_at = crate::now_timestamp_as_millis_u64();
        self.last_error = None;
        self.last_error_code = None;
    }

    pub fn set_failed_status(&mut self, error: &str) {
        self.status = PaymentStatus::Failed;
        self.last_updated_at = crate::now_timestamp_as_millis_u64();
        self.last_error = Some(error.to_string());
    }

    fn success_attempts_amount_is_enough(&self) -> bool {
        let success_amount: u128 = self
            .cached_attempts
            .iter()
            .filter_map(|a| {
                if a.is_success() {
                    Some(a.route.receiver_amount())
                } else {
                    None
                }
            })
            .sum();
        success_amount >= self.request.amount
    }
}
