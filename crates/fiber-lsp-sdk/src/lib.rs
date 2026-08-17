//! Portable SDK for hosted Fiber LSP clients and remote channel signing.
//!
//! This crate owns the root identity key and derives Fiber-compatible channel
//! signers without depending on a node runtime, transport, or database.

#![forbid(unsafe_code)]

#[cfg(feature = "json")]
pub mod json;
mod policy;
mod protocol;
mod root_key;
#[cfg(feature = "json")]
mod session;
mod signer;
mod store;

pub use fiber_types::{
    ChannelOpenSignerMaterial, NextChannelSignerMaterial, TenantId, TenantRegistryPayload,
    TenantRegistrySignature, TENANT_REGISTRY_PROTOCOL,
};
pub use policy::{
    OwnedSettlementBinding, PaymentRegistry, SettlementBinding, SigningDecision, SigningPolicy,
    SigningPolicyInput,
};
pub use protocol::{
    compute_tx_message, ChannelBinding, ChannelKeyId, ChannelPublicMaterial, ChannelSignature,
    ChannelSigningContent, CommitmentCounter, Musig2Nonce, Musig2SignableContent, Musig2Signature,
    Musig2SigningContent, NoncePurpose, NonceSlot, OnchainKeyPurpose, OnchainSignature,
    OnchainSigningContent, PreparedSigning, SignerError, SigningIntent, SigningReview,
    SigningWarning,
};
pub use root_key::{RootKey, RootKeyBackup};
#[cfg(feature = "json")]
pub use session::{
    HostedSession, HostedSessionState, PendingRequest, ProcessOutcome, SessionError, SubmitParams,
};
pub use signer::{ChannelSigner, CreatedRootSigner, RootSigner};
pub use store::{MemoryStore, MemoryStoreError, SignerStore};
