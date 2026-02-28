//! Onion routing types and Sphinx packet processing.
//!
//! This module contains the core types and functions for creating, peeling, and
//! processing Sphinx onion packets used in payment routing and error propagation.

use crate::gen::fiber as molecule_fiber;
use crate::payment::{
    BasicMppPaymentData, CurrentPaymentHopData, PaymentCustomRecords, PaymentHopData, TlcErr,
    USER_CUSTOM_RECORDS_MAX_INDEX,
};
use ckb_types::prelude::{Pack, Unpack};
use fiber_sphinx::OnionErrorPacket;
use molecule::prelude::{Builder, Entity};
use serde::{Deserialize, Serialize};

// ============================================================
// Onion packet version constants
// ============================================================

/// Onion packet version with u64 BE length header for hop data.
pub const ONION_PACKET_VERSION_V0: u8 = 0;
/// Onion packet version with molecule's native u32 LE length for hop data.
pub const ONION_PACKET_VERSION_V1: u8 = 1;

const PACKET_DATA_LEN: usize = 6500;

/// Length of the u64 BE header used in v0 hop data format.
const HOP_DATA_HEAD_LEN: usize = std::mem::size_of::<u64>();

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
// TrampolineOnionData
// ============================================================

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
