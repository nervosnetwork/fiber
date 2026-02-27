//! Payment-related types.

use crate::gen::fiber as molecule_fiber;
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
// TlcErrPacket extended methods
// ============================================================

/// Always decrypting 27 times so the erroring node cannot learn its relative position in the route
/// by performing a timing analysis if the sender were to retry the same route multiple times.
const ERROR_DECODING_PASSES: usize = 27;

impl TlcErrPacket {
    /// Erring node creates the error packet using the shared secret used in forwarding onion packet.
    /// Use all zeros for the origin node. Takes a structured `TlcErr` and serializes it.
    pub fn new(tlc_fail: TlcErr, shared_secret: &[u8; 32]) -> Self {
        let payload = tlc_fail.serialize();
        Self::from_payload(payload, shared_secret)
    }

    /// Decode the onion error packet using the session key and hop public keys.
    pub fn decode(
        &self,
        session_key: &[u8; 32],
        hops_public_keys: Vec<crate::Pubkey>,
    ) -> Option<TlcErr> {
        use secp256k1::{PublicKey, SecretKey};

        if self.is_plaintext() {
            let error = TlcErr::deserialize(&self.onion_packet[32..]);
            if error.is_some() {
                return error;
            }
        }

        let hops_public_keys: Vec<PublicKey> = hops_public_keys
            .iter()
            .map(|k| PublicKey::from_slice(&k.0).expect("valid pubkey"))
            .collect();
        let session_key = SecretKey::from_slice(session_key)
            .inspect_err(|err| {
                tracing::error!(
                    target: "fnn::fiber::types::TlcErrPacket",
                    "decode session_key error={} key={}",
                    err,
                    hex::encode(session_key)
                )
            })
            .ok()?;
        OnionErrorPacket::from_bytes(self.onion_packet.clone())
            .parse(hops_public_keys, session_key, TlcErr::deserialize)
            .map(|(error, hop_index)| {
                for _ in hop_index..ERROR_DECODING_PASSES {
                    OnionErrorPacket::from_bytes(self.onion_packet.clone())
                        .xor_cipher_stream(&NO_SHARED_SECRET);
                }
                error
            })
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

    /// Convert into the raw Sphinx onion packet.
    pub fn into_sphinx_onion_packet(self) -> Result<fiber_sphinx::OnionPacket, OnionPacketError> {
        fiber_sphinx::OnionPacket::from_bytes(self.data).map_err(OnionPacketError::Sphinx)
    }

    /// Peels the next layer of the onion packet using the privkey of the current node.
    ///
    /// Returns errors when:
    /// - This is the packet for the last hop.
    /// - Fail to peel the packet using the given private key.
    pub fn peel<C: secp256k1::Verification>(
        self,
        peeler: &crate::Privkey,
        assoc_data: Option<&[u8]>,
        secp_ctx: &secp256k1::Secp256k1<C>,
    ) -> Result<PeeledPaymentOnionPacket, OnionPacketError> {
        let peeled =
            peel_sphinx_onion::<C, PaymentSphinxCodec>(self.data, peeler, assoc_data, secp_ctx)?;
        Ok(PeeledPaymentOnionPacket {
            current: peeled.current,
            next: peeled.next.map(PaymentOnionPacket::new),
            shared_secret: peeled.shared_secret,
        })
    }
}

// ============================================================
// OnionPacketError
// ============================================================

/// Errors that can occur when processing an onion packet.
#[derive(thiserror::Error, Debug)]
pub enum OnionPacketError {
    #[error("Fail to deserialize the hop data")]
    InvalidHopData,

    #[error("Unknown onion packet version: {0}")]
    UnknownVersion(u8),

