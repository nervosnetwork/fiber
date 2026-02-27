//! JSON-serializable types for the Fiber Network Node RPC API.
//!
//! This crate provides lightweight type definitions for all RPC request/response
//! structs, enums, and serde utilities. It is designed to be depended upon by
//! CLI tools and other consumers that need type-safe RPC parameter construction
//! without pulling in the full `fiber-lib` crate.

pub mod serde_utils;

mod hash256;
mod pubkey;

pub use hash256::Hash256;
pub use pubkey::Pubkey;

pub use serde_utils::{
    duration_hex, EntityHex, SliceBase58, SliceHex, SliceHexNoPrefix, U128Hex, U16Hex, U32Hex,
    U64Hex,
};

pub use ckb_jsonrpc_types::Script as JsonScript;
pub use tentacle::multiaddr::MultiAddr;
pub use tentacle::secio::PeerId;

// Sub-modules for domain types
pub mod cch;
pub mod channel;
pub mod dev;
pub mod graph;
pub mod info;
pub mod invoice;
pub mod payment;
pub mod peer;
