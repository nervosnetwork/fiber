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

pub mod serde_utils;

pub mod primitives;
pub mod protocol;

pub mod cch;
pub mod channel;
pub mod gen;
pub mod invoice;
pub mod network;
pub mod payment;
pub mod schema;

#[cfg(feature = "watchtower")]
pub mod watchtower;

pub use primitives::{Hash256, NodeId, Privkey, Pubkey};

pub use channel::{
    AddTlcCommand, AppliedFlags, AwaitingChannelReadyFlags, AwaitingTxSignaturesFlags,
    ChannelActorStateCore, ChannelBasePublicKeys, ChannelConstraints, ChannelFlags,
    ChannelOpenRecord, ChannelOpeningStatus, ChannelState, ChannelTlcInfo,
    ChannelUpdateChannelFlags, ChannelUpdateMessageFlags, CloseFlags, CollaboratingFundingTxFlags,
    CommitmentNumbers, InMemorySigner, InboundTlcStatus, NegotiatingFundingFlags,
    OutboundTlcStatus, PendingNotifySettleTlc, PendingTlcs, PrevTlcInfo, PublicChannelInfo,
    RetryableTlcOperation, RevokeAndAck, ShutdownInfo, ShuttingDownFlags, SigningCommitmentFlags,
    TLCId, TlcInfo, TlcState, TlcStatus, INITIAL_COMMITMENT_NUMBER,
};

pub use payment::{
    Attempt, AttemptStatus, HopHint, PaymentCustomRecords, PaymentHopData, PaymentOnionPacket,
    PaymentSession, PaymentStatus, RemoveTlcFulfill, RemoveTlcReason, RouterHop, SendPaymentData,
    SessionRoute, SessionRouteNode, TimedResult, TlcErrPacket, TlcErrorCode, TrampolineContext,
    NO_SHARED_SECRET, USER_CUSTOM_RECORDS_MAX_INDEX,
};

pub use protocol::{
    feature_bits, AnnouncedNodeName, BroadcastMessage, BroadcastMessageID, ChannelAnnouncement,
    ChannelUpdate, Cursor, EcdsaSignature, FeatureBit, FeatureVector, NodeAnnouncement,
    SchnorrSignature, UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtDep, UdtScript, CURSOR_SIZE,
};

pub use network::PersistentNetworkActorState;

pub use cch::{CchInvoice, CchOrder, CchOrderStatus};

pub use invoice::{
    ar_decompress, ar_encompress, construct_invoice_preimage, parse_hrp, sha256, Attribute,
    CkbInvoice, CkbInvoiceStatus, CkbScript, Currency, HashAlgorithm, InvoiceData, InvoiceError,
    InvoiceSignature, UnknownHashAlgorithmError, VerificationError, SIGNATURE_U5_SIZE,
};

#[cfg(feature = "watchtower")]
pub use watchtower::{ChannelData, RevocationData, SettlementData, SettlementTlc};

pub use serde_utils::{
    duration_hex, from_hex, to_hex, CompactSignatureAsBytes, EntityHex, PartialSignatureAsBytes,
    PubNonceAsBytes, SliceBase58, SliceHex, SliceHexNoPrefix, U128Hex, U16Hex, U32Hex, U64Hex,
};

// Re-export tentacle types for external use
pub use tentacle_multiaddr::Multiaddr;
pub use tentacle_secio::PeerId;

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
