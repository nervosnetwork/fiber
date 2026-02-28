/// `StoreSample` trait for generating deterministic sample data to test
/// store serialization compatibility and data migration correctness.
///
/// Each implementation generates a set of representative sample instances
/// for a given store-persisted type. These samples can be serialized to
/// fixture files and used to verify that migrations produce correctly
/// deserializable data across versions.
///
/// # Usage
///
/// ```ignore
/// // Generate fixtures for the current version
/// let samples = ChannelActorState::samples(42);
/// for (i, sample) in samples.iter().enumerate() {
///     let bytes = bincode::serialize(sample).unwrap();
///     std::fs::write(format!("fixtures/v0.7.0/channel_state_{}.bin", i), bytes).unwrap();
/// }
///
/// // Verify fixtures from a previous version still deserialize
/// let bytes = std::fs::read("fixtures/v0.6.1/channel_state_0.bin").unwrap();
/// let _state: ChannelActorState = bincode::deserialize(&bytes).unwrap();
/// ```
use serde::{Deserialize, Serialize};

mod sample_broadcast;
mod sample_channel;
mod sample_channel_open;
mod sample_holdtlc;
mod sample_invoice;
mod sample_network;
mod sample_payment;

/// Trait for types that are persisted in the store and need migration testing.
///
/// Implementations must produce deterministic samples for a given seed,
/// meaning the same seed always yields identical byte-level output.
pub trait StoreSample: Serialize + for<'de> Deserialize<'de> + Sized {
    /// The store key prefix byte for this type (from `schema.rs`).
    const STORE_PREFIX: u8;

    /// A human-readable name for logging and fixture file naming.
    const TYPE_NAME: &'static str;

    /// Generate a set of deterministic sample instances.
    ///
    /// The `seed` parameter controls the deterministic random state.
    /// Multiple samples should cover different meaningful states:
    /// - Minimal/default state
    /// - Fully populated state
    /// - Edge cases (e.g., channel with shutdown in progress)
    ///
    /// **IMPORTANT**: The output must be fully deterministic — same seed
    /// must always produce byte-identical serialized output. Do NOT use
    /// `SystemTime::now()`, `rand::thread_rng()`, or any non-deterministic source.
    fn samples(seed: u64) -> Vec<Self>;

    /// Verify that all samples round-trip through bincode serialization.
    fn verify_samples_roundtrip(seed: u64) {
        let samples = Self::samples(seed);
        assert!(
            !samples.is_empty(),
            "{}: samples() must return at least one sample",
            Self::TYPE_NAME
        );
        for (i, sample) in samples.iter().enumerate() {
            let bytes = bincode::serialize(sample).unwrap_or_else(|e| {
                panic!("{} sample {}: serialize failed: {}", Self::TYPE_NAME, i, e)
            });
            let _roundtrip: Self = bincode::deserialize(&bytes).unwrap_or_else(|e| {
                panic!(
                    "{} sample {}: deserialize failed: {}",
                    Self::TYPE_NAME,
                    i,
                    e
                )
            });
        }
    }

    /// Generate and return the serialized bytes for all samples.
    /// Useful for writing fixture files.
    fn sample_bytes(seed: u64) -> Vec<Vec<u8>> {
        Self::samples(seed)
            .iter()
            .map(|s| bincode::serialize(s).expect("serialization should not fail"))
            .collect()
    }
}

/// Generate a deterministic 32-byte hash from a seed and an index.
/// Uses blake2b to ensure good distribution.
pub fn deterministic_hash(seed: u64, index: u32) -> [u8; 32] {
    use ckb_hash::blake2b_256;
    let mut input = Vec::with_capacity(12);
    input.extend_from_slice(&seed.to_le_bytes());
    input.extend_from_slice(&index.to_le_bytes());
    blake2b_256(&input)
}

