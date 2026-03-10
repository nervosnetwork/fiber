//! Layer 1: Basic primitive types — Hash256, Pubkey, Privkey, NodeId.

use crate::gen::fiber as molecule_fiber;
use crate::gen::gossip as molecule_gossip;
use crate::protocol::{EcdsaSignature, SchnorrSignature};
use crate::serde_utils::{SliceBase58, SliceHex, SliceHexNoPrefix};
use anyhow::anyhow;
use ckb_gen_types::packed::Byte32 as MByte32;
use ckb_types::H256;
use molecule::prelude::{Builder, Byte, Entity};
use musig2::secp::{Point, Scalar};
use secp256k1::PublicKey;
use secp256k1::SecretKey;
use secp256k1::XOnlyPublicKey;
use secp256k1::SECP256K1;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, IfIsHumanReadable};
use tracing::trace;

impl From<Hash256> for [u8; 32] {
    fn from(val: Hash256) -> Self {
        val.0
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

impl ::std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        write!(f, "{}", bs58::encode(&self.0).into_string())
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

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ChannelUpdateChannelFlags: u32 {
        const DISABLED = 1;
    }
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ChannelUpdateMessageFlags: u32 {
        const UPDATE_OF_NODE1 = 0;
        const UPDATE_OF_NODE2 = 1;
    }
}

/// A wrapper for secp256k1 secret key
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Privkey(pub SecretKey);

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
        Privkey(SecretKey::from_slice(&k).expect("Invalid secret key"))
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

impl From<Privkey> for SecretKey {
    fn from(pk: Privkey) -> Self {
        pk.0
    }
}

impl From<SecretKey> for Privkey {
    fn from(sk: SecretKey) -> Self {
        Self(sk)
    }
}

/// A 256-bit hash digest, used as identifier of channel, payment, transaction hash etc.
#[serde_as]
#[derive(Copy, Clone, Serialize, Deserialize, Hash, Eq, PartialEq, Default)]
pub struct Hash256(
    #[serde_as(as = "SliceHex")]
    [u8; 32],
);

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

impl From<&Hash256> for MByte32 {
    fn from(hash: &Hash256) -> Self {
        MByte32::from_slice(hash.0.as_ref()).expect("Byte32 from Hash256")
    }
}

impl From<Hash256> for MByte32 {
    fn from(hash: Hash256) -> Self {
        (&hash).into()
    }
}

impl From<&MByte32> for Hash256 {
    fn from(value: &MByte32) -> Self {
        Hash256(value.as_slice().try_into().expect("Hash256 from Byte32"))
    }
}

impl From<MByte32> for Hash256 {
    fn from(value: MByte32) -> Self {
        (&value).into()
    }
}

impl From<Hash256> for H256 {
    fn from(value: Hash256) -> Self {
        H256(value.0)
    }
}

impl From<H256> for Hash256 {
    fn from(value: H256) -> Self {
        Hash256(value.0)
    }
}

#[cfg(feature = "cch")]
impl From<lightning_invoice::Sha256> for Hash256 {
    fn from(value: lightning_invoice::Sha256) -> Self {
        use bitcoin::hashes::Hash as _;
        Hash256(*value.0.as_byte_array())
    }
}

#[cfg(feature = "cch")]
impl From<bitcoin::hashes::sha256::Hash> for Hash256 {
    fn from(value: bitcoin::hashes::sha256::Hash) -> Self {
        use bitcoin::hashes::Hash as _;
        Hash256(value.to_byte_array())
    }
}

impl TryFrom<&[u8]> for Hash256 {
    type Error = anyhow::Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() != 32 {
            return Err(anyhow!("Invalid hash length"));
        }
        let mut data = [0u8; 32];
        data.copy_from_slice(value);
        Ok(Hash256(data))
    }
}

