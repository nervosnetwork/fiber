//! Sample data generation for testing and migration.
///
/// Re-exports from fiber-types::sample for shared types.
/// Fiber-lib specific implementations (e.g., ChannelActorState) are defined here.
#[cfg(any(test, feature = "sample"))]
pub use fiber_types::sample::{
    deterministic_ecdsa_signature, deterministic_hash, deterministic_hash256,
    deterministic_outpoint, deterministic_privkey, deterministic_pubkey,
    deterministic_recoverable_signature, deterministic_schnorr_signature, deterministic_seckey,
    deterministic_xonly_pubkey, StoreSample,
};

// Fiber-lib specific sample implementations
#[cfg(any(test, feature = "sample"))]
mod sample_channel;
#[cfg(any(test, feature = "sample"))]
mod sample_holdtlc;

/// Generate a deterministic PubNonce from a seed and index.
#[cfg(any(test, feature = "sample"))]
pub fn deterministic_pub_nonce(seed: u64, index: u32) -> musig2::PubNonce {
    use musig2::SecNonceBuilder;
    let hash = deterministic_hash(seed, index);
    let sec_nonce = SecNonceBuilder::new(hash).build();
    sec_nonce.public_nonce()
}
