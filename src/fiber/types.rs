use std::str::FromStr;

use super::channel::ChannelFlags;
use super::config::AnnouncedNodeName;
use super::gen::fiber::{self as molecule_fiber, PubNonce as Byte66};
use super::hash_algorithm::{HashAlgorithm, UnknownHashAlgorithmError};
use super::r#gen::fiber::PubNonceOpt;
use super::serde_utils::{EntityHex, SliceHex};
use anyhow::anyhow;
use ckb_sdk::{Since, SinceType};
use ckb_types::core::FeeRate;
use ckb_types::packed::{OutPoint, Uint64};
use ckb_types::{
    packed::{Byte32 as MByte32, BytesVec, Script, Transaction},
    prelude::{Pack, Unpack},
};
use molecule::prelude::{Builder, Byte, Entity};
use musig2::errors::DecodeError;
use musig2::secp::{Point, Scalar};
use musig2::{BinaryEncoding, PartialSignature, PubNonce};
use once_cell::sync::OnceCell;
use secp256k1::XOnlyPublicKey;
use secp256k1::{
    ecdsa::Signature as Secp256k1Signature, schnorr::Signature as SchnorrSignature, All, PublicKey,
    Secp256k1, SecretKey,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tentacle::multiaddr::MultiAddr;
use tentacle::secio::PeerId;
use thiserror::Error;
use tracing::debug;

pub fn secp256k1_instance() -> &'static Secp256k1<All> {
    static INSTANCE: OnceCell<Secp256k1<All>> = OnceCell::new();
    INSTANCE.get_or_init(Secp256k1::new)
}

// TODO: We actually use both relative
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LockTime(u64);

impl LockTime {
    pub fn new(blocks: u64) -> Self {
        LockTime(blocks)
    }
}

impl From<LockTime> for Since {
    fn from(lock_time: LockTime) -> Since {
        Since::new(SinceType::BlockNumber, lock_time.0, true)
    }
}

impl TryFrom<Since> for LockTime {
    type Error = Error;

    fn try_from(since: Since) -> Result<Self, Self::Error> {
        if !since.is_relative() {
            return Err(Error::from(anyhow!(
                "Invalid lock time type: must be relative"
            )));
        }
        since
            .extract_metric()
            .map(|(ty, value)| {
                if ty == SinceType::BlockNumber {
                    Ok(LockTime(value))
                } else {
                    Err(Error::from(anyhow!(
                        "Invalid lock time type: must be blocknumber"
                    )))
                }
            })
            .unwrap_or_else(|| {
                Err(Error::from(anyhow!(
                    "Invalid lock time type: unable to extract metric"
                )))
            })
    }
}

impl From<LockTime> for Uint64 {
    fn from(lock_time: LockTime) -> Uint64 {
        let b: [u8; 8] = lock_time.into();
        Uint64::from_slice(&b).expect("valid locktime serialized to 8 bytes")
    }
}

impl TryFrom<Uint64> for LockTime {
    type Error = Error;

    fn try_from(value: Uint64) -> Result<LockTime, Error> {
        let b = value.as_slice();
        LockTime::try_from(b)
    }
}

impl From<u64> for LockTime {
    fn from(value: u64) -> LockTime {
        LockTime(value)
    }
}

impl From<LockTime> for u64 {
    fn from(lock_time: LockTime) -> u64 {
        lock_time.0
    }
}

impl From<[u8; 8]> for LockTime {
    fn from(value: [u8; 8]) -> LockTime {
        LockTime(u64::from_le_bytes(value))
    }
}

impl From<LockTime> for [u8; 8] {
    fn from(lock_time: LockTime) -> [u8; 8] {
        lock_time.0.to_le_bytes()
    }
}

impl TryFrom<&[u8]> for LockTime {
    type Error = Error;

    fn try_from(value: &[u8]) -> Result<LockTime, Error> {
        if value.len() != 8 {
            return Err(Error::from(anyhow!("Invalid lock time length")));
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(value);
        Ok(LockTime::from(bytes))
    }
}

impl From<&Byte66> for PubNonce {
    fn from(value: &Byte66) -> Self {
        PubNonce::from_bytes(value.as_slice()).unwrap()
    }
}

impl From<&PubNonce> for Byte66 {
    fn from(value: &PubNonce) -> Self {
        Byte66::from_slice(&value.to_bytes()).expect("valid pubnonce serialized to 66 bytes")
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Debug)]
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

impl From<&Hash256> for MByte32 {
    fn from(hash: &Hash256) -> Self {
        MByte32::new_builder()
            .set(
                hash.0
                    .into_iter()
                    .map(Byte::new)
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap(),
            )
            .build()
    }
}

impl From<Hash256> for MByte32 {
    fn from(hash: Hash256) -> Self {
        (&hash).into()
    }
}

impl From<&MByte32> for Hash256 {
    fn from(value: &MByte32) -> Self {
        Hash256(value.as_bytes().to_vec().try_into().unwrap())
    }
}

impl From<MByte32> for Hash256 {
    fn from(value: MByte32) -> Self {
        (&value).into()
    }
}

fn u8_32_as_byte_32(value: &[u8; 32]) -> MByte32 {
    MByte32::new_builder()
        .set(
            value
                .iter()
                .map(|v| Byte::new(*v))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        )
        .build()
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

impl FromStr for Hash256 {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim_start_matches("0x");
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(anyhow!("Invalid hash length"));
        }
        let mut data = [0u8; 32];
        data.copy_from_slice(&bytes);
        Ok(Hash256(data))
    }
}

impl Privkey {
    pub fn from_slice(key: &[u8]) -> Self {
        SecretKey::from_slice(key)
            .expect("Invalid secret key")
            .into()
    }

    pub fn pubkey(&self) -> Pubkey {
        Pubkey::from(self.0.public_key(secp256k1_instance()))
    }

    pub fn tweak<I: Into<[u8; 32]>>(&self, scalar: I) -> Self {
        let scalar = scalar.into();
        let scalar = Scalar::from_slice(&scalar)
            .expect(format!("Value {:?} must be within secp256k1 scalar range. If you generated this value from hash function, then your hash function is busted.", &scalar).as_str());
        let sk = Scalar::from(self);
        (scalar + sk).unwrap().into()
    }

    // Essentially https://docs.rs/ckb-crypto/latest/ckb_crypto/secp/struct.Privkey.html#method.sign_recoverable
    // But we don't want to depend on ckb-crypto because ckb-crypto depends on
    // a different version of secp256k1.
    pub fn sign_ecdsa_recoverable(&self, message: &[u8; 32]) -> [u8; 65] {
        tracing::debug!(
            "Signing message with private key {:?}, public key: {:?}, pubkey hash: {:?},  message {:?}",
            hex::encode(self.as_ref()),
            self.pubkey(),
            hex::encode(ckb_hash::blake2b_256(self.pubkey().serialize())),
            hex::encode(message)
        );
        let (rec_id, data) = secp256k1_instance()
            .sign_ecdsa_recoverable(&secp256k1::Message::from_digest(*message), &self.0)
            .serialize_compact();
        let mut result = [0; 65];
        result[0..64].copy_from_slice(data.as_slice());
        result[64] = rec_id.to_i32() as u8;
        result
    }

    pub fn sign(&self, message: [u8; 32]) -> EcdsaSignature {
        let message = secp256k1::Message::from_digest(message);
        let sig = secp256k1_instance().sign_ecdsa(&message, &self.0);
        debug!(
            "Signing message {:?} with private key {:?} (pub key {:?}), Signature: {:?}",
            message,
            self,
            self.pubkey(),
            EcdsaSignature::from(sig)
        );
        EcdsaSignature::from(sig)
    }
}

#[derive(Copy, Clone, Debug, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pubkey(pub PublicKey);

impl From<Pubkey> for Point {
    fn from(val: Pubkey) -> Self {
        PublicKey::from(val).into()
    }
}

impl From<&Pubkey> for Point {
    fn from(val: &Pubkey) -> Self {
        (*val).into()
    }
}

