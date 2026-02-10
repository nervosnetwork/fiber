/// `StoreSample` trait for generating deterministic sample data to test
/// store serialization compatibility and data migration correctness.
use serde::{Deserialize, Serialize};

mod sample_channel;
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
    fn sample_bytes(seed: u64) -> Vec<Vec<u8>> {
        Self::samples(seed)
            .iter()
            .map(|s| bincode::serialize(s).expect("serialization should not fail"))
            .collect()
    }
}

/// Generate a deterministic 32-byte hash from a seed and an index.
pub fn deterministic_hash(seed: u64, index: u32) -> [u8; 32] {
    use ckb_hash::blake2b_256;
    let mut input = Vec::with_capacity(12);
    input.extend_from_slice(&seed.to_le_bytes());
    input.extend_from_slice(&index.to_le_bytes());
    blake2b_256(&input)
}

/// Generate a deterministic secp256k1 secret key from a seed and index.
pub fn deterministic_seckey(seed: u64, index: u32) -> secp256k1::SecretKey {
    let mut hash = deterministic_hash(seed, index);
    hash[0] = ((hash[0] % 254) + 1) as u8;
    secp256k1::SecretKey::from_slice(&hash).expect("deterministic_seckey should always be valid")
}

/// Generate a deterministic Pubkey from a seed and index.
pub fn deterministic_pubkey(seed: u64, index: u32) -> crate::fiber::types::Pubkey {
    let sk = deterministic_seckey(seed, index);
    let pk = secp256k1::PublicKey::from_secret_key(crate::fiber::types::secp256k1_instance(), &sk);
    pk.into()
}

/// Generate a deterministic Privkey from a seed and index.
pub fn deterministic_privkey(seed: u64, index: u32) -> crate::fiber::types::Privkey {
    let sk = deterministic_seckey(seed, index);
    sk.into()
}

/// Generate a deterministic Hash256 from a seed and index.
pub fn deterministic_hash256(seed: u64, index: u32) -> crate::fiber::types::Hash256 {
    deterministic_hash(seed, index).into()
}

/// Create a deterministic EcdsaSignature from a seed and index.
pub fn deterministic_ecdsa_signature(seed: u64, index: u32) -> crate::fiber::types::EcdsaSignature {
    use secp256k1::Message;
    let sk = deterministic_seckey(seed, index);
    let message = Message::from_digest_slice(&deterministic_hash(seed, index.wrapping_add(1000)))
        .expect("32 bytes");
    let secp = crate::fiber::types::secp256k1_instance();
    let signature = secp.sign_ecdsa(&message, &sk);
    crate::fiber::types::EcdsaSignature(signature)
}

/// Create a deterministic SchnorrSignature from a seed and index.
pub fn deterministic_schnorr_signature(seed: u64, index: u32) -> secp256k1::schnorr::Signature {
    use secp256k1::Keypair;
    let sk = deterministic_seckey(seed, index);
    let secp = crate::fiber::types::secp256k1_instance();
    let keypair = Keypair::from_secret_key(secp, &sk);
    let msg_bytes = deterministic_hash(seed, index.wrapping_add(2000));
    secp.sign_schnorr_no_aux_rand(&msg_bytes, &keypair)
}

/// Create a deterministic XOnlyPublicKey from a seed and index.
pub fn deterministic_xonly_pubkey(seed: u64, index: u32) -> secp256k1::XOnlyPublicKey {
    use secp256k1::Keypair;
    let sk = deterministic_seckey(seed, index);
    let secp = crate::fiber::types::secp256k1_instance();
    let keypair = Keypair::from_secret_key(secp, &sk);
    secp256k1::XOnlyPublicKey::from_keypair(&keypair).0
}

/// Create a deterministic OutPoint from a seed and index.
pub fn deterministic_outpoint(seed: u64, index: u32) -> ckb_types::packed::OutPoint {
    use ckb_types::prelude::Pack;
    let hash = deterministic_hash(seed, index);
    let tx_hash: ckb_types::packed::Byte32 = hash.pack();
    ckb_types::packed::OutPoint::new(tx_hash, 0)
}
