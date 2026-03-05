//! RPC JSON request/response types for the Fiber Network.
//!
//! This crate contains the pure data types (request/response structs and enums)
//! used by the Fiber Network's JSON-RPC API. These types are separated from the
//! RPC server implementations to allow external tools and clients to depend on
//! the type definitions without pulling in the full node implementation.
//!
//! All domain-specific types (Hash256, Pubkey, MultiAddr, etc.) are represented
//! as plain strings in this crate to avoid dependencies on fiber-types, secp256k1,
//! and tentacle-multiaddr. Conversions between these JSON types and the internal
//! domain types are handled in the RPC server layer.
//!
//! The types are organized by RPC module:
//! - `channel`: Channel management types
//! - `payment`: Payment types
//! - `graph`: Network graph types
//! - `invoice`: Invoice types
//! - `info`: Node info types
//! - `peer`: Peer management types
//! - `cch`: Cross-chain hub types
//! - `dev`: Development/debug types
//! - `watchtower`: Watchtower types
//! - `prof`: Profiling types
//! - `context`: RPC context types

pub mod serde_utils;

#[cfg(feature = "conversion")]
pub mod convert;

pub mod cch;
pub mod channel;
pub mod context;
pub mod dev;
pub mod graph;
pub mod info;
pub mod invoice;
pub mod payment;
pub mod peer;
pub mod prof;
pub mod watchtower;

// Re-export all types for convenience
pub use cch::*;
pub use channel::*;
pub use context::*;
pub use dev::*;
pub use graph::*;
pub use info::*;
pub use invoice::*;
pub use payment::*;
pub use peer::*;
pub use prof::*;
pub use serde_utils::Hash256;
pub use serde_utils::Pubkey;
pub use watchtower::*;
