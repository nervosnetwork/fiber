//! Layer 1: Basic primitive types — Hash256, Pubkey, Privkey, NodeId.

use crate::gen::fiber as molecule_fiber;
use crate::gen::gossip as molecule_gossip;
use crate::serde_utils::{SliceBase58, SliceHex, SliceHexNoPrefix};
use ckb_gen_types::packed::Byte32 as MByte32;
use ckb_types::H256;
use molecule::prelude::{Builder, Byte, Entity};
use musig2::secp::Scalar;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, IfIsHumanReadable};

// ============================================================
// Hash256
// ============================================================

/// A 256-bit hash digest, used as identifier of channel, payment, transaction hash etc.
#[serde_as]
#[derive(Copy, Clone, Serialize, Deserialize, Hash, Eq, PartialEq, Default)]
pub struct Hash256(#[serde_as(as = "SliceHex")] [u8; 32]);

impl From<[u8; 32]> for Hash256 {
    fn from(value: [u8; 32]) -> Self {
        Self(value)
    }
}

impl AsRef<[u8]> for Hash256 {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl TryFrom<&[u8]> for Hash256 {
    type Error = anyhow::Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() != 32 {
            return Err(anyhow::anyhow!("Invalid hash length"));
        }
        let mut data = [0u8; 32];
        data.copy_from_slice(value);
        Ok(Hash256(data))
    }
}

impl ::core::fmt::LowerHex for Hash256 {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        if f.alternate() {
            write!(f, "0x")?;
        }
        write!(f, "{}", hex::encode(self.0))
    }
}

impl ::core::fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "Hash256({:#x})", self)
    }
}

impl ::core::fmt::Display for Hash256 {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        let raw_data = hex::encode(self.0);
        write!(f, "Hash256(0x{})", raw_data)
    }
}

impl std::str::FromStr for Hash256 {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim_start_matches("0x");
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(anyhow::anyhow!("Invalid hash length"));
        }
        let mut data = [0u8; 32];
        data.copy_from_slice(&bytes);
        Ok(Hash256(data))
    }
}

impl From<Hash256> for Vec<u8> {
    fn from(val: Hash256) -> Self {
        val.0.to_vec()
    }
}

impl From<Hash256> for [u8; 32] {
    fn from(val: Hash256) -> Self {
        val.0
    }
}

// Hash256 <-> molecule Byte32

impl From<Hash256> for MByte32 {
    fn from(hash: Hash256) -> Self {
        MByte32::from_slice(hash.0.as_slice()).expect("Byte32 from Hash256")
    }
}

impl From<&Hash256> for MByte32 {
    fn from(hash: &Hash256) -> Self {
        MByte32::from_slice(hash.0.as_slice()).expect("Byte32 from Hash256")
    }
}

impl From<MByte32> for Hash256 {
    fn from(byte32: MByte32) -> Self {
        let bytes: [u8; 32] = byte32.as_slice().try_into().expect("Hash256 from Byte32");
        Hash256(bytes)
    }
}

impl From<&MByte32> for Hash256 {
    fn from(byte32: &MByte32) -> Self {
        let bytes: [u8; 32] = byte32.as_slice().try_into().expect("Hash256 from Byte32");
        Hash256(bytes)
    }
}

// Hash256 <-> ckb_types::H256

impl From<Hash256> for H256 {
    fn from(hash: Hash256) -> Self {
        H256(hash.0)
    }
}

impl From<H256> for Hash256 {
    fn from(h256: H256) -> Self {
        Hash256(h256.0)
    }
}

// ============================================================
// Pubkey
// ============================================================

const PUBKEY_SIZE: usize = 33;

/// The public key for a Node.
/// It stores the serialized form ([u8; 33]) directly for fast comparison and hashing.
#[serde_as]
#[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pubkey(#[serde_as(as = "IfIsHumanReadable<SliceHexNoPrefix, [_; 33]>")] pub [u8; 33]);

impl std::fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pubkey({})", hex::encode(self.0))
    }
}

