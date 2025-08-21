use crate::fiber::{
    hash_algorithm::HashAlgorithm,
    types::{AMPPaymentData, Hash256},
};
use bitcoin::hashes::{sha256::Hash as Sha256, Hash as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// AmpSecret represents an n-of-n sharing of a secret 32-byte value.
/// The secret can be recovered by XORing all n shares together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmpSecret([u8; 32]);

impl AmpSecret {
    /// Create a new AmpSecret from a 32-byte array
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create a zero AmpSecret
    pub fn zero() -> Self {
        Self([0u8; 32])
    }

    /// Generate a random AmpSecret
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// XOR this AmpSecret with another AmpSecret, storing the result in self
    pub fn xor_assign(&mut self, other: &AmpSecret) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a ^= b;
        }
    }

    /// XOR two shares and return the result
    pub fn xor(&self, other: &AmpSecret) -> AmpSecret {
        let mut result = *self;
        result.xor_assign(other);
        result
    }

    /// Get the underlying bytes
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to bytes
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// generate a random AmpSecret sequence
    pub fn gen_random_sequence(root: AmpSecret, n: u16) -> Vec<AmpSecret> {
        let mut shares: Vec<AmpSecret> = (0..n - 1).map(|_| AmpSecret::random()).collect();

        let mut final_secret = root;
        for share in &shares {
            final_secret.xor_assign(share);
        }
        shares.push(final_secret);
        shares
    }
}

impl From<[u8; 32]> for AmpSecret {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for AmpSecret {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Child is a payment hash and preimage pair derived from the root seed.
/// In addition to the derived values, a Child carries all information required in
/// the derivation apart from the root seed (unless n=1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmpChild {
    /// Preimage is the child payment preimage that can be used to settle the HTLC
    pub preimage: Hash256,

    /// Hash is the child payment hash that to be carried by the HTLC
    pub hash: Hash256,
}

impl AmpChild {
    pub fn new(preimage: Hash256, hash: Hash256) -> Self {
        Self { preimage, hash }
    }
}

/// DeriveChild computes the child preimage and child hash for a given (root, share, index) tuple.
/// The derivation is defined as:
///   child_preimage = SHA256(root || share || be16(index))
///   child_hash = SHA256(child_preimage)
pub fn derive_child(
    root: AmpSecret,
    data: AMPPaymentData,
    hash_algorithm: HashAlgorithm,
) -> AmpChild {
    let index_bytes = data.index.to_be_bytes();

    // Compute child_preimage as SHA256(root || share || child_index)
    let mut preimage_data = Vec::with_capacity(32 + 32 + 2);
    preimage_data.extend_from_slice(root.as_bytes());
    preimage_data.extend_from_slice(data.secret.as_bytes());
    preimage_data.extend_from_slice(&index_bytes);

    let preimage_hash = Sha256::hash(&preimage_data);
    let preimage: Hash256 = preimage_hash.to_byte_array().into();

    // this is the payment hash for HTLC
    let hash: Hash256 = hash_algorithm.hash(preimage.as_ref()).into();
    AmpChild::new(preimage, hash)
}

/// ReconstructChildren derives the set of children hashes and preimages from the
/// provided descriptors.
pub fn construct_amp_children(
    payment_data_vec: &[AMPPaymentData],
    hash_algorithm: HashAlgorithm,
) -> Vec<AmpChild> {
    if payment_data_vec.is_empty() {
        return Vec::new();
    }

    // Recompute the root by XORing the provided shares
    let mut root = AmpSecret::zero();
    for desc in payment_data_vec {
        root.xor_assign(&desc.secret);
    }

    // With the root computed, derive the child hashes and preimages
    payment_data_vec
        .iter()
        .map(|data| derive_child(root, data.clone(), hash_algorithm))
        .collect()
}