    #[error("Sphinx protocol error")]
    Sphinx(#[from] fiber_sphinx::SphinxError),
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
// PeeledPaymentOnionPacket
// ============================================================

/// A peeled payment onion packet, containing the current hop data and the packet for the next hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeeledPaymentOnionPacket {
    /// The decrypted hop data for the current hop.
    pub current: CurrentPaymentHopData,
    /// The shared secret for `current` used for returning error. Set to all zeros for the origin node
    /// who has no shared secret.
    pub shared_secret: [u8; 32],
    /// The packet for the next hop.
    pub next: Option<PaymentOnionPacket>,
}

impl PeeledPaymentOnionPacket {
    /// - `hops_info`: the first is the instruction for the origin node itself.
    ///   Remaining elements are for each node to receive the packet.
    pub fn create<C: secp256k1::Signing>(
        session_key: crate::Privkey,
        mut hops_infos: Vec<PaymentHopData>,
        assoc_data: Option<Vec<u8>>,
        secp_ctx: &secp256k1::Secp256k1<C>,
    ) -> Result<Self, OnionPacketError> {
        if hops_infos.is_empty() {
            return Err(OnionPacketError::Sphinx(
                fiber_sphinx::SphinxError::HopsIsEmpty,
            ));
        }

        let hops_path: Vec<crate::Pubkey> = hops_infos
            .iter()
            .map(|h| h.next_hop())
            .take_while(Option::is_some)
            .map(|opt| opt.expect("must be some"))
            .collect();

        // Keep the original hop ordering for payloads.
        let current = hops_infos.remove(0);
        let payloads = hops_infos;

        let next = if !hops_path.is_empty() {
            Some(PaymentOnionPacket::new(create_sphinx_onion::<
                C,
                PaymentSphinxCodec,
            >(
                session_key,
                hops_path,
                payloads,
                assoc_data,
                secp_ctx,
            )?))
        } else {
            None
        };

        Ok(PeeledPaymentOnionPacket {
            current: current.into(),
            next,
            // Use all zeros for the sender
            shared_secret: NO_SHARED_SECRET,
        })
    }

    /// Returns true if this is the peeled packet for the last destination.
    pub fn is_last(&self) -> bool {
        self.next.is_none()
    }

    /// Returns the MPP custom records from the current hop data, if present.
    pub fn mpp_custom_records(&self) -> Option<BasicMppPaymentData> {
        self.current
            .custom_records
            .as_ref()
            .and_then(BasicMppPaymentData::read)
    }
}

// ============================================================
// Sphinx Onion Codec infrastructure
// ============================================================

/// Onion packet version with u64 BE length header for hop data.
pub const ONION_PACKET_VERSION_V0: u8 = 0;
/// Onion packet version with molecule's native u32 LE length for hop data.
pub const ONION_PACKET_VERSION_V1: u8 = 1;

const PACKET_DATA_LEN: usize = 6500;

/// Length of the u64 BE header used in v0 hop data format.
const HOP_DATA_HEAD_LEN: usize = std::mem::size_of::<u64>();

/// Trait for encoding/decoding Sphinx onion packet hop data.
///
/// This trait abstracts over different onion packet formats (payment vs trampoline),
/// allowing the generic `peel_sphinx_onion` and `create_sphinx_onion` functions to
/// work with any format.
pub trait SphinxOnionCodec {
    type Decoded;
    type Current;

    const PACKET_DATA_LEN: usize;
    /// The onion packet version used when creating new packets.
    const CURRENT_VERSION: u8;

