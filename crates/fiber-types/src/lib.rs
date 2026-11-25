//! Core domain types for the Fiber Network.
//!
//! This crate provides the shared type definitions used across the Fiber Network
//! ecosystem, including:
//! - Primitive types: `Hash256`, `Pubkey`
//! - Channel state types: bitflags, `ChannelState`, `TLCId`, TLC status enums
//! - Payment types: `PaymentStatus`, `PaymentCustomRecords`
//! - Invoice types: `CkbInvoiceStatus`, `Currency`, `HashAlgorithm`, `CkbScript`,
//!   `InvoiceSignature`
//! - CCH types: `CchOrderStatus`
//! - Network types: `PersistentNetworkActorState`
//! - Watchtower types: `ChannelData` (feature-gated)
//! - Store schema constants
//! - Serde utilities for hex and base58 serialization
//! - Molecule generated types for protocol messages

#[cfg(feature = "cch")]
pub mod cch;
pub mod channel;
pub mod channel_signer;
pub mod channel_v2_validation;
pub mod config;
pub mod gen;
pub mod hosted_lsp;
pub mod invoice;
pub mod network;
pub mod onion;
pub mod payment;
pub mod primitives;
pub mod protocol;
pub mod schema;
pub mod serde_utils;

#[cfg(feature = "sample")]
pub mod sample;

#[cfg(feature = "cch")]
pub use cch::{
    CchInvoice, CchOrder, CchOrderStatus, CchReceiveBtcOrderCreation, CchSendBtcOrderCreation,
};
pub use channel::*;
pub use channel_signer::*;
pub use config::*;
pub use hosted_lsp::*;
pub use invoice::*;
pub use network::{HopRequire, PersistentNetworkActorState};
pub use onion::*;
pub use payment::*;
pub use primitives::{Hash256, NodeId, Privkey, Pubkey};
pub use protocol::*;

pub use watchtower::{
    commitment_lock_settlement_hash, self_funded_watchtower_tx_hash, settlement_data_to_witness,
    settlement_matches_commitment_lock_args, settlement_matches_commitment_tx,
    settlement_tlc_local_pubkey_hash, settlement_tlc_to_witness, settlement_witness_hash,
    validate_and_normalize_self_funded_watchtower_tx, ChannelData, LastAppliedWatchtowerSignature,
    PendingWatchtowerRequest, RevocationData, SettlementData, SettlementKey, SettlementTlc,
    SignedWatchtowerRequest, WatchChannelKeys, WatchtowerExternalSignerState,
    WatchtowerSignerState, MAX_SELF_FUNDED_WATCHTOWER_TX_BYTES,
    MAX_SELF_FUNDED_WATCHTOWER_TX_ITEMS, MAX_WITNESS_TLC_COUNT,
};

pub mod watchtower;

pub use serde_utils::{
    duration_hex, from_hex, to_hex, CompactSignatureAsBytes, EntityHex, PartialSignatureAsBytes,
    PubNonceAsBytes, SliceBase58, SliceHex, SliceHexNoPrefix, U128Hex, U16Hex, U32Hex, U64Hex,
};

pub use tentacle_multiaddr::Multiaddr;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use std::time as crate_time;
#[cfg(target_arch = "wasm32")]
pub(crate) use web_time as crate_time;

/// Get the current timestamp as milliseconds since the Unix epoch.
pub fn now_timestamp_as_millis_u64() -> u64 {
    crate_time::SystemTime::now()
        .duration_since(crate_time::UNIX_EPOCH)
        .expect("Duration since unix epoch")
        .as_millis() as u64
}

/// Deserialize a value from bincode-encoded bytes.
///
/// This function is used to deserialize values stored in the node's RocksDB.
/// External applications can use this to read and parse store data directly.
///
/// It is a plain bincode read, so it does not validate what it decodes, and it accepts trailing
/// bytes. A channel is stored as two records (`CHANNEL_ACTOR_STATE_PREFIX` holds
/// [`ChannelMutableData`], `CHANNEL_IMMUTABLE_STATE_PREFIX` holds [`ChannelImmutableData`]), and
/// the node reads them with `channel_v2_validation::decode_channel_actor_data_parts`, which checks
/// both records and the channel id. Use that when the data may be corrupt or from another layout.
///
/// A backup written by a released version is a different case: its channel is one record whose
/// session still carries the retired schema marker, so `decode_channel_actor_data` rejects it and it
/// has to go through the migration. That decoder only reads the intermediate layout written after
/// the marker was dropped and before the split.
///
/// # Example
///
/// ```ignore
/// use fiber_types::{schema, ChannelActorData, ChannelImmutableData, ChannelMutableData, deserialize};
///
/// let mutable_key = [&[schema::CHANNEL_ACTOR_STATE_PREFIX], channel_id.as_ref()].concat();
/// let immutable_key = [&[schema::CHANNEL_IMMUTABLE_STATE_PREFIX], channel_id.as_ref()].concat();
/// // Read both keys from one snapshot: a delete or a move between two reads would pair half a
/// // channel with half of another.
/// let mut values = db.multi_get(&[mutable_key, immutable_key]).into_iter();
/// let mutable = values.next().flatten().ok_or("channel is gone")?;
/// let immutable = values.next().flatten().ok_or("fixed record is missing")?;
/// let state = channel_v2_validation::decode_channel_actor_data_parts(&channel_id, &mutable, &immutable)?;
/// ```
pub fn deserialize<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, bincode::Error> {
    bincode::deserialize(bytes)
}

/// Serialize a value to bincode-encoded bytes.
///
/// This function is used to serialize values for storage in the node's RocksDB.
pub fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, bincode::Error> {
    bincode::serialize(value)
}