pub(crate) fn u8_32_as_byte_32(value: &[u8; 32]) -> MByte32 {
    MByte32::from_slice(value.as_slice()).expect("[u8; 32] to Byte32")
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

impl Privkey {
    pub fn from_slice(key: &[u8]) -> Self {
        SecretKey::from_slice(key)
            .expect("Invalid secret key")
            .into()
    }

    pub fn pubkey(&self) -> Pubkey {
        Pubkey::from(self.0.public_key(SECP256K1))
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

    pub fn sign(&self, message: [u8; 32]) -> EcdsaSignature {
        let message = secp256k1::Message::from_digest(message);
        SECP256K1.sign_ecdsa(&message, &self.0).into()
    }

    pub fn x_only_pub_key(&self) -> XOnlyPublicKey {
        let secret_key = self.0;
        let keypair = secp256k1::Keypair::from_secret_key(SECP256K1, &secret_key);
        XOnlyPublicKey::from_keypair(&keypair).0
    }

    pub fn sign_schnorr(&self, message: [u8; 32]) -> SchnorrSignature {
        let secret_key = self.0;
        let keypair = secp256k1::Keypair::from_secret_key(SECP256K1, &secret_key);
        let sig = SECP256K1.sign_schnorr(&message, &keypair);
        trace!(
            "Schnorr signing message {:?} (pub key {:?}), Signature: {:?}",
            message,
            keypair.public_key(),
            &sig
        );
        sig.into()
    }
}

/// A compressed secp256k1 public key (33 bytes), used as the primary identity of a node.
/// In the RPC interface this value is exposed as fields such as `pubkey`.
/// It is serialized as a 66-character hex string (e.g. `"02aaaa..."`) in JSON.
#[serde_as]
#[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pubkey(
    #[serde_as(as = "IfIsHumanReadable<SliceHexNoPrefix, [_; 33]>")]
    pub [u8; 33],
);

impl std::fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display as 33-byte compressed public key hex (matches JSON format and internal storage)
        write!(f, "Pubkey({})", hex::encode(self.0))
    }
}

impl From<Pubkey> for Point {
    fn from(val: Pubkey) -> Self {
        let pk: PublicKey = val.into();
        pk.into()
    }
}

impl From<&Pubkey> for Point {
    fn from(val: &Pubkey) -> Self {
        let pk: PublicKey = val.into();
        pk.into()
    }
}

impl From<&Pubkey> for PublicKey {
    fn from(val: &Pubkey) -> Self {
        PublicKey::from_slice(&val.0)
            .expect("Pubkey should always contain valid serialized public key")
    }
}

impl From<Pubkey> for PublicKey {
    fn from(pk: Pubkey) -> Self {
        PublicKey::from_slice(&pk.0)
            .expect("Pubkey should always contain valid serialized public key")
    }
}

impl From<PublicKey> for Pubkey {
    fn from(pk: PublicKey) -> Pubkey {
        Pubkey(pk.serialize())
    }
}

impl From<Point> for Pubkey {
    fn from(point: Point) -> Self {
        PublicKey::from(point).into()
    }
}

const PUBKEY_SIZE: usize = 33;
impl Pubkey {
    pub const fn serialization_len() -> usize {
        PUBKEY_SIZE
    }

    pub fn serialize(&self) -> [u8; PUBKEY_SIZE] {
        self.0
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self, secp256k1::Error> {
        // Validate by parsing, then store the bytes directly
        let _ = PublicKey::from_slice(slice)?;
        let mut bytes = [0u8; PUBKEY_SIZE];
        bytes.copy_from_slice(slice);
        Ok(Pubkey(bytes))
    }

    pub fn tweak<I: Into<[u8; 32]>>(&self, scalar: I) -> Self {
        let scalar = scalar.into();
        let scalar = Scalar::from_slice(&scalar)
            .expect(format!("Value {:?} must be within secp256k1 scalar range. If you generated this value from hash function, then your hash function is busted.", &scalar).as_str());
        // Convert to Point, perform operation, then serialize back
        let result = Point::from(self) + scalar.base_point_mul();
        let point = result.not_inf().expect("valid public key");
        PublicKey::from(point).into()
    }
}