    /// Packs the decoded data for transmission. Must use `CURRENT_VERSION` format.
    fn pack(decoded: &Self::Decoded) -> Vec<u8>;
    /// Unpacks data received from the network. Must handle all versions allowed by `is_version_allowed`.
    fn unpack(version: u8, buf: &[u8]) -> Option<Self::Decoded>;
    fn to_current(decoded: Self::Decoded) -> Self::Current;
    /// Returns true if the given version is allowed for this codec.
    fn is_version_allowed(version: u8) -> bool;
    /// Returns the total length of hop data (including any headers) for the specified version.
    fn hop_data_len(version: u8, buf: &[u8]) -> Option<usize>;
}

/// Internal result of peeling a Sphinx onion layer.
pub struct SphinxPeeled<Current> {
    pub current: Current,
    pub shared_secret: [u8; 32],
    pub next: Option<Vec<u8>>,
}

/// Peels one layer of a Sphinx onion packet.
pub fn peel_sphinx_onion<C: secp256k1::Verification, Codec: SphinxOnionCodec>(
    packet_bytes: Vec<u8>,
    peeler: &crate::Privkey,
    assoc_data: Option<&[u8]>,
    secp_ctx: &secp256k1::Secp256k1<C>,
) -> Result<SphinxPeeled<Codec::Current>, OnionPacketError> {
    let sphinx_packet =
        fiber_sphinx::OnionPacket::from_bytes(packet_bytes).map_err(OnionPacketError::Sphinx)?;
    let version = sphinx_packet.version;
    if !Codec::is_version_allowed(version) {
        return Err(OnionPacketError::UnknownVersion(version));
    }
    let shared_secret = sphinx_packet.shared_secret(&peeler.0);

    let (new_current, new_next) = sphinx_packet
        .peel(&peeler.0, assoc_data, secp_ctx, |buf| {
            Codec::hop_data_len(version, buf)
        })
        .map_err(OnionPacketError::Sphinx)?;

    let decoded = Codec::unpack(version, &new_current).ok_or(OnionPacketError::InvalidHopData)?;
    let current = Codec::to_current(decoded);

    // All zeros hmac indicates the last hop.
    let next = new_next
        .hmac
        .iter()
        .any(|b| *b != 0)
        .then(|| new_next.into_bytes());

    Ok(SphinxPeeled {
        current,
        shared_secret,
        next,
    })
}

/// Creates a Sphinx onion packet from hop data.
pub fn create_sphinx_onion<C: secp256k1::Signing, Codec: SphinxOnionCodec>(
    session_key: crate::Privkey,
    hops_path: Vec<crate::Pubkey>,
    payloads: Vec<Codec::Decoded>,
    assoc_data: Option<Vec<u8>>,
    secp_ctx: &secp256k1::Secp256k1<C>,
) -> Result<Vec<u8>, OnionPacketError> {
    let hops_path: Vec<secp256k1::PublicKey> = hops_path
        .into_iter()
        .map(|pk| secp256k1::PublicKey::from_slice(&pk.0).expect("valid public key"))
        .collect();
    let hops_data: Vec<Vec<u8>> = payloads.iter().map(|p| Codec::pack(p)).collect();
    let mut packet = fiber_sphinx::OnionPacket::create(
        session_key.0,
        hops_path,
        hops_data,
        assoc_data,
        Codec::PACKET_DATA_LEN,
        secp_ctx,
    )
    .map_err(OnionPacketError::Sphinx)?;
    // Set the version to indicate which hop data format is used
    packet.version = Codec::CURRENT_VERSION;
    Ok(packet.into_bytes())
}

// ============================================================
// PaymentSphinxCodec
// ============================================================

/// Codec for payment onion packets (used by the outer payment onion layer).
pub struct PaymentSphinxCodec;

impl PaymentSphinxCodec {
    /// Packs hop data according to the specified onion packet version.
    /// - Version 0: Prepends u64 BE length header before molecule data.
    /// - Version 1: Returns molecule-serialized data directly (uses molecule's native u32 LE length).
    pub fn pack_hop_data(version: u8, hop_data: &PaymentHopData) -> Vec<u8> {
        match version {
            ONION_PACKET_VERSION_V0 => pack_len_prefixed(hop_data.serialize()),
            ONION_PACKET_VERSION_V1 => hop_data.serialize(),
            other => {
                debug_assert!(
                    false,
                    "Unknown onion packet version {} passed to pack_hop_data; defaulting to v1",
                    other
                );
                hop_data.serialize()
            }
        }
    }

    /// Unpacks hop data according to the specified onion packet version.
    /// - Version 0: Skips u64 BE length header, deserializes molecule data.
    /// - Version 1: Deserializes molecule data directly (using molecule's u32 LE length).
    /// - Unknown versions: Returns None to fail fast and avoid silent misparsing.
    pub fn unpack_hop_data(version: u8, buf: &[u8]) -> Option<PaymentHopData> {
        match version {
            ONION_PACKET_VERSION_V0 => {
                let payload = unpack_len_prefixed_payload(buf)?;
                PaymentHopData::deserialize(payload)
            }
            ONION_PACKET_VERSION_V1 => {
                let len = molecule_table_data_len(buf)?;
                if buf.len() < len {
                    return None;
                }
                PaymentHopData::deserialize(&buf[..len])
            }
            _ => None,
        }
    }
}

impl SphinxOnionCodec for PaymentSphinxCodec {
    type Decoded = PaymentHopData;
    type Current = CurrentPaymentHopData;