impl From<&Pubkey> for PublicKey {
    fn from(val: &Pubkey) -> Self {
        val.0
    }
}

impl From<Pubkey> for PublicKey {
    fn from(pk: Pubkey) -> Self {
        pk.0
    }
}

impl From<PublicKey> for Pubkey {
    fn from(pk: PublicKey) -> Pubkey {
        Pubkey(pk)
    }
}

impl From<Point> for Pubkey {
    fn from(point: Point) -> Self {
        PublicKey::from(point).into()
    }
}

impl From<tentacle::secio::PublicKey> for Pubkey {
    fn from(pk: tentacle::secio::PublicKey) -> Self {
        secp256k1::PublicKey::from_slice(pk.inner_ref())
            .expect("valid tentacle pubkey can be converted to secp pubkey")
            .into()
    }
}

impl From<Pubkey> for tentacle::secio::PublicKey {
    fn from(pk: Pubkey) -> Self {
        tentacle::secio::PublicKey::from_raw_key(pk.serialize().to_vec())
    }
}

impl Pubkey {
    pub fn serialize(&self) -> [u8; 33] {
        PublicKey::from(self).serialize()
    }

    pub fn tweak<I: Into<[u8; 32]>>(&self, scalar: I) -> Self {
        let scalar = scalar.into();
        let scalar = Scalar::from_slice(&scalar)
            .expect(format!("Value {:?} must be within secp256k1 scalar range. If you generated this value from hash function, then your hash function is busted.", &scalar).as_str());
        let result = Point::from(self) + scalar.base_point_mul();
        PublicKey::from(result.unwrap()).into()
    }

    pub fn tentacle_peer_id(&self) -> PeerId {
        let pubkey = self.clone().into();
        PeerId::from_public_key(&pubkey)
    }
}

#[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct EcdsaSignature(pub Secp256k1Signature);

impl EcdsaSignature {
    pub fn verify(&self, pubkey: &Pubkey, message: &[u8; 32]) -> bool {
        let message = secp256k1::Message::from_digest(*message);
        debug!(
            "Verifying message {:?} with pubkey {:?} and signature {:?}",
            message, pubkey, self
        );
        secp256k1_instance()
            .verify_ecdsa(&message, &self.0, &pubkey.0)
            .is_ok()
    }
}

impl From<EcdsaSignature> for Secp256k1Signature {
    fn from(sig: EcdsaSignature) -> Self {
        sig.0
    }
}

impl From<Secp256k1Signature> for EcdsaSignature {
    fn from(sig: Secp256k1Signature) -> Self {
        Self(sig)
    }
}