impl std::fmt::Display for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl Pubkey {
    pub const fn serialization_len() -> usize {
        PUBKEY_SIZE
    }

    pub fn serialize(&self) -> [u8; PUBKEY_SIZE] {
        self.0
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self, secp256k1::Error> {
        // Validate by parsing, then store the bytes directly
        let _ = secp256k1::PublicKey::from_slice(slice)?;
        let mut bytes = [0u8; PUBKEY_SIZE];
        bytes.copy_from_slice(slice);
        Ok(Pubkey(bytes))
    }
}

// Pubkey <-> secp256k1::PublicKey

impl From<secp256k1::PublicKey> for Pubkey {
    fn from(pk: secp256k1::PublicKey) -> Pubkey {
        Pubkey(pk.serialize())
    }
}

impl From<Pubkey> for secp256k1::PublicKey {
    fn from(pk: Pubkey) -> Self {
        secp256k1::PublicKey::from_slice(&pk.0)
            .expect("Pubkey should always contain valid serialized public key")
    }
}

impl From<&Pubkey> for secp256k1::PublicKey {
    fn from(val: &Pubkey) -> Self {
        secp256k1::PublicKey::from_slice(&val.0)
            .expect("Pubkey should always contain valid serialized public key")
    }
}

// Pubkey <-> molecule_fiber::Pubkey

impl From<Pubkey> for molecule_fiber::Pubkey {
    fn from(pk: Pubkey) -> Self {
        molecule_fiber::Pubkey::new_builder()
            .set(
                pk.0.into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Public serialized to correct length"),
            )
            .build()
    }
}

impl From<&Pubkey> for molecule_fiber::Pubkey {
    fn from(pk: &Pubkey) -> Self {
        molecule_fiber::Pubkey::new_builder()
            .set(
                pk.0.into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Public serialized to correct length"),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::Pubkey> for Pubkey {
    type Error = secp256k1::Error;

    fn try_from(pubkey: molecule_fiber::Pubkey) -> Result<Self, Self::Error> {
        Pubkey::from_slice(pubkey.as_slice())
    }
}

// ============================================================
// Privkey
// ============================================================

/// A wrapper for secp256k1 secret key.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Privkey(pub secp256k1::SecretKey);

impl Privkey {
    pub fn from_slice(key: &[u8]) -> Self {
        secp256k1::SecretKey::from_slice(key)
            .expect("Invalid secret key")
            .into()
    }

    pub fn pubkey(&self) -> Pubkey {
        Pubkey::from(self.0.public_key(secp256k1::SECP256K1))
    }

    pub fn tweak<I: Into<[u8; 32]>>(&self, scalar: I) -> Self {
        let scalar = scalar.into();
        let scalar = Scalar::from_slice(&scalar)
            .expect(format!("Value {:?} must be within secp256k1 scalar range. If you generated this value from hash function, then your hash function is busted.", &scalar).as_str());
        let sk = Scalar::from(self);
        (scalar + sk)
            .not_zero()
            .expect("valid secp256k1 scalar addition")
            .into()
    }

    pub fn x_only_pub_key(&self) -> secp256k1::XOnlyPublicKey {
        let secret_key = self.0;
        let keypair = secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &secret_key);
        secp256k1::XOnlyPublicKey::from_keypair(&keypair).0
    }
}

impl From<Privkey> for Scalar {
    fn from(pk: Privkey) -> Self {
        pk.0.into()
    }
}

impl From<&Privkey> for Scalar {
    fn from(pk: &Privkey) -> Self {
        pk.0.into()
    }
}

impl From<[u8; 32]> for Privkey {
    fn from(k: [u8; 32]) -> Self {
        Privkey(secp256k1::SecretKey::from_slice(&k).expect("Invalid secret key"))
    }
}

impl From<Scalar> for Privkey {
    fn from(scalar: Scalar) -> Self {
        scalar.serialize().into()
    }
}

impl From<Hash256> for Privkey {
    fn from(hash: Hash256) -> Self {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(hash.as_ref());
        Privkey::from_slice(&bytes)
    }
}

impl From<Privkey> for secp256k1::SecretKey {
    fn from(pk: Privkey) -> Self {
        pk.0
    }
}

impl From<secp256k1::SecretKey> for Privkey {
    fn from(sk: secp256k1::SecretKey) -> Self {
        Self(sk)
    }
}

impl From<&[u8; 32]> for Privkey {
    fn from(k: &[u8; 32]) -> Self {
        Self::from_slice(k)
    }
}

impl AsRef<[u8; 32]> for Privkey {
    /// Gets a reference to the underlying array.
    ///
    /// # Side channel attacks
    ///
    /// Using ordering functions (`PartialOrd`/`Ord`) on a reference to secret keys leaks data
    /// because the implementations are not constant time. Doing so will make your code vulnerable
    /// to side channel attacks. [`SecretKey::eq`] is implemented using a constant time algorithm,
    /// please consider using it to do comparisons of secret keys.
    #[inline]
    fn as_ref(&self) -> &[u8; 32] {
        self.0.as_ref()
    }
}

// ============================================================
// NodeId
// ============================================================

/// Used as identifier of node.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct NodeId(#[serde_as(as = "SliceBase58")] Vec<u8>);

impl NodeId {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Return an empty NodeId representing the local node.
    pub fn local() -> Self {
        Self(Default::default())
    }
}

impl AsRef<[u8]> for NodeId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl ::std::str::FromStr for NodeId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = bs58::decode(s)
            .into_vec()
            .map_err(|_| anyhow::anyhow!("can't parse node_id: {s}"))?;

