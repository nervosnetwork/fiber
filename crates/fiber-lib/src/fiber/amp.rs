use crate::fiber::{hash_algorithm::HashAlgorithm, types::Hash256};
use bitcoin::hashes::{sha256::Hash as Sha256, Hash as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Share represents an n-of-n sharing of a secret 32-byte value.
/// The secret can be recovered by XORing all n shares together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share([u8; 32]);

impl Share {
    /// Create a new Share from a 32-byte array
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create a zero share
    pub fn zero() -> Self {
        Self([0u8; 32])
    }

    /// Generate a random share
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// XOR this share with another share, storing the result in self
    pub fn xor_assign(&mut self, other: &Share) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a ^= b;
        }
    }

    /// XOR two shares and return the result
    pub fn xor(&self, other: &Share) -> Share {
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
}

impl From<[u8; 32]> for Share {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Share {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for Share {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

/// ChildDesc contains the information necessary to derive a child hash/preimage
/// pair that is attached to a particular HTLC. This information will be known by
/// both the sender and receiver in the process of fulfilling an AMP payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildDesc {
    /// Share is one of n shares of the root seed. Once all n shares are
    /// known to the receiver, the Share will also provide entropy to the
    /// derivation of child hash and preimage.
    pub share: Share,

    /// Index is 16-bit value that can be used to derive up to 2^16 child
    /// hashes and preimages from a single Share. This allows the payment
    /// hashes sent over the network to be refreshed without needing to
    /// modify the Share.
    pub index: u16,
}

impl ChildDesc {
    pub fn new(share: Share, index: u16) -> Self {
        Self { share, index }
    }
}

/// Child is a payment hash and preimage pair derived from the root seed.
/// In addition to the derived values, a Child carries all information required in
/// the derivation apart from the root seed (unless n=1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Child {
    /// ChildDesc contains the data required to derive the child hash and preimage
    pub desc: ChildDesc,

    /// Preimage is the child payment preimage that can be used to settle the HTLC
    pub preimage: Hash256,

    /// Hash is the child payment hash that to be carried by the HTLC
    pub hash: Hash256,
}

impl Child {
    pub fn new(desc: ChildDesc, preimage: Hash256, hash: Hash256) -> Self {
        Self {
            desc,
            preimage,
            hash,
        }
    }

    pub fn share(&self) -> Share {
        self.desc.share
    }

    pub fn index(&self) -> u16 {
        self.desc.index
    }
}

impl fmt::Display for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "share={}, index={} -> preimage={}, hash={}",
            self.desc.share,
            self.desc.index,
            hex::encode(self.preimage.as_ref()),
            hex::encode(self.hash.as_ref())
        )
    }
}

/// DeriveChild computes the child preimage and child hash for a given (root, share, index) tuple.
/// The derivation is defined as:
///   child_preimage = SHA256(root || share || be16(index))
///   child_hash = SHA256(child_preimage)
pub fn derive_child(root: Share, desc: ChildDesc, hash_algorithm: HashAlgorithm) -> Child {
    // Serialize the child index in big-endian order
    let index_bytes = desc.index.to_be_bytes();

    // Compute child_preimage as SHA256(root || share || child_index)
    let mut preimage_data = Vec::with_capacity(32 + 32 + 2);
    preimage_data.extend_from_slice(root.as_bytes());
    preimage_data.extend_from_slice(desc.share.as_bytes());
    preimage_data.extend_from_slice(&index_bytes);

    let preimage_hash = Sha256::hash(&preimage_data);
    let preimage: Hash256 = preimage_hash.to_byte_array().into();

    // Compute child_hash as SHA256(child_preimage)
    let hash: Hash256 = hash_algorithm.hash(preimage.as_ref()).into();
    Child::new(desc, preimage, hash)
}

/// ReconstructChildren derives the set of children hashes and preimages from the
/// provided descriptors.
pub fn reconstruct_children(descs: &[ChildDesc], hash_algorithm: HashAlgorithm) -> Vec<Child> {
    if descs.is_empty() {
        return Vec::new();
    }

    // Recompute the root by XORing the provided shares
    let mut root = Share::zero();
    for desc in descs {
        root.xor_assign(&desc.share);
    }

    // With the root computed, derive the child hashes and preimages
    descs
        .iter()
        .map(|&desc| derive_child(root, desc, hash_algorithm))
        .collect()
}