/// The error type wrap various ser/de errors.
#[derive(Error, Debug)]
pub enum Error {
    /// Invalid pubkey/signature format
    #[error("Secp error: {0}")]
    Secp(#[from] secp256k1::Error),
    #[error("Molecule error: {0}")]
    Molecule(#[from] molecule::error::VerificationError),
    #[error("Musig2 error: {0}")]
    Musig2(String),
    #[error("Error: {0}")]
    AnyHow(#[from] anyhow::Error),
}

impl From<Pubkey> for molecule_fiber::Pubkey {
    fn from(pk: Pubkey) -> molecule_fiber::Pubkey {
        molecule_fiber::Pubkey::new_builder()
            .set(
                pk.0.serialize()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Public serialized to corrent length"),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::Pubkey> for Pubkey {
    type Error = Error;

    fn try_from(pubkey: molecule_fiber::Pubkey) -> Result<Self, Self::Error> {
        let pubkey = pubkey.as_slice();
        PublicKey::from_slice(pubkey)
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<EcdsaSignature> for molecule_fiber::EcdsaSignature {
    fn from(signature: EcdsaSignature) -> molecule_fiber::EcdsaSignature {
        molecule_fiber::EcdsaSignature::new_builder()
            .set(
                signature
                    .0
                    .serialize_compact()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Signature serialized to corrent length"),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::EcdsaSignature> for EcdsaSignature {
    type Error = Error;

    fn try_from(signature: molecule_fiber::EcdsaSignature) -> Result<Self, Self::Error> {
        let signature = signature.raw_data();
        Secp256k1Signature::from_compact(&signature)
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<XOnlyPublicKey> for molecule_fiber::SchnorrXOnlyPubkey {
    fn from(pk: XOnlyPublicKey) -> molecule_fiber::SchnorrXOnlyPubkey {
        molecule_fiber::SchnorrXOnlyPubkey::new_builder()
            .set(
                pk.serialize()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Public serialized to corrent length"),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::SchnorrXOnlyPubkey> for XOnlyPublicKey {
    type Error = Error;

    fn try_from(pubkey: molecule_fiber::SchnorrXOnlyPubkey) -> Result<Self, Self::Error> {
        let pubkey = pubkey.as_slice();
        XOnlyPublicKey::from_slice(pubkey).map_err(Into::into)
    }
}

impl From<SchnorrSignature> for molecule_fiber::SchnorrSignature {
    fn from(signature: SchnorrSignature) -> molecule_fiber::SchnorrSignature {
        molecule_fiber::SchnorrSignature::new_builder()
            .set(
                signature
                    .serialize()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<Byte>>()
                    .try_into()
                    .expect("Signature serialized to corrent length"),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::SchnorrSignature> for SchnorrSignature {
    type Error = Error;

    fn try_from(signature: molecule_fiber::SchnorrSignature) -> Result<Self, Self::Error> {
        let signature = signature.as_slice();
        SchnorrSignature::from_slice(signature)
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl TryFrom<Byte66> for PubNonce {
    type Error = DecodeError<Self>;

    fn try_from(value: Byte66) -> Result<Self, Self::Error> {
        PubNonce::from_bytes(value.as_slice())
    }
}

#[derive(Clone, Debug)]
pub struct OpenChannel {
    pub chain_hash: Hash256,
    pub channel_id: Hash256,
    pub funding_udt_type_script: Option<Script>,
    pub funding_amount: u128,
    pub reserved_ckb_amount: u64,
    pub funding_fee_rate: u64,
    pub commitment_fee_rate: u64,
    pub max_tlc_value_in_flight: u128,
    pub max_num_of_accept_tlcs: u64,
    pub min_tlc_value: u128,
    pub to_local_delay: LockTime,
    pub funding_pubkey: Pubkey,
    pub revocation_basepoint: Pubkey,
    pub payment_basepoint: Pubkey,
    pub delayed_payment_basepoint: Pubkey,
    pub tlc_basepoint: Pubkey,
    pub first_per_commitment_point: Pubkey,
    pub second_per_commitment_point: Pubkey,
    pub channel_announcement_nonce: Option<PubNonce>,
    pub next_local_nonce: PubNonce,
    pub channel_flags: ChannelFlags,
}

impl OpenChannel {
    pub fn all_ckb_amount(&self) -> u64 {
        if self.funding_udt_type_script.is_none() {
            self.funding_amount as u64 + self.reserved_ckb_amount
        } else {
            self.reserved_ckb_amount
        }
    }
}

impl From<OpenChannel> for molecule_fiber::OpenChannel {
    fn from(open_channel: OpenChannel) -> Self {
        molecule_fiber::OpenChannel::new_builder()
            .chain_hash(open_channel.chain_hash.into())
            .channel_id(open_channel.channel_id.into())
            .funding_udt_type_script(open_channel.funding_udt_type_script.pack())
            .funding_amount(open_channel.funding_amount.pack())
            .reserved_ckb_amount(open_channel.reserved_ckb_amount.pack())
            .funding_fee_rate(open_channel.funding_fee_rate.pack())
            .commitment_fee_rate(open_channel.commitment_fee_rate.pack())
            .max_tlc_value_in_flight(open_channel.max_tlc_value_in_flight.pack())
            .max_num_of_accept_tlcs(open_channel.max_num_of_accept_tlcs.pack())
            .min_tlc_value(open_channel.min_tlc_value.pack())
            .to_self_delay(open_channel.to_local_delay.into())
            .funding_pubkey(open_channel.funding_pubkey.into())
            .revocation_basepoint(open_channel.revocation_basepoint.into())
            .payment_basepoint(open_channel.payment_basepoint.into())
            .delayed_payment_basepoint(open_channel.delayed_payment_basepoint.into())
            .tlc_basepoint(open_channel.tlc_basepoint.into())
            .first_per_commitment_point(open_channel.first_per_commitment_point.into())
            .second_per_commitment_point(open_channel.second_per_commitment_point.into())
            .next_local_nonce((&open_channel.next_local_nonce).into())
            .channel_annoucement_nonce(
                PubNonceOpt::new_builder()
                    .set(open_channel.channel_announcement_nonce.map(|x| (&x).into()))
                    .build(),
            )
            .channel_flags(open_channel.channel_flags.bits().into())
            .build()
    }
}

impl TryFrom<molecule_fiber::OpenChannel> for OpenChannel {
    type Error = Error;

    fn try_from(open_channel: molecule_fiber::OpenChannel) -> Result<Self, Self::Error> {
        Ok(OpenChannel {
            chain_hash: open_channel.chain_hash().into(),
            channel_id: open_channel.channel_id().into(),
            funding_udt_type_script: open_channel.funding_udt_type_script().to_opt(),
            funding_amount: open_channel.funding_amount().unpack(),
            reserved_ckb_amount: open_channel.reserved_ckb_amount().unpack(),
            funding_fee_rate: open_channel.funding_fee_rate().unpack(),
            commitment_fee_rate: open_channel.commitment_fee_rate().unpack(),
            max_tlc_value_in_flight: open_channel.max_tlc_value_in_flight().unpack(),
            max_num_of_accept_tlcs: open_channel.max_num_of_accept_tlcs().unpack(),
            min_tlc_value: open_channel.min_tlc_value().unpack(),
            to_local_delay: open_channel.to_self_delay().try_into()?,
            funding_pubkey: open_channel.funding_pubkey().try_into()?,
            revocation_basepoint: open_channel.revocation_basepoint().try_into()?,
            payment_basepoint: open_channel.payment_basepoint().try_into()?,
            delayed_payment_basepoint: open_channel.delayed_payment_basepoint().try_into()?,
            tlc_basepoint: open_channel.tlc_basepoint().try_into()?,
            first_per_commitment_point: open_channel.first_per_commitment_point().try_into()?,
            second_per_commitment_point: open_channel.second_per_commitment_point().try_into()?,
            next_local_nonce: open_channel
                .next_local_nonce()
                .try_into()
                .map_err(|err| Error::Musig2(format!("{err}")))?,
            channel_announcement_nonce: open_channel
                .channel_annoucement_nonce()
                .to_opt()
                .map(TryInto::try_into)
                .transpose()
                .map_err(|err| Error::Musig2(format!("{err}")))?,
            channel_flags: ChannelFlags::from_bits(open_channel.channel_flags().into()).ok_or(
                anyhow!("Invalid channel flags: {}", open_channel.channel_flags()),
            )?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AcceptChannel {
    pub channel_id: Hash256,
    pub funding_amount: u128,
    pub reserved_ckb_amount: u64,
    pub max_tlc_value_in_flight: u128,
    pub max_num_of_accept_tlcs: u64,
    pub min_tlc_value: u128,
    pub to_local_delay: LockTime,
    pub funding_pubkey: Pubkey,
    pub revocation_basepoint: Pubkey,
    pub payment_basepoint: Pubkey,
    pub delayed_payment_basepoint: Pubkey,
    pub tlc_basepoint: Pubkey,
    pub first_per_commitment_point: Pubkey,
    pub second_per_commitment_point: Pubkey,
    pub channel_announcement_nonce: Option<PubNonce>,
    pub next_local_nonce: PubNonce,
}

impl From<AcceptChannel> for molecule_fiber::AcceptChannel {
    fn from(accept_channel: AcceptChannel) -> Self {
        molecule_fiber::AcceptChannel::new_builder()
            .channel_id(accept_channel.channel_id.into())
            .funding_amount(accept_channel.funding_amount.pack())
            .reserved_ckb_amount(accept_channel.reserved_ckb_amount.pack())
            .max_tlc_value_in_flight(accept_channel.max_tlc_value_in_flight.pack())
            .max_num_of_accept_tlcs(accept_channel.max_num_of_accept_tlcs.pack())
            .min_tlc_value(accept_channel.min_tlc_value.pack())
            .to_self_delay(accept_channel.to_local_delay.into())
            .funding_pubkey(accept_channel.funding_pubkey.into())
            .revocation_basepoint(accept_channel.revocation_basepoint.into())
            .payment_basepoint(accept_channel.payment_basepoint.into())
            .delayed_payment_basepoint(accept_channel.delayed_payment_basepoint.into())
            .tlc_basepoint(accept_channel.tlc_basepoint.into())
            .first_per_commitment_point(accept_channel.first_per_commitment_point.into())
            .second_per_commitment_point(accept_channel.second_per_commitment_point.into())
            .channel_annoucement_nonce(
                PubNonceOpt::new_builder()
                    .set(
                        accept_channel
                            .channel_announcement_nonce
                            .map(|x| (&x).into()),
                    )
                    .build(),
            )
            .next_local_nonce((&accept_channel.next_local_nonce).into())
            .build()
    }
}

impl TryFrom<molecule_fiber::AcceptChannel> for AcceptChannel {
    type Error = Error;

    fn try_from(accept_channel: molecule_fiber::AcceptChannel) -> Result<Self, Self::Error> {
        Ok(AcceptChannel {
            channel_id: accept_channel.channel_id().into(),
            funding_amount: accept_channel.funding_amount().unpack(),
            reserved_ckb_amount: accept_channel.reserved_ckb_amount().unpack(),
            max_tlc_value_in_flight: accept_channel.max_tlc_value_in_flight().unpack(),
            max_num_of_accept_tlcs: accept_channel.max_num_of_accept_tlcs().unpack(),
            min_tlc_value: accept_channel.min_tlc_value().unpack(),
            to_local_delay: accept_channel.to_self_delay().try_into()?,
            funding_pubkey: accept_channel.funding_pubkey().try_into()?,
            revocation_basepoint: accept_channel.revocation_basepoint().try_into()?,
            payment_basepoint: accept_channel.payment_basepoint().try_into()?,
            delayed_payment_basepoint: accept_channel.delayed_payment_basepoint().try_into()?,
            tlc_basepoint: accept_channel.tlc_basepoint().try_into()?,
            first_per_commitment_point: accept_channel.first_per_commitment_point().try_into()?,
            second_per_commitment_point: accept_channel.second_per_commitment_point().try_into()?,
            channel_announcement_nonce: accept_channel
                .channel_annoucement_nonce()
                .to_opt()
                .map(TryInto::try_into)
                .transpose()
                .map_err(|err| Error::Musig2(format!("{err}")))?,
            next_local_nonce: accept_channel
                .next_local_nonce()
                .try_into()
                .map_err(|err| Error::Musig2(format!("{err}")))?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitmentSigned {
    pub channel_id: Hash256,
    pub partial_signature: PartialSignature,
    pub next_local_nonce: PubNonce,
}

fn partial_signature_to_molecule(partial_signature: PartialSignature) -> MByte32 {
    MByte32::new_builder()
        .set(
            partial_signature
                .serialize()
                .into_iter()
                .map(Byte::new)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        )
        .build()
}

impl From<CommitmentSigned> for molecule_fiber::CommitmentSigned {
    fn from(commitment_signed: CommitmentSigned) -> Self {
        molecule_fiber::CommitmentSigned::new_builder()
            .channel_id(commitment_signed.channel_id.into())
            .partial_signature(partial_signature_to_molecule(
                commitment_signed.partial_signature,
            ))
            .next_local_nonce((&commitment_signed.next_local_nonce).into())
            .build()
    }
}

impl TryFrom<molecule_fiber::CommitmentSigned> for CommitmentSigned {
    type Error = Error;

    fn try_from(commitment_signed: molecule_fiber::CommitmentSigned) -> Result<Self, Self::Error> {
        Ok(CommitmentSigned {
            channel_id: commitment_signed.channel_id().into(),
            partial_signature: PartialSignature::from_slice(
                commitment_signed.partial_signature().as_slice(),
            )
            .map_err(|e| anyhow!(e))?,
            next_local_nonce: commitment_signed
                .next_local_nonce()
                .try_into()
                .map_err(|e| anyhow!(format!("{e:?}")))?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxSignatures {
    pub channel_id: Hash256,
    pub tx_hash: Hash256,
    pub witnesses: Vec<Vec<u8>>,
}

impl From<TxSignatures> for molecule_fiber::TxSignatures {
    fn from(tx_signatures: TxSignatures) -> Self {
        molecule_fiber::TxSignatures::new_builder()
            .channel_id(tx_signatures.channel_id.into())
            .tx_hash(tx_signatures.tx_hash.into())
            .witnesses(
                BytesVec::new_builder()
                    .set(
                        tx_signatures
                            .witnesses
                            .into_iter()
                            .map(|witness| witness.pack())
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::TxSignatures> for TxSignatures {
    type Error = Error;

    fn try_from(tx_signatures: molecule_fiber::TxSignatures) -> Result<Self, Self::Error> {
        Ok(TxSignatures {
            channel_id: tx_signatures.channel_id().into(),
            tx_hash: tx_signatures.tx_hash().into(),
            witnesses: tx_signatures
                .witnesses()
                .into_iter()
                .map(|witness| witness.unpack())
                .collect(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelReady {
    pub channel_id: Hash256,
}

impl From<ChannelReady> for molecule_fiber::ChannelReady {
    fn from(channel_ready: ChannelReady) -> Self {
        molecule_fiber::ChannelReady::new_builder()
            .channel_id(channel_ready.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::ChannelReady> for ChannelReady {
    type Error = Error;

    fn try_from(channel_ready: molecule_fiber::ChannelReady) -> Result<Self, Self::Error> {
        Ok(ChannelReady {
            channel_id: channel_ready.channel_id().into(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum TxCollaborationMsg {
    TxUpdate(TxUpdate),
    TxComplete(TxComplete),
}

#[derive(Debug, Clone)]
pub struct TxUpdate {
    pub channel_id: Hash256,
    pub tx: Transaction,
}

impl From<TxUpdate> for molecule_fiber::TxUpdate {
    fn from(tx_update: TxUpdate) -> Self {
        molecule_fiber::TxUpdate::new_builder()
            .channel_id(tx_update.channel_id.into())
            .tx(tx_update.tx)
            .build()
    }
}

impl TryFrom<molecule_fiber::TxUpdate> for TxUpdate {
    type Error = Error;

    fn try_from(tx_update: molecule_fiber::TxUpdate) -> Result<Self, Self::Error> {
        Ok(TxUpdate {
            channel_id: tx_update.channel_id().into(),
            tx: tx_update.tx(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxComplete {
    pub channel_id: Hash256,
}

impl From<TxComplete> for molecule_fiber::TxComplete {
    fn from(tx_complete: TxComplete) -> Self {
        molecule_fiber::TxComplete::new_builder()
            .channel_id(tx_complete.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::TxComplete> for TxComplete {
    type Error = Error;

    fn try_from(tx_complete: molecule_fiber::TxComplete) -> Result<Self, Self::Error> {
        Ok(TxComplete {
            channel_id: tx_complete.channel_id().into(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAbort {
    pub channel_id: Hash256,
    pub message: Vec<u8>,
}

impl From<TxAbort> for molecule_fiber::TxAbort {
    fn from(tx_abort: TxAbort) -> Self {
        molecule_fiber::TxAbort::new_builder()
            .channel_id(tx_abort.channel_id.into())
            .message(tx_abort.message.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::TxAbort> for TxAbort {
    type Error = Error;

    fn try_from(tx_abort: molecule_fiber::TxAbort) -> Result<Self, Self::Error> {
        Ok(TxAbort {
            channel_id: tx_abort.channel_id().into(),
            message: tx_abort.message().unpack(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct TxInitRBF {
    pub channel_id: Hash256,
    pub fee_rate: u64,
}

impl From<TxInitRBF> for molecule_fiber::TxInitRBF {
    fn from(tx_init_rbf: TxInitRBF) -> Self {
        molecule_fiber::TxInitRBF::new_builder()
            .channel_id(tx_init_rbf.channel_id.into())
            .fee_rate(tx_init_rbf.fee_rate.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::TxInitRBF> for TxInitRBF {
    type Error = Error;

    fn try_from(tx_init_rbf: molecule_fiber::TxInitRBF) -> Result<Self, Self::Error> {
        Ok(TxInitRBF {
            channel_id: tx_init_rbf.channel_id().into(),
            fee_rate: tx_init_rbf.fee_rate().unpack(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAckRBF {
    pub channel_id: Hash256,
}

impl From<TxAckRBF> for molecule_fiber::TxAckRBF {
    fn from(tx_ack_rbf: TxAckRBF) -> Self {
        molecule_fiber::TxAckRBF::new_builder()
            .channel_id(tx_ack_rbf.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::TxAckRBF> for TxAckRBF {
    type Error = Error;

    fn try_from(tx_ack_rbf: molecule_fiber::TxAckRBF) -> Result<Self, Self::Error> {
        Ok(TxAckRBF {
            channel_id: tx_ack_rbf.channel_id().into(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Shutdown {
    pub channel_id: Hash256,
    pub close_script: Script,
    pub force: bool,
    pub fee_rate: FeeRate,
}

impl From<Shutdown> for molecule_fiber::Shutdown {
    fn from(shutdown: Shutdown) -> Self {
        molecule_fiber::Shutdown::new_builder()
            .channel_id(shutdown.channel_id.into())
            .close_script(shutdown.close_script)
            .fee_rate(shutdown.fee_rate.as_u64().pack())
            .force(if shutdown.force { 1_u8 } else { 0_u8 }.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::Shutdown> for Shutdown {
    type Error = Error;

    fn try_from(shutdown: molecule_fiber::Shutdown) -> Result<Self, Self::Error> {
        let force: u8 = shutdown.force().into();
        Ok(Shutdown {
            channel_id: shutdown.channel_id().into(),
            close_script: shutdown.close_script(),
            fee_rate: FeeRate::from_u64(shutdown.fee_rate().unpack()),
            force: force != 0,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ClosingSigned {
    pub channel_id: Hash256,
    pub partial_signature: PartialSignature,
}

impl From<ClosingSigned> for molecule_fiber::ClosingSigned {
    fn from(closing_signed: ClosingSigned) -> Self {
        molecule_fiber::ClosingSigned::new_builder()
            .channel_id(closing_signed.channel_id.into())
            .partial_signature(partial_signature_to_molecule(
                closing_signed.partial_signature,
            ))
            .build()
    }
}

impl TryFrom<molecule_fiber::ClosingSigned> for ClosingSigned {
    type Error = Error;

    fn try_from(closing_signed: molecule_fiber::ClosingSigned) -> Result<Self, Self::Error> {
        Ok(ClosingSigned {
            channel_id: closing_signed.channel_id().into(),
            partial_signature: PartialSignature::from_slice(
                closing_signed.partial_signature().as_slice(),
            )
            .map_err(|e| anyhow!(e))?,
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AddTlc {
    pub channel_id: Hash256,
    pub tlc_id: u64,
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: LockTime,
    pub hash_algorithm: HashAlgorithm,
}

impl From<AddTlc> for molecule_fiber::AddTlc {
    fn from(add_tlc: AddTlc) -> Self {
        molecule_fiber::AddTlc::new_builder()
            .channel_id(add_tlc.channel_id.into())
            .tlc_id(add_tlc.tlc_id.pack())
            .amount(add_tlc.amount.pack())
            .payment_hash(add_tlc.payment_hash.into())
            .expiry(add_tlc.expiry.into())
            .hash_algorithm(Byte::new(add_tlc.hash_algorithm as u8))
            .build()
    }
}

impl TryFrom<molecule_fiber::AddTlc> for AddTlc {
    type Error = Error;

    fn try_from(add_tlc: molecule_fiber::AddTlc) -> Result<Self, Self::Error> {
        Ok(AddTlc {
            channel_id: add_tlc.channel_id().into(),
            tlc_id: add_tlc.tlc_id().unpack(),
            amount: add_tlc.amount().unpack(),
            payment_hash: add_tlc.payment_hash().into(),
            expiry: add_tlc.expiry().try_into()?,
            hash_algorithm: add_tlc
                .hash_algorithm()
                .try_into()
                .map_err(|err: UnknownHashAlgorithmError| Error::AnyHow(err.into()))?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeAndAck {
    pub channel_id: Hash256,
    pub per_commitment_secret: Hash256,
    pub next_per_commitment_point: Pubkey,
}

impl From<RevokeAndAck> for molecule_fiber::RevokeAndAck {
    fn from(revoke_and_ack: RevokeAndAck) -> Self {
        molecule_fiber::RevokeAndAck::new_builder()
            .channel_id(revoke_and_ack.channel_id.into())
            .per_commitment_secret(revoke_and_ack.per_commitment_secret.into())
            .next_per_commitment_point(revoke_and_ack.next_per_commitment_point.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::RevokeAndAck> for RevokeAndAck {
    type Error = Error;

    fn try_from(revoke_and_ack: molecule_fiber::RevokeAndAck) -> Result<Self, Self::Error> {
        Ok(RevokeAndAck {
            channel_id: revoke_and_ack.channel_id().into(),
            per_commitment_secret: revoke_and_ack.per_commitment_secret().into(),
            next_per_commitment_point: revoke_and_ack.next_per_commitment_point().try_into()?,
        })
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveTlcFulfill {
    pub payment_preimage: Hash256,
}

impl From<RemoveTlcFulfill> for molecule_fiber::RemoveTlcFulfill {
    fn from(remove_tlc_fulfill: RemoveTlcFulfill) -> Self {
        molecule_fiber::RemoveTlcFulfill::new_builder()
            .payment_preimage(remove_tlc_fulfill.payment_preimage.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::RemoveTlcFulfill> for RemoveTlcFulfill {
    type Error = Error;

    fn try_from(remove_tlc_fulfill: molecule_fiber::RemoveTlcFulfill) -> Result<Self, Self::Error> {
        Ok(RemoveTlcFulfill {
            payment_preimage: remove_tlc_fulfill.payment_preimage().into(),
        })
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveTlcFail {
    pub error_code: u32,
}

impl From<RemoveTlcFail> for molecule_fiber::RemoveTlcFail {
    fn from(remove_tlc_fail: RemoveTlcFail) -> Self {
        molecule_fiber::RemoveTlcFail::new_builder()
            .error_code(remove_tlc_fail.error_code.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::RemoveTlcFail> for RemoveTlcFail {
    type Error = Error;

    fn try_from(remove_tlc_fail: molecule_fiber::RemoveTlcFail) -> Result<Self, Self::Error> {
        Ok(RemoveTlcFail {
            error_code: remove_tlc_fail.error_code().unpack(),
        })
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoveTlcReason {
    RemoveTlcFulfill(RemoveTlcFulfill),
    RemoveTlcFail(RemoveTlcFail),
}

impl From<RemoveTlcReason> for molecule_fiber::RemoveTlcReasonUnion {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        match remove_tlc_reason {
            RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill) => {
                molecule_fiber::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill.into())
            }
            RemoveTlcReason::RemoveTlcFail(remove_tlc_fail) => {
                molecule_fiber::RemoveTlcReasonUnion::RemoveTlcFail(remove_tlc_fail.into())
            }
        }
    }
}

impl From<RemoveTlcReason> for molecule_fiber::RemoveTlcReason {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        molecule_fiber::RemoveTlcReason::new_builder()
            .set(remove_tlc_reason)
            .build()
    }
}

impl TryFrom<molecule_fiber::RemoveTlcReason> for RemoveTlcReason {
    type Error = Error;

    fn try_from(remove_tlc_reason: molecule_fiber::RemoveTlcReason) -> Result<Self, Self::Error> {
        match remove_tlc_reason.to_enum() {
            molecule_fiber::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill) => Ok(
                RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill.try_into()?),
            ),
            molecule_fiber::RemoveTlcReasonUnion::RemoveTlcFail(remove_tlc_fail) => {
                Ok(RemoveTlcReason::RemoveTlcFail(remove_tlc_fail.try_into()?))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveTlc {
    pub channel_id: Hash256,
    pub tlc_id: u64,
    pub reason: RemoveTlcReason,
}

impl From<RemoveTlc> for molecule_fiber::RemoveTlc {
    fn from(remove_tlc: RemoveTlc) -> Self {
        molecule_fiber::RemoveTlc::new_builder()
            .channel_id(remove_tlc.channel_id.into())
            .tlc_id(remove_tlc.tlc_id.pack())
            .reason(
                molecule_fiber::RemoveTlcReason::new_builder()
                    .set(remove_tlc.reason)
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::RemoveTlc> for RemoveTlc {
    type Error = Error;

    fn try_from(remove_tlc: molecule_fiber::RemoveTlc) -> Result<Self, Self::Error> {
        Ok(RemoveTlc {
            channel_id: remove_tlc.channel_id().into(),
            tlc_id: remove_tlc.tlc_id().unpack(),
            reason: remove_tlc.reason().try_into()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ReestablishChannel {
    pub channel_id: Hash256,
    pub local_commitment_number: u64,
    pub remote_commitment_number: u64,
}

impl From<ReestablishChannel> for molecule_fiber::ReestablishChannel {
    fn from(reestablish_channel: ReestablishChannel) -> Self {
        molecule_fiber::ReestablishChannel::new_builder()
            .channel_id(reestablish_channel.channel_id.into())
            .local_commitment_number(reestablish_channel.local_commitment_number.pack())
            .remote_commitment_number(reestablish_channel.remote_commitment_number.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::ReestablishChannel> for ReestablishChannel {
    type Error = Error;

    fn try_from(
        reestablish_channel: molecule_fiber::ReestablishChannel,
    ) -> Result<Self, Self::Error> {
        Ok(ReestablishChannel {
            channel_id: reestablish_channel.channel_id().into(),
            local_commitment_number: reestablish_channel.local_commitment_number().unpack(),
            remote_commitment_number: reestablish_channel.remote_commitment_number().unpack(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct AnnouncementSignatures {
    pub channel_id: Hash256,
    pub channel_outpoint: OutPoint,
    pub node_signature: EcdsaSignature,
    pub partial_signature: PartialSignature,
}

impl From<AnnouncementSignatures> for molecule_fiber::AnnouncementSignatures {
    fn from(announcement_signatures: AnnouncementSignatures) -> Self {
        molecule_fiber::AnnouncementSignatures::new_builder()
            .channel_id(announcement_signatures.channel_id.into())
            .channel_outpoint(announcement_signatures.channel_outpoint)
            .node_signature(announcement_signatures.node_signature.into())
            .partial_signature(partial_signature_to_molecule(
                announcement_signatures.partial_signature,
            ))
            .build()
    }
}

impl TryFrom<molecule_fiber::AnnouncementSignatures> for AnnouncementSignatures {
    type Error = Error;

    fn try_from(
        announcement_signatures: molecule_fiber::AnnouncementSignatures,
    ) -> Result<Self, Self::Error> {
        Ok(AnnouncementSignatures {
            channel_id: announcement_signatures.channel_id().into(),
            channel_outpoint: announcement_signatures.channel_outpoint(),
            node_signature: announcement_signatures.node_signature().try_into()?,
            partial_signature: PartialSignature::from_slice(
                announcement_signatures.partial_signature().as_slice(),
            )
            .map_err(|e| anyhow!(e))?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeAnnouncement {
    // Signature to this message, may be empty the message is not signed yet.
    pub signature: Option<EcdsaSignature>,
    // Tentatively using 64 bits for features. May change the type later while developing.
    // rust-lightning uses a Vec<u8> here.
    pub features: u64,
    // Timestamp to the node announcement update, later update should have larger timestamp.
    pub timestamp: u64,
    pub node_id: Pubkey,
    // Must be a valid utf-8 string of length maximal length 32 bytes.
    // If the length is less than 32 bytes, it will be padded with 0.
    // If the length is more than 32 bytes, it should be truncated.
    pub alias: AnnouncedNodeName,
    // All the reachable addresses.
    pub addresses: Vec<Vec<u8>>,
}

impl NodeAnnouncement {
    pub fn new_unsigned(
        alias: AnnouncedNodeName,
        addresses: Vec<MultiAddr>,
        node_id: Pubkey,
    ) -> Self {
        Self {
            signature: None,
            features: Default::default(),
            timestamp: Default::default(),
            node_id,
            alias,
            addresses: addresses.iter().map(|a| a.to_vec()).collect(),
        }
    }

    pub fn new(
        alias: AnnouncedNodeName,
        addresses: Vec<MultiAddr>,
        private_key: &Privkey,
    ) -> NodeAnnouncement {
        let mut unsigned = NodeAnnouncement::new_unsigned(alias, addresses, private_key.pubkey());
        unsigned.signature = Some(private_key.sign(unsigned.message_to_sign()));
        unsigned
    }

    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_announcement = NodeAnnouncement {
            signature: None,
            features: self.features,
            timestamp: self.timestamp,
            node_id: self.node_id,
            alias: self.alias,
            addresses: self.addresses.clone(),
        };
        deterministically_hash(&unsigned_announcement)
    }
}

impl From<NodeAnnouncement> for molecule_fiber::NodeAnnouncement {
    fn from(node_announcement: NodeAnnouncement) -> Self {
        molecule_fiber::NodeAnnouncement::new_builder()
            .signature(
                node_announcement
                    .signature
                    .expect("node announcement signed")
                    .into(),
            )
            .features(node_announcement.features.pack())
            .timestamp(node_announcement.timestamp.pack())
            .node_id(node_announcement.node_id.into())
            .alias(u8_32_as_byte_32(&node_announcement.alias.0))
            .address(
                BytesVec::new_builder()
                    .set(
                        node_announcement
                            .addresses
                            .into_iter()
                            .map(|address| address.pack())
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::NodeAnnouncement> for NodeAnnouncement {
    type Error = Error;

    fn try_from(node_announcement: molecule_fiber::NodeAnnouncement) -> Result<Self, Self::Error> {
        Ok(NodeAnnouncement {
            signature: Some(node_announcement.signature().try_into()?),
            features: node_announcement.features().unpack(),
            timestamp: node_announcement.timestamp().unpack(),
            node_id: node_announcement.node_id().try_into()?,
            alias: AnnouncedNodeName::from_slice(node_announcement.alias().as_slice())
                .map_err(|e| Error::AnyHow(anyhow!("Invalid alias: {}", e)))?,
            addresses: node_announcement
                .address()
                .into_iter()
                .map(|address| address.unpack())
                .collect(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelAnnouncement {
    pub node_1_signature: Option<EcdsaSignature>,
    pub node_2_signature: Option<EcdsaSignature>,
    // Signature signed by the funding transaction output public key.
    pub ckb_signature: Option<SchnorrSignature>,
    // Tentatively using 64 bits for features. May change the type later while developing.
    // rust-lightning uses a Vec<u8> here.
    pub features: u64,
    pub chain_hash: Hash256,
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    pub node_1_id: Pubkey,
    pub node_2_id: Pubkey,
    // The aggregated public key of the funding transaction output.
    pub ckb_key: XOnlyPublicKey,
}

impl ChannelAnnouncement {
    pub fn new_unsigned(
        node_1_pubkey: &Pubkey,
        node_2_pubkey: &Pubkey,
        channel_outpoint: OutPoint,
        chain_hash: Hash256,
        ckb_pubkey: &XOnlyPublicKey,
    ) -> Self {
        Self {
            node_1_signature: None,
            node_2_signature: None,
            ckb_signature: None,
            features: Default::default(),
            chain_hash,
            channel_outpoint,
            node_1_id: node_1_pubkey.clone(),
            node_2_id: node_2_pubkey.clone(),
            ckb_key: ckb_pubkey.clone(),
        }
    }

    pub fn is_signed(&self) -> bool {
        self.node_1_signature.is_some()
            && self.node_2_signature.is_some()
            && self.ckb_signature.is_some()
    }

    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_announcement = Self {
            node_1_signature: None,
            node_2_signature: None,
            ckb_signature: None,
            features: self.features,
            chain_hash: self.chain_hash,
            channel_outpoint: self.channel_outpoint.clone(),
            node_1_id: self.node_1_id,
            node_2_id: self.node_2_id,
            ckb_key: self.ckb_key,
        };
        deterministically_hash(&unsigned_announcement)
    }
}

impl From<ChannelAnnouncement> for molecule_fiber::ChannelAnnouncement {
    fn from(channel_announcement: ChannelAnnouncement) -> Self {
        molecule_fiber::ChannelAnnouncement::new_builder()
            .node_signature_1(
                channel_announcement
                    .node_1_signature
                    .expect("channel announcement signed")
                    .into(),
            )
            .node_signature_2(
                channel_announcement
                    .node_2_signature
                    .expect("channel announcement signed")
                    .into(),
            )
            .ckb_signature(
                channel_announcement
                    .ckb_signature
                    .expect("channel announcement signed")
                    .into(),
            )
            .features(channel_announcement.features.pack())
            .chain_hash(channel_announcement.chain_hash.into())
            .channel_outpoint(channel_announcement.channel_outpoint)
            .node_1_id(channel_announcement.node_1_id.into())
            .node_2_id(channel_announcement.node_2_id.into())
            .ckb_key(channel_announcement.ckb_key.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::ChannelAnnouncement> for ChannelAnnouncement {
    type Error = Error;

    fn try_from(
        channel_announcement: molecule_fiber::ChannelAnnouncement,
    ) -> Result<Self, Self::Error> {
        Ok(ChannelAnnouncement {
            node_1_signature: Some(channel_announcement.node_signature_1().try_into()?),
            node_2_signature: Some(channel_announcement.node_signature_2().try_into()?),
            ckb_signature: Some(channel_announcement.ckb_signature().try_into()?),
            features: channel_announcement.features().unpack(),
            chain_hash: channel_announcement.chain_hash().into(),
            channel_outpoint: channel_announcement.channel_outpoint(),
            node_1_id: channel_announcement.node_1_id().try_into()?,
            node_2_id: channel_announcement.node_2_id().try_into()?,
            ckb_key: channel_announcement.ckb_key().try_into()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelUpdate {
    // Signature of the node that wants to update the channel information.
    pub signature: Option<EcdsaSignature>,
    pub chain_hash: Hash256,
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    pub timestamp: u64,
    pub message_flags: u32,
    pub channel_flags: u32,
    pub cltv_expiry_delta: u64,
    pub htlc_minimum_value: u128,
    pub htlc_maximum_value: u128,
    pub fee_value: u128,
}

impl ChannelUpdate {
    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_update = ChannelUpdate {
            signature: None,
            chain_hash: self.chain_hash,
            channel_outpoint: self.channel_outpoint.clone(),
            timestamp: self.timestamp,
            message_flags: self.message_flags,
            channel_flags: self.channel_flags,
            cltv_expiry_delta: self.cltv_expiry_delta,
            htlc_minimum_value: self.htlc_minimum_value,
            htlc_maximum_value: self.htlc_maximum_value,
            fee_value: self.fee_value,
        };
        deterministically_hash(&unsigned_update)
    }

    pub fn sign(&mut self, private_key: &Privkey) {
        self.signature = Some(private_key.sign(self.message_to_sign()));
    }
}

impl From<ChannelUpdate> for molecule_fiber::ChannelUpdate {
    fn from(channel_update: ChannelUpdate) -> Self {
        molecule_fiber::ChannelUpdate::new_builder()
            .signature(
                channel_update
                    .signature
                    .expect("channel update signed")
                    .into(),
            )
            .chain_hash(channel_update.chain_hash.into())
            .channel_outpoint(channel_update.channel_outpoint)
            .timestamp(channel_update.timestamp.pack())
            .message_flags(channel_update.message_flags.pack())
            .channel_flags(channel_update.channel_flags.pack())
            .cltv_expiry_delta(channel_update.cltv_expiry_delta.pack())
            .htlc_minimum_value(channel_update.htlc_minimum_value.pack())
            .htlc_maximum_value(channel_update.htlc_maximum_value.pack())
            .fee_value(channel_update.fee_value.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::ChannelUpdate> for ChannelUpdate {
    type Error = Error;

    fn try_from(channel_update: molecule_fiber::ChannelUpdate) -> Result<Self, Self::Error> {
        Ok(ChannelUpdate {
            signature: Some(channel_update.signature().try_into()?),
            chain_hash: channel_update.chain_hash().into(),
            channel_outpoint: channel_update.channel_outpoint(),
            timestamp: channel_update.timestamp().unpack(),
            message_flags: channel_update.message_flags().unpack(),
            channel_flags: channel_update.channel_flags().unpack(),
            cltv_expiry_delta: channel_update.cltv_expiry_delta().unpack(),
            htlc_minimum_value: channel_update.htlc_minimum_value().unpack(),
            htlc_maximum_value: channel_update.htlc_maximum_value().unpack(),
            fee_value: channel_update.fee_value().unpack(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum FiberMessage {
    ChannelInitialization(OpenChannel),
    ChannelNormalOperation(FiberChannelMessage),
    BroadcastMessage(FiberBroadcastMessage),
}

impl FiberMessage {
    pub fn open_channel(open_channel: OpenChannel) -> Self {
        FiberMessage::ChannelInitialization(open_channel)
    }

    pub fn accept_channel(accept_channel: AcceptChannel) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::AcceptChannel(accept_channel))
    }

    pub fn commitment_signed(commitment_signed: CommitmentSigned) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSigned(
            commitment_signed,
        ))
    }

    pub fn tx_signatures(tx_signatures: TxSignatures) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxSignatures(tx_signatures))
    }

    pub fn channel_ready(channel_ready: ChannelReady) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReady(channel_ready))
    }

    pub fn tx_update(tx_update: TxUpdate) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxUpdate(tx_update))
    }

    pub fn tx_complete(tx_complete: TxComplete) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxComplete(tx_complete))
    }

    pub fn tx_abort(tx_abort: TxAbort) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxAbort(tx_abort))
    }

    pub fn tx_init_rbf(tx_init_rbf: TxInitRBF) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxInitRBF(tx_init_rbf))
    }

    pub fn tx_ack_rbf(tx_ack_rbf: TxAckRBF) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxAckRBF(tx_ack_rbf))
    }

    pub fn shutdown(shutdown: Shutdown) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::Shutdown(shutdown))
    }

    pub fn closing_signed(closing_signed: ClosingSigned) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::ClosingSigned(closing_signed))
    }

    pub fn add_tlc(add_tlc: AddTlc) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(add_tlc))
    }

    pub fn revoke_and_ack(revoke_and_ack: RevokeAndAck) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAck(revoke_and_ack))
    }

    pub fn remove_tlc(remove_tlc: RemoveTlc) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::RemoveTlc(remove_tlc))
    }

    pub fn reestablish_channel(reestablish_channel: ReestablishChannel) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::ReestablishChannel(
            reestablish_channel,
        ))
    }

    pub fn announcement_signatures(announcement_signatures: AnnouncementSignatures) -> Self {
        FiberMessage::ChannelNormalOperation(FiberChannelMessage::AnnouncementSignatures(
            announcement_signatures,
        ))
    }

    pub fn node_announcement(node_announcement: NodeAnnouncement) -> Self {
        FiberMessage::BroadcastMessage(FiberBroadcastMessage::NodeAnnouncement(node_announcement))
    }

    pub fn channel_announcement(channel_announcement: ChannelAnnouncement) -> Self {
        FiberMessage::BroadcastMessage(FiberBroadcastMessage::ChannelAnnouncement(
            channel_announcement,
        ))
    }

    pub fn channel_update(channel_update: ChannelUpdate) -> Self {
        FiberMessage::BroadcastMessage(FiberBroadcastMessage::ChannelUpdate(channel_update))
    }
}

#[derive(Debug, Clone)]
pub enum FiberChannelMessage {
    AcceptChannel(AcceptChannel),
    CommitmentSigned(CommitmentSigned),
    TxSignatures(TxSignatures),
    ChannelReady(ChannelReady),
    TxUpdate(TxUpdate),
    TxComplete(TxComplete),
    TxAbort(TxAbort),
    TxInitRBF(TxInitRBF),
    TxAckRBF(TxAckRBF),
    Shutdown(Shutdown),
    ClosingSigned(ClosingSigned),
    AddTlc(AddTlc),
    RevokeAndAck(RevokeAndAck),
    RemoveTlc(RemoveTlc),
    ReestablishChannel(ReestablishChannel),
    AnnouncementSignatures(AnnouncementSignatures),
}

impl FiberChannelMessage {
    pub fn get_channel_id(&self) -> Hash256 {
        match self {
            FiberChannelMessage::AcceptChannel(accept_channel) => accept_channel.channel_id,
            FiberChannelMessage::CommitmentSigned(commitment_signed) => {
                commitment_signed.channel_id
            }
            FiberChannelMessage::TxSignatures(tx_signatures) => tx_signatures.channel_id,
            FiberChannelMessage::ChannelReady(channel_ready) => channel_ready.channel_id,
            FiberChannelMessage::TxUpdate(tx_update) => tx_update.channel_id,
            FiberChannelMessage::TxComplete(tx_complete) => tx_complete.channel_id,
            FiberChannelMessage::TxAbort(tx_abort) => tx_abort.channel_id,
            FiberChannelMessage::TxInitRBF(tx_init_rbf) => tx_init_rbf.channel_id,
            FiberChannelMessage::TxAckRBF(tx_ack_rbf) => tx_ack_rbf.channel_id,
            FiberChannelMessage::Shutdown(shutdown) => shutdown.channel_id,
            FiberChannelMessage::ClosingSigned(closing_signed) => closing_signed.channel_id,
            FiberChannelMessage::AddTlc(add_tlc) => add_tlc.channel_id,
            FiberChannelMessage::RevokeAndAck(revoke_and_ack) => revoke_and_ack.channel_id,
            FiberChannelMessage::RemoveTlc(remove_tlc) => remove_tlc.channel_id,
            FiberChannelMessage::ReestablishChannel(reestablish_channel) => {
                reestablish_channel.channel_id
            }
            FiberChannelMessage::AnnouncementSignatures(annoucement_signatures) => {
                annoucement_signatures.channel_id
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum FiberBroadcastMessage {
    NodeAnnouncement(NodeAnnouncement),
    ChannelAnnouncement(ChannelAnnouncement),
    ChannelUpdate(ChannelUpdate),
}

impl FiberBroadcastMessage {
    pub fn id(&self) -> Hash256 {
        match self {
            FiberBroadcastMessage::NodeAnnouncement(node_announcement) => {
                deterministically_hash(node_announcement).into()
            }
            FiberBroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                deterministically_hash(channel_announcement).into()
            }
            FiberBroadcastMessage::ChannelUpdate(channel_update) => {
                deterministically_hash(channel_update).into()
            }
        }
    }
}

impl From<FiberMessage> for molecule_fiber::FiberMessageUnion {
    fn from(fiber_message: FiberMessage) -> Self {
        match fiber_message {
            FiberMessage::ChannelInitialization(open_channel) => {
                molecule_fiber::FiberMessageUnion::OpenChannel(open_channel.into())
            }
            FiberMessage::ChannelNormalOperation(m) => match m {
                FiberChannelMessage::AcceptChannel(accept_channel) => {
                    molecule_fiber::FiberMessageUnion::AcceptChannel(accept_channel.into())
                }
                FiberChannelMessage::CommitmentSigned(commitment_signed) => {
                    molecule_fiber::FiberMessageUnion::CommitmentSigned(commitment_signed.into())
                }
                FiberChannelMessage::TxSignatures(tx_signatures) => {
                    molecule_fiber::FiberMessageUnion::TxSignatures(tx_signatures.into())
                }
                FiberChannelMessage::ChannelReady(channel_ready) => {
                    molecule_fiber::FiberMessageUnion::ChannelReady(channel_ready.into())
                }
                FiberChannelMessage::TxUpdate(tx_update) => {
                    molecule_fiber::FiberMessageUnion::TxUpdate(tx_update.into())
                }
                FiberChannelMessage::TxComplete(tx_complete) => {
                    molecule_fiber::FiberMessageUnion::TxComplete(tx_complete.into())
                }
                FiberChannelMessage::TxAbort(tx_abort) => {
                    molecule_fiber::FiberMessageUnion::TxAbort(tx_abort.into())
                }
                FiberChannelMessage::TxInitRBF(tx_init_rbf) => {
                    molecule_fiber::FiberMessageUnion::TxInitRBF(tx_init_rbf.into())
                }
                FiberChannelMessage::TxAckRBF(tx_ack_rbf) => {
                    molecule_fiber::FiberMessageUnion::TxAckRBF(tx_ack_rbf.into())
                }
                FiberChannelMessage::Shutdown(shutdown) => {
                    molecule_fiber::FiberMessageUnion::Shutdown(shutdown.into())
                }
                FiberChannelMessage::ClosingSigned(closing_signed) => {
                    molecule_fiber::FiberMessageUnion::ClosingSigned(closing_signed.into())
                }
                FiberChannelMessage::AddTlc(add_tlc) => {
                    molecule_fiber::FiberMessageUnion::AddTlc(add_tlc.into())
                }
                FiberChannelMessage::RemoveTlc(remove_tlc) => {
                    molecule_fiber::FiberMessageUnion::RemoveTlc(remove_tlc.into())
                }
                FiberChannelMessage::RevokeAndAck(revoke_and_ack) => {
                    molecule_fiber::FiberMessageUnion::RevokeAndAck(revoke_and_ack.into())
                }
                FiberChannelMessage::ReestablishChannel(reestablish_channel) => {
                    molecule_fiber::FiberMessageUnion::ReestablishChannel(
                        reestablish_channel.into(),
                    )
                }
                FiberChannelMessage::AnnouncementSignatures(announcement_signatures) => {
                    molecule_fiber::FiberMessageUnion::AnnouncementSignatures(
                        announcement_signatures.into(),
                    )
                }
            },
            FiberMessage::BroadcastMessage(m) => match m {
                FiberBroadcastMessage::NodeAnnouncement(node_annoucement) => {
                    molecule_fiber::FiberMessageUnion::NodeAnnouncement(node_annoucement.into())
                }
                FiberBroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                    molecule_fiber::FiberMessageUnion::ChannelAnnouncement(
                        channel_announcement.into(),
                    )
                }
                FiberBroadcastMessage::ChannelUpdate(channel_update) => {
                    molecule_fiber::FiberMessageUnion::ChannelUpdate(channel_update.into())
                }
            },
        }
    }
}

impl From<FiberMessage> for molecule_fiber::FiberMessage {
    fn from(fiber_message: FiberMessage) -> Self {
        molecule_fiber::FiberMessage::new_builder()
            .set(fiber_message)
            .build()
    }
}

impl TryFrom<molecule_fiber::FiberMessage> for FiberMessage {
    type Error = Error;

    fn try_from(fiber_message: molecule_fiber::FiberMessage) -> Result<Self, Self::Error> {
        Ok(match fiber_message.to_enum() {
            molecule_fiber::FiberMessageUnion::OpenChannel(open_channel) => {
                FiberMessage::ChannelInitialization(open_channel.try_into()?)
            }
            molecule_fiber::FiberMessageUnion::AcceptChannel(accept_channel) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AcceptChannel(
                    accept_channel.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::CommitmentSigned(commitment_signed) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::CommitmentSigned(
                    commitment_signed.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::TxSignatures(tx_signatures) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxSignatures(
                    tx_signatures.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::ChannelReady(channel_ready) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ChannelReady(
                    channel_ready.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::TxUpdate(tx_update) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxUpdate(
                    tx_update.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::TxComplete(tx_complete) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxComplete(
                    tx_complete.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::TxAbort(tx_abort) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxAbort(
                    tx_abort.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::TxInitRBF(tx_init_rbf) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxInitRBF(
                    tx_init_rbf.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::TxAckRBF(tx_ack_rbf) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::TxAckRBF(
                    tx_ack_rbf.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::Shutdown(shutdown) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::Shutdown(
                    shutdown.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::ClosingSigned(closing_signed) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ClosingSigned(
                    closing_signed.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::AddTlc(add_tlc) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AddTlc(
                    add_tlc.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::RemoveTlc(remove_tlc) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::RemoveTlc(
                    remove_tlc.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::RevokeAndAck(revoke_and_ack) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::RevokeAndAck(
                    revoke_and_ack.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::ReestablishChannel(reestablish_channel) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::ReestablishChannel(
                    reestablish_channel.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::AnnouncementSignatures(announcement_signatures) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::AnnouncementSignatures(
                    announcement_signatures.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::NodeAnnouncement(node_announcement) => {
                FiberMessage::BroadcastMessage(FiberBroadcastMessage::NodeAnnouncement(
                    node_announcement.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::ChannelAnnouncement(channel_announcement) => {
                FiberMessage::BroadcastMessage(FiberBroadcastMessage::ChannelAnnouncement(
                    channel_announcement.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::ChannelUpdate(channel_update) => {
                FiberMessage::BroadcastMessage(FiberBroadcastMessage::ChannelUpdate(
                    channel_update.try_into()?,
                ))
            }
        })
    }
}

macro_rules! impl_traits {
    ($t:ident) => {
        impl $t {
            pub fn to_molecule_bytes(self) -> molecule::bytes::Bytes {
                molecule_fiber::$t::from(self).as_bytes()
            }
        }

        impl $t {
            pub fn from_molecule_slice(data: &[u8]) -> Result<Self, Error> {
                molecule_fiber::$t::from_slice(data)
                    .map_err(Into::into)
                    .and_then(TryInto::try_into)
            }
        }
    };
}

impl_traits!(FiberMessage);

pub(crate) fn deterministically_serialize<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec_pretty(v).expect("serialize value")
}

pub(crate) fn deterministically_hash<T: Serialize>(v: &T) -> [u8; 32] {
    ckb_hash::blake2b_256(deterministically_serialize(v))
}

#[cfg(test)]
mod tests {
    use super::{secp256k1_instance, Pubkey};

    use secp256k1::SecretKey;

    #[test]
    fn test_serde_public_key() {
        let sk = SecretKey::from_slice(&[42; 32]).unwrap();
        let public_key = Pubkey::from(sk.public_key(secp256k1_instance()));
        let pk_str = serde_json::to_string(&public_key).unwrap();
        assert_eq!(
            "\"035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c\"",
            &pk_str
        );
        let pubkey: Pubkey = serde_json::from_str(&pk_str).unwrap();
        assert_eq!(pubkey, public_key)
    }

    #[test]
    fn test_add_tlc_serialization() {
        let add_tlc = super::AddTlc {
            channel_id: [42; 32].into(),
            tlc_id: 42,
            amount: 42,
            payment_hash: [42; 32].into(),
            expiry: 42.into(),
            hash_algorithm: super::HashAlgorithm::Sha256,
        };
        let add_tlc_mol: super::molecule_fiber::AddTlc = add_tlc.clone().into();
        let add_tlc2 = add_tlc_mol.try_into().expect("decode");
        assert_eq!(add_tlc, add_tlc2);
    }
}
