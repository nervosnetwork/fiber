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

pub mod cch;
pub mod channel;
pub mod config;
pub mod gen;
pub mod invoice;
pub mod network;
pub mod onion;
pub mod payment;
pub mod primitives;
pub mod protocol;
pub mod schema;
pub mod serde_utils;

pub use cch::{CchInvoice, CchOrder, CchOrderStatus};
pub use channel::*;
pub use config::*;
pub use invoice::*;
pub use network::PersistentNetworkActorState;
pub use onion::*;
pub use payment::*;
pub use primitives::{Hash256, NodeId, Privkey, Pubkey};
pub use protocol::*;

#[cfg(feature = "watchtower")]
pub use watchtower::{ChannelData, RevocationData, SettlementData, SettlementTlc};

#[cfg(feature = "watchtower")]
pub mod watchtower;

pub use serde_utils::{
    duration_hex, from_hex, to_hex, CompactSignatureAsBytes, EntityHex, PartialSignatureAsBytes,
    PubNonceAsBytes, SliceBase58, SliceHex, SliceHexNoPrefix, U128Hex, U16Hex, U32Hex, U64Hex,
};

pub use tentacle_multiaddr::Multiaddr;
pub use tentacle_secio::PeerId;

// ============================================================
// WASM-compatible time
// ============================================================

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use std::time as crate_time;
#[cfg(target_arch = "wasm32")]
pub(crate) use web_time as crate_time;

// ============================================================
// Utility functions
// ============================================================

/// Get the current timestamp as milliseconds since the Unix epoch.
pub fn now_timestamp_as_millis_u64() -> u64 {
    crate_time::SystemTime::now()
        .duration_since(crate_time::UNIX_EPOCH)
        .expect("Duration since unix epoch")
        .as_millis() as u64
}

// ============================================================
// Store serialization utilities
// ============================================================

/// Deserialize a value from bincode-encoded bytes.
///
/// This function is used to deserialize values stored in the node's RocksDB.
/// External applications can use this to read and parse store data directly.
///
/// # Example
///
/// ```ignore
/// use fiber_types::{schema, ChannelActorState, deserialize};
///
/// let key = [&[schema::CHANNEL_ACTOR_STATE_PREFIX], channel_id.as_ref()].concat();
/// let value = db.get(&key)?;
/// let state: ChannelActorState = deserialize(&value)?;
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
