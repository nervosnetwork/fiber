//! Portable SDK for hosted Fiber LSP clients and remote channel signing.
//!
//! This crate owns the root identity key and derives Fiber-compatible channel
//! signers without depending on a node runtime, transport, or database.

#![forbid(unsafe_code)]

mod commitment;
mod lifecycle;
#[cfg(feature = "json")]
mod opening;
#[cfg(feature = "json")]
pub use opening::{FundingVerifier, OpeningNetwork, OpeningPersistence, TenantOpeningRpc};
mod onchain;
pub use lifecycle::{
    CloseAuthorization, CommitmentReference, RecoveryRecord, RevocationContext, RevocationRecord,
    SigningSessionEvidence,
};
pub use onchain::{ChainVerifier, OnchainSpendAuthorization, VerifiedCell};
#[cfg(feature = "json")]
pub mod json;
mod policy;
mod protocol;
mod root_key;
#[cfg(feature = "json")]
mod session;
mod signer;
mod store;
mod watchtower;

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
#[cfg(feature = "json")]
pub use watchtower::evaluate_watchtower_status;
pub use watchtower::{
    handle_watchtower_rpc_error, NoopWatchtowerEventHandler, WatchtowerEventHandler,
    WatchtowerHealthStatus, WatchtowerLimitPolicy, WatchtowerQuotaAction, WatchtowerQuotaError,
    WatchtowerQuotaKind, WatchtowerQuotaScope,
};

pub use commitment::{
    CommitmentContext, CommitmentParameters, CommitmentReview, PaymentAuthorization,
};

#[cfg(feature = "simulation")]
type SimulationRngFill = fn(&mut [u8]);

#[cfg(feature = "simulation")]
thread_local! {
    static SIMULATION_RNG: std::cell::Cell<Option<SimulationRngFill>> = const { std::cell::Cell::new(None) };
}

/// Install a deterministic random-byte filler on the current thread for simulation tests.
#[cfg(feature = "simulation")]
pub fn with_simulation_rng<T>(fill: SimulationRngFill, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<SimulationRngFill>);
    impl Drop for Reset {
        fn drop(&mut self) {
            SIMULATION_RNG.set(self.0);
        }
    }
    let previous = SIMULATION_RNG.replace(Some(fill));
    let _reset = Reset(previous);
    run()
}

pub(crate) fn fill_random(dest: &mut [u8]) -> Result<(), SignerError> {
    #[cfg(feature = "simulation")]
    if let Some(fill) = SIMULATION_RNG.get() {
        fill(dest);
        return Ok(());
    }
    getrandom::fill(dest).map_err(|error| SignerError::Random(error.to_string()))
}

#[cfg(test)]
mod commitment_tests;
