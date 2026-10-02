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
pub mod channel_v2_validation;
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

#[cfg(feature = "sample")]
pub mod sample;

#[cfg(feature = "cch")]
pub use cch::{
    CchInvoice, CchOrder, CchOrderStatus, CchReceiveBtcOrderCreation, CchSendBtcOrderCreation,
};
pub use channel::*;
pub use config::*;
pub use invoice::*;
pub use network::{HopRequire, PersistentNetworkActorState};
pub use onion::*;
pub use payment::*;
pub use primitives::{Hash256, NodeId, Privkey, Pubkey};
pub use protocol::*;

pub use watchtower::{ChannelData, RevocationData, SettlementData, SettlementTlc};

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
/// Channel actor data uses the exact, structurally checked published-legacy compatibility
/// decoder. Stored values are owned so this dispatch can use their actual type.
///
/// # Example
///
/// ```ignore
/// use fiber_types::{schema, ChannelActorData, deserialize};
///
/// let key = [&[schema::CHANNEL_ACTOR_STATE_PREFIX], channel_id.as_ref()].concat();
/// let value = db.get(&key)?;
/// let state: ChannelActorData = deserialize(&value)?;
/// ```
pub fn deserialize<T: serde::de::DeserializeOwned + 'static>(
    bytes: &[u8],
) -> Result<T, bincode::Error> {
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<ChannelActorData>() {
        let channel = channel_v2_validation::decode_channel_actor_data(bytes)
            .map_err(|e| Box::new(bincode::ErrorKind::Custom(e)))?;
        let value: Box<dyn std::any::Any> = Box::new(channel);
        return value.downcast::<T>().map(|channel| *channel).map_err(|_| {
            Box::new(bincode::ErrorKind::Custom(
                "Stored channel type mismatch".to_owned(),
            ))
        });
    }
    bincode::deserialize(bytes)
}

/// Serialize a value to bincode-encoded bytes.
///
/// This function is used to serialize values for storage in the node's RocksDB.
pub fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, bincode::Error> {
    bincode::serialize(value)
}
