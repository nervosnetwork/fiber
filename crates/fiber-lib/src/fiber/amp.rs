use crate::fiber::types::Hash256;
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

    /// Index is 32-bit value that can be used to derive up to 2^32 child
    /// hashes and preimages from a single Share. This allows the payment
    /// hashes sent over the network to be refreshed without needing to
    /// modify the Share.
    pub index: u32,
}

impl ChildDesc {
    pub fn new(share: Share, index: u32) -> Self {
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

    pub fn index(&self) -> u32 {
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
///   child_preimage = SHA256(root || share || be32(index))
///   child_hash = SHA256(child_preimage)
pub fn derive_child(root: Share, desc: ChildDesc) -> Child {
    // Serialize the child index in big-endian order
    let index_bytes = desc.index.to_be_bytes();

    // Compute child_preimage as SHA256(root || share || child_index)
    let mut preimage_data = Vec::with_capacity(32 + 32 + 4);
    preimage_data.extend_from_slice(root.as_bytes());
    preimage_data.extend_from_slice(desc.share.as_bytes());
    preimage_data.extend_from_slice(&index_bytes);

    let preimage_hash = Sha256::hash(&preimage_data);
    let preimage = Hash256::from(preimage_hash.to_byte_array());

    // Compute child_hash as SHA256(child_preimage)
    let hash_hash = Sha256::hash(preimage.as_ref());
    let hash = Hash256::from(hash_hash.to_byte_array());

    Child::new(desc, preimage, hash)
}

/// ReconstructChildren derives the set of children hashes and preimages from the
/// provided descriptors.
pub fn reconstruct_children(descs: &[ChildDesc]) -> Vec<Child> {
    if descs.is_empty() {
        return Vec::new();
    }

    // Recompute the root by XORing the provided shares
    let mut root = Share::zero();
    for desc in descs {
        root.xor_assign(&desc.share);
    }

    // With the root computed, derive the child hashes and preimages
    descs.iter().map(|&desc| derive_child(root, desc)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_share_xor() {
        let share1 = Share::random();
        let share2 = Share::random();
        let result = share1.xor(&share2);

        // XOR is commutative
        assert_eq!(result, share2.xor(&share1));

        // XOR with self should be zero
        assert_eq!(share1.xor(&share1), Share::zero());
    }

    #[test]
    fn test_derive_child() {
        let root = Share::random();
        let share = Share::random();
        let index = 42;

        let desc = ChildDesc::new(share, index);
        let child = derive_child(root, desc);

        assert_eq!(child.desc.share, share);
        assert_eq!(child.desc.index, index);
        assert_ne!(child.preimage, Hash256::default());
        assert_ne!(child.hash, Hash256::default());

        // Deriving the same child should produce the same result
        let child2 = derive_child(root, desc);
        assert_eq!(child, child2);
    }

    #[test]
    fn test_different_indices_produce_different_children() {
        let root = Share::random();
        let share = Share::random();

        let child1 = derive_child(root, ChildDesc::new(share, 1));
        let child2 = derive_child(root, ChildDesc::new(share, 2));

        assert_ne!(child1.preimage, child2.preimage);
        assert_ne!(child1.hash, child2.hash);
    }

    #[test]
    fn test_reconstruct_children() {
        let root = Share::random();
        let share1 = Share::random();
        let share2 = root.xor(&share1); // share2 = root ^ share1

        let desc1 = ChildDesc::new(share1, 1);
        let desc2 = ChildDesc::new(share2, 2);

        let children = reconstruct_children(&[desc1, desc2]);

        assert_eq!(children.len(), 2);
        assert_eq!(children[0].desc, desc1);
        assert_eq!(children[1].desc, desc2);

        // Verify that the children are correctly derived from the reconstructed root
        let expected_child1 = derive_child(root, desc1);
        let expected_child2 = derive_child(root, desc2);

        assert_eq!(children[0], expected_child1);
        assert_eq!(children[1], expected_child2);
    }

    #[test]
    fn test_reconstruct_empty_children() {
        let children = reconstruct_children(&[]);
        assert!(children.is_empty());
    }

    #[test]
    fn test_child_display() {
        let root = Share::random();
        let share = Share::random();
        let desc = ChildDesc::new(share, 123);
        let child = derive_child(root, desc);

        let display_str = child.to_string();
        assert!(display_str.contains("share="));
        assert!(display_str.contains("index=123"));
        assert!(display_str.contains("preimage="));
        assert!(display_str.contains("hash="));
    }
}