    const PACKET_DATA_LEN: usize = PACKET_DATA_LEN;
    // Send v1, accept both v0 and v1 for backward compatibility
    const CURRENT_VERSION: u8 = ONION_PACKET_VERSION_V1;

    fn pack(decoded: &Self::Decoded) -> Vec<u8> {
        Self::pack_hop_data(Self::CURRENT_VERSION, decoded)
    }

    fn unpack(version: u8, buf: &[u8]) -> Option<Self::Decoded> {
        Self::unpack_hop_data(version, buf)
    }

    fn to_current(decoded: Self::Decoded) -> Self::Current {
        decoded.into()
    }

    fn is_version_allowed(version: u8) -> bool {
        // Accept both v0 and v1 for backward compatibility
        version <= ONION_PACKET_VERSION_V1
    }

    fn hop_data_len(version: u8, buf: &[u8]) -> Option<usize> {
        match version {
            ONION_PACKET_VERSION_V0 => len_with_u64_header(buf),
            ONION_PACKET_VERSION_V1 => molecule_table_data_len(buf),
            _ => None,
        }
    }
}

// ============================================================
// Onion packet helper functions
// ============================================================

/// Packs data with u64 BE length header (v0 format).
/// Used by Trampoline (bincode serialization) and v0 payment hop data.
pub fn pack_len_prefixed(mut payload: Vec<u8>) -> Vec<u8> {
    let mut packed = (payload.len() as u64).to_be_bytes().to_vec();
    packed.append(&mut payload);
    packed
}

/// Unpacks length-prefixed payload (v0 format): [u64 BE length][data].
pub fn unpack_len_prefixed_payload(buf: &[u8]) -> Option<&[u8]> {
    let len = len_with_u64_header(buf)?;
    if buf.len() < len {
        return None;
    }
    buf.get(HOP_DATA_HEAD_LEN..len)
}

/// Returns the total length with u64 BE header: [u64 BE length] + data.
/// Used by v0 format (Trampoline and legacy payment hop data).
pub fn len_with_u64_header(buf: &[u8]) -> Option<usize> {
    if buf.len() < HOP_DATA_HEAD_LEN {
        return None;
    }
    let len = u64::from_be_bytes(
        buf[0..HOP_DATA_HEAD_LEN]
            .try_into()
            .expect("u64 from slice"),
    );
    // Safe conversion: check value fits in usize and addition won't overflow.
    // Note: Caller (fiber-sphinx) is responsible for validating len against packet bounds.
    usize::try_from(len).ok()?.checked_add(HOP_DATA_HEAD_LEN)
}

/// Returns the total length from molecule's native u32 LE header.
/// Used by v1 format (current payment hop data).
pub fn molecule_table_data_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < molecule::NUMBER_SIZE {
        return None;
    }
    let len = molecule::unpack_number(buf) as usize;
    // Molecule size must be at least NUMBER_SIZE (4 bytes for the length header itself).
    // Reject malformed data claiming a smaller size.
    if len < molecule::NUMBER_SIZE {
        return None;
    }
    Some(len)
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

use crate::U128Hex;

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

/// Helper to store the trampoline onion packet inside `custom_records`.
///
/// This embeds the trampoline onion bytes as a custom record entry so that the molecule
/// `PaymentHopData` schema stays at 7 fields — matching v0.6.1 — and old onion packets
/// created before trampoline support can still be deserialized.
pub struct TrampolineOnionData;

impl TrampolineOnionData {
    /// Custom record key for embedded trampoline onion data.
    /// `BasicMppPaymentData` uses `USER_CUSTOM_RECORDS_MAX_INDEX + 1` (65536).
    pub const CUSTOM_RECORD_KEY: u32 = USER_CUSTOM_RECORDS_MAX_INDEX + 2;

    pub fn write(data: Vec<u8>, custom_records: &mut PaymentCustomRecords) {
        custom_records.data.insert(Self::CUSTOM_RECORD_KEY, data);
    }

    pub fn read(custom_records: &PaymentCustomRecords) -> Option<Vec<u8>> {
        custom_records.data.get(&Self::CUSTOM_RECORD_KEY).cloned()
    }
}

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