        Ok(Self::from_bytes(bytes))
    }
}

// ============================================================
// Molecule conversions for external crate types
// ============================================================

// u16 <-> molecule_fiber::Uint16

impl From<u16> for molecule_fiber::Uint16 {
    fn from(count: u16) -> Self {
        let le_bytes = count.to_le_bytes();
        Self::new_builder()
            .set(
                le_bytes
                    .into_iter()
                    .map(Byte::new)
                    .collect::<Vec<_>>()
                    .try_into()
                    .expect("Uint16 from u16"),
            )
            .build()
    }
}

impl From<molecule_fiber::Uint16> for u16 {
    fn from(count: molecule_fiber::Uint16) -> Self {
        let le_bytes = count.as_slice().try_into().expect("Uint16 to u16");
        u16::from_le_bytes(le_bytes)
    }
}

// secp256k1::XOnlyPublicKey <-> molecule_gossip::SchnorrXOnlyPubkey

impl From<secp256k1::XOnlyPublicKey> for molecule_gossip::SchnorrXOnlyPubkey {
    fn from(pk: secp256k1::XOnlyPublicKey) -> molecule_gossip::SchnorrXOnlyPubkey {
        molecule_gossip::SchnorrXOnlyPubkey::new_builder()
            .set(
                pk.serialize()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Public serialized to correct length"),
            )
            .build()
    }
}

impl TryFrom<molecule_gossip::SchnorrXOnlyPubkey> for secp256k1::XOnlyPublicKey {
    type Error = secp256k1::Error;

    fn try_from(pubkey: molecule_gossip::SchnorrXOnlyPubkey) -> Result<Self, Self::Error> {
        secp256k1::XOnlyPublicKey::from_slice(pubkey.as_slice())
    }
}

// secp256k1::schnorr::Signature <-> molecule_gossip::SchnorrSignature

impl From<secp256k1::schnorr::Signature> for molecule_gossip::SchnorrSignature {
    fn from(signature: secp256k1::schnorr::Signature) -> molecule_gossip::SchnorrSignature {
        molecule_gossip::SchnorrSignature::new_builder()
            .set(
                signature
                    .to_byte_array()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Signature serialized to correct length"),
            )
            .build()
    }
}

impl TryFrom<molecule_gossip::SchnorrSignature> for secp256k1::schnorr::Signature {
    type Error = secp256k1::Error;

    fn try_from(signature: molecule_gossip::SchnorrSignature) -> Result<Self, Self::Error> {
        secp256k1::schnorr::Signature::from_slice(signature.as_slice())
    }
}