/// Generate a deterministic secp256k1 secret key from a seed and index.
/// The result is guaranteed to be a valid secret key.
pub fn deterministic_seckey(seed: u64, index: u32) -> secp256k1::SecretKey {
    let mut hash = deterministic_hash(seed, index);
    // Ensure the hash is a valid secp256k1 secret key (non-zero, less than curve order).
    // Setting the first byte to a small nonzero value practically guarantees validity.
    hash[0] = ((hash[0] % 254) + 1) as u8;
    secp256k1::SecretKey::from_slice(&hash).expect("deterministic_seckey should always be valid")
}

/// Generate a deterministic Pubkey from a seed and index.
pub fn deterministic_pubkey(seed: u64, index: u32) -> crate::fiber::Pubkey {
    let sk = deterministic_seckey(seed, index);
    let pk = secp256k1::PublicKey::from_secret_key(secp256k1::SECP256K1, &sk);
    pk.into()
}

/// Generate a deterministic Privkey from a seed and index.
pub fn deterministic_privkey(seed: u64, index: u32) -> crate::fiber::Privkey {
    let sk = deterministic_seckey(seed, index);
    sk.into()
}

/// Generate a deterministic Hash256 from a seed and index.
pub fn deterministic_hash256(seed: u64, index: u32) -> fiber_types::Hash256 {
    deterministic_hash(seed, index).into()
}

/// Create a deterministic EcdsaSignature from a seed and index.
pub fn deterministic_ecdsa_signature(seed: u64, index: u32) -> crate::fiber::EcdsaSignature {
    use secp256k1::{Message, SECP256K1};
    let sk = deterministic_seckey(seed, index);
    let message = Message::from_digest_slice(&deterministic_hash(seed, index.wrapping_add(1000)))
        .expect("32 bytes");
    let signature = SECP256K1.sign_ecdsa(&message, &sk);
    crate::fiber::EcdsaSignature(signature)
}

/// Create a deterministic SchnorrSignature from a seed and index.
/// Uses `sign_schnorr_no_aux_rand` to avoid non-deterministic auxiliary randomness.
pub fn deterministic_schnorr_signature(seed: u64, index: u32) -> secp256k1::schnorr::Signature {
    use secp256k1::{Keypair, SECP256K1};
    let sk = deterministic_seckey(seed, index);
    let keypair = Keypair::from_secret_key(SECP256K1, &sk);
    let msg_bytes = deterministic_hash(seed, index.wrapping_add(2000));
    SECP256K1.sign_schnorr_no_aux_rand(&msg_bytes, &keypair)
}

/// Create a deterministic XOnlyPublicKey from a seed and index.
pub fn deterministic_xonly_pubkey(seed: u64, index: u32) -> secp256k1::XOnlyPublicKey {
    use secp256k1::{Keypair, SECP256K1};
    let sk = deterministic_seckey(seed, index);
    let keypair = Keypair::from_secret_key(SECP256K1, &sk);
    secp256k1::XOnlyPublicKey::from_keypair(&keypair).0
}

/// Create a deterministic OutPoint from a seed and index.
pub fn deterministic_outpoint(seed: u64, index: u32) -> ckb_types::packed::OutPoint {
    use ckb_types::prelude::Pack;
    let hash = deterministic_hash(seed, index);
    let tx_hash: ckb_types::packed::Byte32 = hash.pack();
    ckb_types::packed::OutPoint::new(tx_hash, 0)
}

/// Create a deterministic RecoverableSignature from a seed and index.
pub fn deterministic_recoverable_signature(
    seed: u64,
    index: u32,
) -> secp256k1::ecdsa::RecoverableSignature {
    use secp256k1::{Message, SECP256K1};
    let sk = deterministic_seckey(seed, index);
    let message = Message::from_digest_slice(&deterministic_hash(seed, index.wrapping_add(3000)))
        .expect("32 bytes");
    SECP256K1.sign_ecdsa_recoverable(&message, &sk)
}
