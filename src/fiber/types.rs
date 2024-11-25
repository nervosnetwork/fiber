use crate::ckb::config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtScript};
use crate::ckb::contracts::get_udt_whitelist;

use super::channel::ChannelFlags;
use super::config::AnnouncedNodeName;
use super::gen::fiber::{
    self as molecule_fiber, BroadcastMessageQueries, PubNonce as Byte66, UdtCellDeps, Uint128Opt,
};
use super::hash_algorithm::{HashAlgorithm, UnknownHashAlgorithmError};
use super::network::get_chain_hash;
use super::r#gen::fiber::PubNonceOpt;
use super::serde_utils::{EntityHex, SliceHex};
use anyhow::anyhow;
use ckb_types::{
    core::FeeRate,
    packed::{Byte32 as MByte32, BytesVec, OutPoint, Script, Transaction},
    prelude::{Pack, Unpack},
};
use core::fmt::{self, Formatter};
use fiber_sphinx::SphinxError;
use molecule::prelude::{Builder, Byte, Entity};
use musig2::errors::DecodeError;
use musig2::secp::{Point, Scalar};
use musig2::{BinaryEncoding, PartialSignature, PubNonce};
use once_cell::sync::OnceCell;
use secp256k1::{
    ecdsa::Signature as Secp256k1Signature, schnorr::Signature as SchnorrSignature, All, PublicKey,
    Secp256k1, SecretKey, Signing,
};
use secp256k1::{Verification, XOnlyPublicKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::marker::PhantomData;
use std::str::FromStr;
use strum::{AsRefStr, EnumString};
use tentacle::multiaddr::MultiAddr;
use tentacle::secio::PeerId;
use thiserror::Error;
use tracing::{debug, trace};

pub fn secp256k1_instance() -> &'static Secp256k1<All> {
    static INSTANCE: OnceCell<Secp256k1<All>> = OnceCell::new();
    INSTANCE.get_or_init(Secp256k1::new)
}

impl From<&Byte66> for PubNonce {
    fn from(value: &Byte66) -> Self {
        PubNonce::from_bytes(value.as_slice()).expect("PubNonce from Byte66")
    }
}

impl From<&PubNonce> for Byte66 {
    fn from(value: &PubNonce) -> Self {
        Byte66::from_slice(&value.to_bytes()).expect("valid pubnonce serialized to 66 bytes")
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
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
                    .expect("Byte32 from Hash256"),
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
        Hash256(
            value
                .as_bytes()
                .to_vec()
                .try_into()
                .expect("Hash256 from Byte32"),
        )
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
                .expect("[u8; 32] to Byte32"),
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
        (scalar + sk)
            .not_zero()
            .expect("valid secp256k1 scalar addition")
            .into()
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
        secp256k1_instance().sign_ecdsa(&message, &self.0).into()
    }

    pub fn x_only_pub_key(&self) -> XOnlyPublicKey {
        let secp256k1_instance = secp256k1_instance();
        let secret_key = self.0;
        let keypair = secp256k1::Keypair::from_secret_key(secp256k1_instance, &secret_key);
        XOnlyPublicKey::from_keypair(&keypair).0
    }

    pub fn sign_schnorr(&self, message: [u8; 32]) -> SchnorrSignature {
        let secp256k1_instance = secp256k1_instance();
        let secret_key = self.0;
        let keypair = secp256k1::Keypair::from_secret_key(secp256k1_instance, &secret_key);
        let message = secp256k1::Message::from_digest(message);
        let sig = secp256k1_instance.sign_schnorr(&message, &keypair);
        trace!(
            "Schnorr signing message {:?} with private key {:?} (pub key {:?}), Signature: {:?}",
            message,
            keypair.secret_key(),
            keypair.public_key(),
            &sig
        );
        sig
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
        PublicKey::from(result.not_inf().expect("valid public key")).into()
    }

    pub fn tentacle_peer_id(&self) -> PeerId {
        let pubkey = (*self).into();
        PeerId::from_public_key(&pubkey)
    }
}

#[derive(Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
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
    #[error("Tentacle multiaddr error: {0}")]
    TentacleMultiAddr(#[from] tentacle::multiaddr::Error),
    #[error("Musig2 error: {0}")]
    Musig2(String),
    #[error("Invalid onion packet")]
    OnionPacket(#[from] OnionPacketError),
    #[error("Error: {0}")]
    AnyHow(#[from] anyhow::Error),
}

#[derive(Error, Debug)]
pub enum OnionPacketError {
    #[error("Try to peel the last hop")]
    PeelingLastHop,

    #[error("Fail to deserialize the hop data")]
    InvalidHopData,

    #[error("Sphinx protocol error")]
    Sphinx(#[from] SphinxError),
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
    pub shutdown_script: Script,
    pub reserved_ckb_amount: u64,
    pub funding_fee_rate: u64,
    pub commitment_fee_rate: u64,
    pub commitment_delay_epoch: u64,
    pub max_tlc_value_in_flight: u128,
    pub max_tlc_number_in_flight: u64,
    pub min_tlc_value: u128,
    pub funding_pubkey: Pubkey,
    pub tlc_basepoint: Pubkey,
    pub first_per_commitment_point: Pubkey,
    pub second_per_commitment_point: Pubkey,
    pub channel_announcement_nonce: Option<PubNonce>,
    pub next_local_nonce: PubNonce,
    pub channel_flags: ChannelFlags,
}

impl OpenChannel {
    pub fn is_public(&self) -> bool {
        self.channel_flags.contains(ChannelFlags::PUBLIC)
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
            .commitment_delay_epoch(open_channel.commitment_delay_epoch.pack())
            .max_tlc_value_in_flight(open_channel.max_tlc_value_in_flight.pack())
            .max_tlc_number_in_flight(open_channel.max_tlc_number_in_flight.pack())
            .min_tlc_value(open_channel.min_tlc_value.pack())
            .shutdown_script(open_channel.shutdown_script)
            .funding_pubkey(open_channel.funding_pubkey.into())
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
            shutdown_script: open_channel.shutdown_script(),
            funding_fee_rate: open_channel.funding_fee_rate().unpack(),
            commitment_fee_rate: open_channel.commitment_fee_rate().unpack(),
            commitment_delay_epoch: open_channel.commitment_delay_epoch().unpack(),
            max_tlc_value_in_flight: open_channel.max_tlc_value_in_flight().unpack(),
            max_tlc_number_in_flight: open_channel.max_tlc_number_in_flight().unpack(),
            min_tlc_value: open_channel.min_tlc_value().unpack(),
            funding_pubkey: open_channel.funding_pubkey().try_into()?,
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
    pub max_tlc_number_in_flight: u64,
    pub min_tlc_value: u128,
    pub funding_pubkey: Pubkey,
    pub shutdown_script: Script,
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
            .max_tlc_number_in_flight(accept_channel.max_tlc_number_in_flight.pack())
            .shutdown_script(accept_channel.shutdown_script)
            .min_tlc_value(accept_channel.min_tlc_value.pack())
            .funding_pubkey(accept_channel.funding_pubkey.into())
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
            shutdown_script: accept_channel.shutdown_script(),
            reserved_ckb_amount: accept_channel.reserved_ckb_amount().unpack(),
            max_tlc_value_in_flight: accept_channel.max_tlc_value_in_flight().unpack(),
            max_tlc_number_in_flight: accept_channel.max_tlc_number_in_flight().unpack(),
            min_tlc_value: accept_channel.min_tlc_value().unpack(),
            funding_pubkey: accept_channel.funding_pubkey().try_into()?,
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
    pub funding_tx_partial_signature: PartialSignature,
    pub commitment_tx_partial_signature: PartialSignature,
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
                .expect("[Byte; 32] from [u8; 32]"),
        )
        .build()
}

impl From<CommitmentSigned> for molecule_fiber::CommitmentSigned {
    fn from(commitment_signed: CommitmentSigned) -> Self {
        molecule_fiber::CommitmentSigned::new_builder()
            .channel_id(commitment_signed.channel_id.into())
            .funding_tx_partial_signature(partial_signature_to_molecule(
                commitment_signed.funding_tx_partial_signature,
            ))
            .commitment_tx_partial_signature(partial_signature_to_molecule(
                commitment_signed.commitment_tx_partial_signature,
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
            funding_tx_partial_signature: PartialSignature::from_slice(
                commitment_signed.funding_tx_partial_signature().as_slice(),
            )
            .map_err(|e| anyhow!(e))?,
            commitment_tx_partial_signature: PartialSignature::from_slice(
                commitment_signed
                    .commitment_tx_partial_signature()
                    .as_slice(),
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
    pub fee_rate: FeeRate,
}

impl From<Shutdown> for molecule_fiber::Shutdown {
    fn from(shutdown: Shutdown) -> Self {
        molecule_fiber::Shutdown::new_builder()
            .channel_id(shutdown.channel_id.into())
            .close_script(shutdown.close_script)
            .fee_rate(shutdown.fee_rate.as_u64().pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::Shutdown> for Shutdown {
    type Error = Error;

    fn try_from(shutdown: molecule_fiber::Shutdown) -> Result<Self, Self::Error> {
        Ok(Shutdown {
            channel_id: shutdown.channel_id().into(),
            close_script: shutdown.close_script(),
            fee_rate: FeeRate::from_u64(shutdown.fee_rate().unpack()),
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
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    pub onion_packet: Vec<u8>,
}

impl From<AddTlc> for molecule_fiber::AddTlc {
    fn from(add_tlc: AddTlc) -> Self {
        molecule_fiber::AddTlc::new_builder()
            .channel_id(add_tlc.channel_id.into())
            .tlc_id(add_tlc.tlc_id.pack())
            .amount(add_tlc.amount.pack())
            .payment_hash(add_tlc.payment_hash.into())
            .expiry(add_tlc.expiry.pack())
            .hash_algorithm(Byte::new(add_tlc.hash_algorithm as u8))
            .onion_packet(add_tlc.onion_packet.pack())
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
            expiry: add_tlc.expiry().unpack(),
            onion_packet: add_tlc.onion_packet().unpack(),
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
    pub partial_signature: PartialSignature,
    pub next_per_commitment_point: Pubkey,
}

impl From<RevokeAndAck> for molecule_fiber::RevokeAndAck {
    fn from(revoke_and_ack: RevokeAndAck) -> Self {
        molecule_fiber::RevokeAndAck::new_builder()
            .channel_id(revoke_and_ack.channel_id.into())
            .partial_signature(partial_signature_to_molecule(
                revoke_and_ack.partial_signature,
            ))
            .next_per_commitment_point(revoke_and_ack.next_per_commitment_point.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::RevokeAndAck> for RevokeAndAck {
    type Error = Error;

    fn try_from(revoke_and_ack: molecule_fiber::RevokeAndAck) -> Result<Self, Self::Error> {
        Ok(RevokeAndAck {
            channel_id: revoke_and_ack.channel_id().into(),
            partial_signature: PartialSignature::from_slice(
                revoke_and_ack.partial_signature().as_slice(),
            )
            .map_err(|e| anyhow!(e))?,
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

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TlcErrData {
    ChannelFailed {
        #[serde_as(as = "EntityHex")]
        channel_outpoint: OutPoint,
        channel_update: Option<ChannelUpdate>,
    },
    NodeFailed {
        node_id: Pubkey,
    },
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TlcErr {
    pub error_code: TlcErrorCode,
    pub extra_data: Option<TlcErrData>,
}

impl TlcErr {
    pub fn new(error_code: TlcErrorCode) -> Self {
        TlcErr {
            error_code: error_code,
            extra_data: None,
        }
    }

    pub fn new_node_fail(error_code: TlcErrorCode, node_id: Pubkey) -> Self {
        TlcErr {
            error_code: error_code.into(),
            extra_data: Some(TlcErrData::NodeFailed { node_id }),
        }
    }

    pub fn new_channel_fail(
        error_code: TlcErrorCode,
        channel_outpoint: OutPoint,
        channel_update: Option<ChannelUpdate>,
    ) -> Self {
        TlcErr {
            error_code: error_code.into(),
            extra_data: Some(TlcErrData::ChannelFailed {
                channel_outpoint,
                channel_update,
            }),
        }
    }

    pub fn error_node_id(&self) -> Option<Pubkey> {
        match &self.extra_data {
            Some(TlcErrData::NodeFailed { node_id }) => Some(*node_id),
            _ => None,
        }
    }

    pub fn error_channel_outpoint(&self) -> Option<OutPoint> {
        match &self.extra_data {
            Some(TlcErrData::ChannelFailed {
                channel_outpoint, ..
            }) => Some(channel_outpoint.clone()),
            _ => None,
        }
    }

    pub fn error_code(&self) -> TlcErrorCode {
        self.error_code
    }

    pub fn error_code_as_str(&self) -> String {
        let error_code: TlcErrorCode = self.error_code.into();
        error_code.as_ref().to_string()
    }

    pub fn set_extra_data(&mut self, extra_data: TlcErrData) {
        self.extra_data = Some(extra_data);
    }

    fn serialize(&self) -> Vec<u8> {
        deterministically_serialize(self)
    }

    fn deserialize(data: &[u8]) -> Option<Self> {
        serde_json::from_slice(data).ok()
    }
}

// This is the onion packet we need to encode and send back to the sender,
// currently it's the raw TlcErr serialized data from the TlcErr struct,
// sender should decode it and then decide what to do with the error.
// Note: this supposed to be only accessible by the sender, and it's not reliable since it
//       is not placed on-chain due to the possibility of hop failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TlcErrPacket {
    // TODO: replace this with the real onion packet
    pub onion_packet: Vec<u8>,
}

impl TlcErrPacket {
    pub fn new(tlc_fail: TlcErr) -> Self {
        TlcErrPacket {
            onion_packet: tlc_fail.serialize(),
        }
    }

    pub fn decode(&self) -> Option<TlcErr> {
        TlcErr::deserialize(&self.onion_packet)
    }
}

impl From<TlcErrPacket> for molecule_fiber::TlcErrPacket {
    fn from(remove_tlc_fail: TlcErrPacket) -> Self {
        molecule_fiber::TlcErrPacket::new_builder()
            .onion_packet(remove_tlc_fail.onion_packet.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::TlcErrPacket> for TlcErrPacket {
    type Error = Error;

    fn try_from(remove_tlc_fail: molecule_fiber::TlcErrPacket) -> Result<Self, Self::Error> {
        Ok(TlcErrPacket {
            onion_packet: remove_tlc_fail.onion_packet().unpack(),
        })
    }
}

impl std::fmt::Display for TlcErrPacket {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "TlcErrPacket")
    }
}

// The onion packet is invalid
const BADONION: u16 = 0x8000;
// Permanent errors (otherwise transient)
const PERM: u16 = 0x4000;
// Node releated errors (otherwise channels)
const NODE: u16 = 0x2000;
// Channel forwarding parameter was violated
const UPDATE: u16 = 0x1000;

#[repr(u16)]
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, AsRefStr, EnumString)]
pub enum TlcErrorCode {
    TemporaryNodeFailure = NODE | 2,
    PermanentNodeFailure = PERM | NODE | 2,
    // unused right now
    RequiredNodeFeatureMissing = PERM | NODE | 3,
    // unused right now, maybe need to add onion version in future?
    InvalidOnionVersion = BADONION | PERM | 4,
    InvalidOnionHmac = BADONION | PERM | 5,
    InvalidOnionKey = BADONION | PERM | 6,
    TemporaryChannelFailure = UPDATE | 7,
    // used for shutting down the channel
    PermanentChannelFailure = PERM | 8,
    RequiredChannelFeatureMissing = PERM | 9,
    UnknownNextPeer = PERM | 10,
    AmountBelowMinimum = UPDATE | 11,
    FeeInsufficient = UPDATE | 12,
    IncorrectTlcExpiry = UPDATE | 13,
    ExpiryTooSoon = PERM | 14,
    IncorrectOrUnknownPaymentDetails = PERM | 15,
    InvoiceExpired = PERM | 16,
    InvoiceCancelled = PERM | 17,
    FinalIncorrectExpiryDelta = 18,
    FinalIncorrectTlcAmount = 19,
    ChannelDisabled = UPDATE | 20,
    ExpiryTooFar = PERM | 21,
    InvalidOnionPayload = PERM | 22,
    MppTimeout = 23,
    InvalidOnionBlinding = BADONION | PERM | 24,
}

impl TlcErrorCode {
    pub fn is_node(&self) -> bool {
        *self as u16 & NODE != 0
    }

    pub fn is_bad_onion(&self) -> bool {
        *self as u16 & BADONION != 0
    }

    pub fn is_perm(&self) -> bool {
        *self as u16 & PERM != 0
    }

    pub fn is_update(&self) -> bool {
        *self as u16 & UPDATE != 0
    }

    pub fn payment_failed(&self) -> bool {
        match self {
            TlcErrorCode::IncorrectOrUnknownPaymentDetails
            | TlcErrorCode::FinalIncorrectExpiryDelta
            | TlcErrorCode::FinalIncorrectTlcAmount
            | TlcErrorCode::InvoiceExpired
            | TlcErrorCode::InvoiceCancelled
            | TlcErrorCode::ExpiryTooFar
            | TlcErrorCode::ExpiryTooSoon
            | TlcErrorCode::MppTimeout => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoveTlcReason {
    RemoveTlcFulfill(RemoveTlcFulfill),
    RemoveTlcFail(TlcErrPacket),
}

impl From<RemoveTlcReason> for molecule_fiber::RemoveTlcReasonUnion {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        match remove_tlc_reason {
            RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill) => {
                molecule_fiber::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill.into())
            }
            RemoveTlcReason::RemoveTlcFail(remove_tlc_fail) => {
                molecule_fiber::RemoveTlcReasonUnion::TlcErrPacket(remove_tlc_fail.into())
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
            molecule_fiber::RemoveTlcReasonUnion::TlcErrPacket(remove_tlc_fail) => {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeAnnouncement {
    // Signature to this message, may be empty the message is not signed yet.
    pub signature: Option<EcdsaSignature>,
    // Tentatively using 64 bits for features. May change the type later while developing.
    // rust-lightning uses a Vec<u8> here.
    pub features: u64,
    // Opaque version number of the node announcement update, later update should have larger version number.
    pub version: u64,
    pub node_id: Pubkey,
    // Must be a valid utf-8 string of length maximal length 32 bytes.
    // If the length is less than 32 bytes, it will be padded with 0.
    // If the length is more than 32 bytes, it should be truncated.
    pub alias: AnnouncedNodeName,
    // All the reachable addresses.
    pub addresses: Vec<MultiAddr>,
    // chain_hash
    pub chain_hash: Hash256,
    // If the other party funding more than this amount, we will automatically accept the channel.
    pub auto_accept_min_ckb_funding_amount: u64,
    // UDT config info
    pub udt_cfg_infos: UdtCfgInfos,
}

impl NodeAnnouncement {
    pub fn new_unsigned(
        alias: AnnouncedNodeName,
        addresses: Vec<MultiAddr>,
        node_id: Pubkey,
        version: u64,
        auto_accept_min_ckb_funding_amount: u64,
    ) -> Self {
        Self {
            signature: None,
            features: Default::default(),
            version,
            node_id,
            alias,
            chain_hash: get_chain_hash(),
            addresses,
            auto_accept_min_ckb_funding_amount,
            udt_cfg_infos: get_udt_whitelist(),
        }
    }

    pub fn new(
        alias: AnnouncedNodeName,
        addresses: Vec<MultiAddr>,
        private_key: &Privkey,
        version: u64,
        auto_accept_min_ckb_funding_amount: u64,
    ) -> NodeAnnouncement {
        let mut unsigned = NodeAnnouncement::new_unsigned(
            alias,
            addresses,
            private_key.pubkey(),
            version,
            auto_accept_min_ckb_funding_amount,
        );
        unsigned.signature = Some(private_key.sign(unsigned.message_to_sign()));
        unsigned
    }

    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_announcement = NodeAnnouncement {
            signature: None,
            features: self.features,
            version: self.version,
            node_id: self.node_id,
            alias: self.alias,
            chain_hash: self.chain_hash,
            addresses: self.addresses.clone(),
            auto_accept_min_ckb_funding_amount: self.auto_accept_min_ckb_funding_amount,
            udt_cfg_infos: get_udt_whitelist(),
        };
        deterministically_hash(&unsigned_announcement)
    }

    pub fn peer_id(&self) -> PeerId {
        PeerId::from_public_key(&self.node_id.into())
    }
}

impl From<UdtCellDep> for molecule_fiber::UdtCellDep {
    fn from(udt_cell_dep: UdtCellDep) -> Self {
        molecule_fiber::UdtCellDep::new_builder()
            .dep_type(udt_cell_dep.dep_type.into())
            .tx_hash(udt_cell_dep.tx_hash.pack())
            .index(udt_cell_dep.index.pack())
            .build()
    }
}

impl From<molecule_fiber::UdtCellDep> for UdtCellDep {
    fn from(udt_cell_dep: molecule_fiber::UdtCellDep) -> Self {
        UdtCellDep {
            dep_type: udt_cell_dep
                .dep_type()
                .try_into()
                .expect("invalid dep type"),
            tx_hash: udt_cell_dep.tx_hash().unpack(),
            index: udt_cell_dep.index().unpack(),
        }
    }
}

impl From<UdtScript> for molecule_fiber::UdtScript {
    fn from(udt_script: UdtScript) -> Self {
        molecule_fiber::UdtScript::new_builder()
            .code_hash(udt_script.code_hash.pack())
            .hash_type(udt_script.hash_type.into())
            .args(udt_script.args.pack())
            .build()
    }
}

impl From<molecule_fiber::UdtScript> for UdtScript {
    fn from(udt_script: molecule_fiber::UdtScript) -> Self {
        UdtScript {
            code_hash: udt_script.code_hash().unpack(),
            hash_type: udt_script
                .hash_type()
                .try_into()
                .expect("invalid hash type"),
            args: String::from_utf8(udt_script.args().unpack()).expect("invalid utf8"),
        }
    }
}

impl From<UdtArgInfo> for molecule_fiber::UdtArgInfo {
    fn from(udt_arg_info: UdtArgInfo) -> Self {
        molecule_fiber::UdtArgInfo::new_builder()
            .name(udt_arg_info.name.pack())
            .script(udt_arg_info.script.into())
            .auto_accept_amount(
                Uint128Opt::new_builder()
                    .set(udt_arg_info.auto_accept_amount.map(|x| x.pack()))
                    .build(),
            )
            .cell_deps(
                UdtCellDeps::new_builder()
                    .set(
                        udt_arg_info
                            .cell_deps
                            .into_iter()
                            .map(|cell_dep| cell_dep.into())
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl From<molecule_fiber::UdtArgInfo> for UdtArgInfo {
    fn from(udt_arg_info: molecule_fiber::UdtArgInfo) -> Self {
        UdtArgInfo {
            name: String::from_utf8(udt_arg_info.name().unpack()).expect("invalid name"),
            script: udt_arg_info.script().into(),
            auto_accept_amount: udt_arg_info
                .auto_accept_amount()
                .to_opt()
                .map(|x| x.unpack()),
            cell_deps: udt_arg_info
                .cell_deps()
                .into_iter()
                .map(|cell_dep| cell_dep.into())
                .collect(),
        }
    }
}

impl From<UdtCfgInfos> for molecule_fiber::UdtCfgInfos {
    fn from(udt_arg_info: UdtCfgInfos) -> Self {
        molecule_fiber::UdtCfgInfos::new_builder()
            .set(
                udt_arg_info
                    .0
                    .into_iter()
                    .map(|udt_arg_info| udt_arg_info.into())
                    .collect(),
            )
            .build()
    }
}

impl From<molecule_fiber::UdtCfgInfos> for UdtCfgInfos {
    fn from(udt_arg_infos: molecule_fiber::UdtCfgInfos) -> Self {
        UdtCfgInfos(
            udt_arg_infos
                .into_iter()
                .map(|udt_arg_info| udt_arg_info.into())
                .collect(),
        )
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
            .timestamp(node_announcement.version.pack())
            .node_id(node_announcement.node_id.into())
            .alias(u8_32_as_byte_32(&node_announcement.alias.0))
            .chain_hash(node_announcement.chain_hash.into())
            .auto_accept_min_ckb_funding_amount(
                node_announcement.auto_accept_min_ckb_funding_amount.pack(),
            )
            .udt_cfg_infos(node_announcement.udt_cfg_infos.into())
            .address(
                BytesVec::new_builder()
                    .set(
                        node_announcement
                            .addresses
                            .into_iter()
                            .map(|address| address.to_vec().pack())
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
            version: node_announcement.timestamp().unpack(),
            node_id: node_announcement.node_id().try_into()?,
            chain_hash: node_announcement.chain_hash().into(),
            auto_accept_min_ckb_funding_amount: node_announcement
                .auto_accept_min_ckb_funding_amount()
                .unpack(),
            alias: AnnouncedNodeName::from_slice(node_announcement.alias().as_slice())
                .map_err(|e| Error::AnyHow(anyhow!("Invalid alias: {}", e)))?,
            udt_cfg_infos: node_announcement.udt_cfg_infos().into(),
            addresses: node_announcement
                .address()
                .into_iter()
                .map(|address| MultiAddr::try_from(address.raw_data()))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelAnnouncement {
    pub node1_signature: Option<EcdsaSignature>,
    pub node2_signature: Option<EcdsaSignature>,
    // Signature signed by the funding transaction output public key.
    pub ckb_signature: Option<SchnorrSignature>,
    // Tentatively using 64 bits for features. May change the type later while developing.
    // rust-lightning uses a Vec<u8> here.
    pub features: u64,
    pub chain_hash: Hash256,
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    pub node1_id: Pubkey,
    pub node2_id: Pubkey,
    // The aggregated public key of the funding transaction output.
    pub ckb_key: XOnlyPublicKey,
    // The total capacity of the channel.
    pub capacity: u128,
    // UDT script
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
}

impl ChannelAnnouncement {
    pub fn new_unsigned(
        node1_pubkey: &Pubkey,
        node2_pubkey: &Pubkey,
        channel_outpoint: OutPoint,
        chain_hash: Hash256,
        ckb_pubkey: &XOnlyPublicKey,
        capacity: u128,
        udt_type_script: Option<Script>,
    ) -> Self {
        Self {
            node1_signature: None,
            node2_signature: None,
            ckb_signature: None,
            features: Default::default(),
            chain_hash,
            channel_outpoint,
            node1_id: *node1_pubkey,
            node2_id: *node2_pubkey,
            ckb_key: *ckb_pubkey,
            capacity,
            udt_type_script,
        }
    }

    pub fn is_signed(&self) -> bool {
        self.node1_signature.is_some()
            && self.node2_signature.is_some()
            && self.ckb_signature.is_some()
    }

    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_announcement = Self {
            node1_signature: None,
            node2_signature: None,
            ckb_signature: None,
            features: self.features,
            chain_hash: self.chain_hash,
            channel_outpoint: self.channel_outpoint.clone(),
            node1_id: self.node1_id,
            node2_id: self.node2_id,
            ckb_key: self.ckb_key,
            capacity: self.capacity,
            udt_type_script: self.udt_type_script.clone(),
        };
        deterministically_hash(&unsigned_announcement)
    }
}

impl From<ChannelAnnouncement> for molecule_fiber::ChannelAnnouncement {
    fn from(channel_announcement: ChannelAnnouncement) -> Self {
        molecule_fiber::ChannelAnnouncement::new_builder()
            .node1_signature(
                channel_announcement
                    .node1_signature
                    .expect("channel announcement signed")
                    .into(),
            )
            .node2_signature(
                channel_announcement
                    .node2_signature
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
            .node1_id(channel_announcement.node1_id.into())
            .node2_id(channel_announcement.node2_id.into())
            .capacity(channel_announcement.capacity.pack())
            .udt_type_script(channel_announcement.udt_type_script.pack())
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
            node1_signature: Some(channel_announcement.node1_signature().try_into()?),
            node2_signature: Some(channel_announcement.node2_signature().try_into()?),
            ckb_signature: Some(channel_announcement.ckb_signature().try_into()?),
            features: channel_announcement.features().unpack(),
            capacity: channel_announcement.capacity().unpack(),
            chain_hash: channel_announcement.chain_hash().into(),
            channel_outpoint: channel_announcement.channel_outpoint(),
            udt_type_script: channel_announcement.udt_type_script().to_opt(),
            node1_id: channel_announcement.node1_id().try_into()?,
            node2_id: channel_announcement.node2_id().try_into()?,
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
    pub version: u64,
    // Currently only the first bit is used to indicate the direction of the channel.
    // If it is 0, it means this channel message is from node 1 (thus applies to tlcs
    // sent from node 2 to node 1). Otherwise, it is from node 2.
    pub message_flags: u32,
    // Currently only the first bit is used to indicate if the channel is disabled.
    // If the first bit is set, the channel is disabled.
    pub channel_flags: u32,
    pub tlc_expiry_delta: u64,
    pub tlc_minimum_value: u128,
    pub tlc_maximum_value: u128,
    pub tlc_fee_proportional_millionths: u128,
}

impl ChannelUpdate {
    pub fn new_unsigned(
        chain_hash: Hash256,
        channel_outpoint: OutPoint,
        timestamp: u64,
        message_flags: u32,
        channel_flags: u32,
        tlc_expiry_delta: u64,
        tlc_minimum_value: u128,
        tlc_maximum_value: u128,
        tlc_fee_proportional_millionths: u128,
    ) -> Self {
        Self {
            signature: None,
            chain_hash,
            channel_outpoint,
            version: timestamp,
            message_flags,
            channel_flags,
            tlc_expiry_delta,
            tlc_minimum_value,
            tlc_maximum_value,
            tlc_fee_proportional_millionths,
        }
    }

    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_update = ChannelUpdate {
            signature: None,
            chain_hash: self.chain_hash,
            channel_outpoint: self.channel_outpoint.clone(),
            version: self.version,
            message_flags: self.message_flags,
            channel_flags: self.channel_flags,
            tlc_expiry_delta: self.tlc_expiry_delta,
            tlc_minimum_value: self.tlc_minimum_value,
            tlc_maximum_value: self.tlc_maximum_value,
            tlc_fee_proportional_millionths: self.tlc_fee_proportional_millionths,
        };
        deterministically_hash(&unsigned_update)
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
            .timestamp(channel_update.version.pack())
            .message_flags(channel_update.message_flags.pack())
            .channel_flags(channel_update.channel_flags.pack())
            .tlc_expiry_delta(channel_update.tlc_expiry_delta.pack())
            .tlc_minimum_value(channel_update.tlc_minimum_value.pack())
            .tlc_maximum_value(channel_update.tlc_maximum_value.pack())
            .tlc_fee_proportional_millionths(channel_update.tlc_fee_proportional_millionths.pack())
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
            version: channel_update.timestamp().unpack(),
            message_flags: channel_update.message_flags().unpack(),
            channel_flags: channel_update.channel_flags().unpack(),
            tlc_expiry_delta: channel_update.tlc_expiry_delta().unpack(),
            tlc_minimum_value: channel_update.tlc_minimum_value().unpack(),
            tlc_maximum_value: channel_update.tlc_maximum_value().unpack(),
            tlc_fee_proportional_millionths: channel_update
                .tlc_fee_proportional_millionths()
                .unpack(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum FiberQueryInformation {
    GetBroadcastMessages(GetBroadcastMessages),
    GetBroadcastMessagesResult(GetBroadcastMessagesResult),
    QueryChannelsWithinBlockRange(QueryChannelsWithinBlockRange),
    QueryChannelsWithinBlockRangeResult(QueryChannelsWithinBlockRangeResult),
    QueryBroadcastMessagesWithinTimeRange(QueryBroadcastMessagesWithinTimeRange),
    QueryBroadcastMessagesWithinTimeRangeResult(QueryBroadcastMessagesWithinTimeRangeResult),
}

#[derive(Debug, Clone)]
pub enum FiberMessage {
    ChannelInitialization(OpenChannel),
    ChannelNormalOperation(FiberChannelMessage),
    BroadcastMessage(FiberBroadcastMessage),
    QueryInformation(FiberQueryInformation),
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

impl From<FiberBroadcastMessage> for molecule_fiber::BroadcastMessageUnion {
    fn from(fiber_broadcast_message: FiberBroadcastMessage) -> Self {
        match fiber_broadcast_message {
            FiberBroadcastMessage::NodeAnnouncement(node_announcement) => {
                molecule_fiber::BroadcastMessageUnion::NodeAnnouncement(node_announcement.into())
            }
            FiberBroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                molecule_fiber::BroadcastMessageUnion::ChannelAnnouncement(
                    channel_announcement.into(),
                )
            }
            FiberBroadcastMessage::ChannelUpdate(channel_update) => {
                molecule_fiber::BroadcastMessageUnion::ChannelUpdate(channel_update.into())
            }
        }
    }
}

impl TryFrom<molecule_fiber::BroadcastMessageUnion> for FiberBroadcastMessage {
    type Error = Error;

    fn try_from(
        fiber_broadcast_message: molecule_fiber::BroadcastMessageUnion,
    ) -> Result<Self, Self::Error> {
        match fiber_broadcast_message {
            molecule_fiber::BroadcastMessageUnion::NodeAnnouncement(node_announcement) => Ok(
                FiberBroadcastMessage::NodeAnnouncement(node_announcement.try_into()?),
            ),
            molecule_fiber::BroadcastMessageUnion::ChannelAnnouncement(channel_announcement) => Ok(
                FiberBroadcastMessage::ChannelAnnouncement(channel_announcement.try_into()?),
            ),
            molecule_fiber::BroadcastMessageUnion::ChannelUpdate(channel_update) => Ok(
                FiberBroadcastMessage::ChannelUpdate(channel_update.try_into()?),
            ),
        }
    }
}

impl From<FiberBroadcastMessage> for molecule_fiber::BroadcastMessage {
    fn from(fiber_broadcast_message: FiberBroadcastMessage) -> Self {
        molecule_fiber::BroadcastMessage::new_builder()
            .set(fiber_broadcast_message)
            .build()
    }
}

impl TryFrom<molecule_fiber::BroadcastMessage> for FiberBroadcastMessage {
    type Error = Error;

    fn try_from(
        fiber_broadcast_message: molecule_fiber::BroadcastMessage,
    ) -> Result<Self, Self::Error> {
        fiber_broadcast_message.to_enum().try_into()
    }
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

#[derive(Debug, Clone)]
pub struct NodeAnnouncementQuery {
    pub node_id: Pubkey,
    pub flags: u8,
}

impl From<NodeAnnouncementQuery> for molecule_fiber::NodeAnnouncementQuery {
    fn from(node_announcement_query: NodeAnnouncementQuery) -> Self {
        molecule_fiber::NodeAnnouncementQuery::new_builder()
            .node_id(node_announcement_query.node_id.into())
            .flags(node_announcement_query.flags.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::NodeAnnouncementQuery> for NodeAnnouncementQuery {
    type Error = Error;

    fn try_from(
        node_announcement_query: molecule_fiber::NodeAnnouncementQuery,
    ) -> Result<Self, Self::Error> {
        Ok(NodeAnnouncementQuery {
            node_id: node_announcement_query.node_id().try_into()?,
            flags: node_announcement_query.flags().into(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChannelAnnouncementQuery {
    pub channel_outpoint: OutPoint,
    pub flags: u8,
}

impl From<ChannelAnnouncementQuery> for molecule_fiber::ChannelAnnouncementQuery {
    fn from(channel_announcement_query: ChannelAnnouncementQuery) -> Self {
        molecule_fiber::ChannelAnnouncementQuery::new_builder()
            .channel_outpoint(channel_announcement_query.channel_outpoint)
            .flags(channel_announcement_query.flags.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::ChannelAnnouncementQuery> for ChannelAnnouncementQuery {
    type Error = Error;

    fn try_from(
        channel_announcement_query: molecule_fiber::ChannelAnnouncementQuery,
    ) -> Result<Self, Self::Error> {
        Ok(ChannelAnnouncementQuery {
            channel_outpoint: channel_announcement_query.channel_outpoint(),
            flags: channel_announcement_query.flags().into(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChannelUpdateQuery {
    pub channel_outpoint: OutPoint,
    pub flags: u8,
}

impl From<ChannelUpdateQuery> for molecule_fiber::ChannelUpdateQuery {
    fn from(channel_update_query: ChannelUpdateQuery) -> Self {
        molecule_fiber::ChannelUpdateQuery::new_builder()
            .channel_outpoint(channel_update_query.channel_outpoint)
            .flags(channel_update_query.flags.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::ChannelUpdateQuery> for ChannelUpdateQuery {
    type Error = Error;

    fn try_from(
        channel_update_query: molecule_fiber::ChannelUpdateQuery,
    ) -> Result<Self, Self::Error> {
        Ok(ChannelUpdateQuery {
            channel_outpoint: channel_update_query.channel_outpoint(),
            flags: channel_update_query.flags().into(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum FiberBroadcastMessageQuery {
    NodeAnnouncement(NodeAnnouncementQuery),
    ChannelAnnouncement(ChannelAnnouncementQuery),
    ChannelUpdate(ChannelUpdateQuery),
}

impl From<FiberBroadcastMessageQuery> for molecule_fiber::BroadcastMessageQuery {
    fn from(fiber_broadcast_message_query: FiberBroadcastMessageQuery) -> Self {
        molecule_fiber::BroadcastMessageQuery::new_builder()
            .set(fiber_broadcast_message_query)
            .build()
    }
}

impl TryFrom<molecule_fiber::BroadcastMessageQuery> for FiberBroadcastMessageQuery {
    type Error = Error;

    fn try_from(
        fiber_broadcast_message_query: molecule_fiber::BroadcastMessageQuery,
    ) -> Result<Self, Self::Error> {
        fiber_broadcast_message_query.to_enum().try_into()
    }
}

impl From<FiberBroadcastMessageQuery> for molecule_fiber::BroadcastMessageQueryUnion {
    fn from(fiber_broadcast_message_query: FiberBroadcastMessageQuery) -> Self {
        match fiber_broadcast_message_query {
            FiberBroadcastMessageQuery::NodeAnnouncement(node_announcement_query) => {
                molecule_fiber::BroadcastMessageQueryUnion::NodeAnnouncementQuery(
                    node_announcement_query.into(),
                )
            }
            FiberBroadcastMessageQuery::ChannelAnnouncement(channel_announcement_query) => {
                molecule_fiber::BroadcastMessageQueryUnion::ChannelAnnouncementQuery(
                    channel_announcement_query.into(),
                )
            }
            FiberBroadcastMessageQuery::ChannelUpdate(channel_update_query) => {
                molecule_fiber::BroadcastMessageQueryUnion::ChannelUpdateQuery(
                    channel_update_query.into(),
                )
            }
        }
    }
}

impl TryFrom<molecule_fiber::BroadcastMessageQueryUnion> for FiberBroadcastMessageQuery {
    type Error = Error;

    fn try_from(
        fiber_broadcast_message_query: molecule_fiber::BroadcastMessageQueryUnion,
    ) -> Result<Self, Self::Error> {
        match fiber_broadcast_message_query {
            molecule_fiber::BroadcastMessageQueryUnion::NodeAnnouncementQuery(
                node_announcement_query,
            ) => Ok(FiberBroadcastMessageQuery::NodeAnnouncement(
                node_announcement_query.try_into()?,
            )),
            molecule_fiber::BroadcastMessageQueryUnion::ChannelAnnouncementQuery(
                channel_announcement_query,
            ) => Ok(FiberBroadcastMessageQuery::ChannelAnnouncement(
                channel_announcement_query.try_into()?,
            )),
            molecule_fiber::BroadcastMessageQueryUnion::ChannelUpdateQuery(
                channel_update_query,
            ) => Ok(FiberBroadcastMessageQuery::ChannelUpdate(
                channel_update_query.try_into()?,
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GetBroadcastMessages {
    pub id: u64,
    pub queries: Vec<FiberBroadcastMessageQuery>,
}

impl From<GetBroadcastMessages> for molecule_fiber::GetBroadcastMessages {
    fn from(get_broadcast_messages: GetBroadcastMessages) -> Self {
        molecule_fiber::GetBroadcastMessages::new_builder()
            .id(get_broadcast_messages.id.pack())
            .queries(
                BroadcastMessageQueries::new_builder()
                    .set(
                        get_broadcast_messages
                            .queries
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::GetBroadcastMessages> for GetBroadcastMessages {
    type Error = Error;

    fn try_from(
        get_broadcast_messages: molecule_fiber::GetBroadcastMessages,
    ) -> Result<Self, Self::Error> {
        Ok(GetBroadcastMessages {
            id: get_broadcast_messages.id().unpack(),
            queries: get_broadcast_messages
                .queries()
                .into_iter()
                .map(|query| query.try_into())
                .collect::<Result<Vec<FiberBroadcastMessageQuery>, Error>>()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct GetBroadcastMessagesResult {
    pub id: u64,
    pub messages: Vec<FiberBroadcastMessage>,
}

impl From<GetBroadcastMessagesResult> for molecule_fiber::GetBroadcastMessagesResult {
    fn from(get_broadcast_messages_result: GetBroadcastMessagesResult) -> Self {
        molecule_fiber::GetBroadcastMessagesResult::new_builder()
            .id(get_broadcast_messages_result.id.pack())
            .messages(
                molecule_fiber::BroadcastMessages::new_builder()
                    .set(
                        get_broadcast_messages_result
                            .messages
                            .into_iter()
                            .map(|message| message.into())
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::GetBroadcastMessagesResult> for GetBroadcastMessagesResult {
    type Error = Error;

    fn try_from(
        get_broadcast_messages_result: molecule_fiber::GetBroadcastMessagesResult,
    ) -> Result<Self, Self::Error> {
        Ok(GetBroadcastMessagesResult {
            id: get_broadcast_messages_result.id().unpack(),
            messages: get_broadcast_messages_result
                .messages()
                .into_iter()
                .map(|message| message.try_into())
                .collect::<Result<Vec<FiberBroadcastMessage>, Error>>()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryChannelsWithinBlockRange {
    pub id: u64,
    pub chain_hash: Hash256,
    pub start_block: u64,
    pub end_block: u64,
}

impl From<QueryChannelsWithinBlockRange> for molecule_fiber::QueryChannelsWithinBlockRange {
    fn from(query_channels_within_block_range: QueryChannelsWithinBlockRange) -> Self {
        molecule_fiber::QueryChannelsWithinBlockRange::new_builder()
            .id(query_channels_within_block_range.id.pack())
            .chain_hash(query_channels_within_block_range.chain_hash.into())
            .start_block(query_channels_within_block_range.start_block.pack())
            .end_block(query_channels_within_block_range.end_block.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::QueryChannelsWithinBlockRange> for QueryChannelsWithinBlockRange {
    type Error = Error;

    fn try_from(
        query_channels_within_block_range: molecule_fiber::QueryChannelsWithinBlockRange,
    ) -> Result<Self, Self::Error> {
        Ok(QueryChannelsWithinBlockRange {
            id: query_channels_within_block_range.id().unpack(),
            chain_hash: query_channels_within_block_range.chain_hash().into(),
            start_block: query_channels_within_block_range.start_block().unpack(),
            end_block: query_channels_within_block_range.end_block().unpack(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryChannelsWithinBlockRangeResult {
    pub id: u64,
    pub next_block: u64,
    pub is_finished: bool,
    pub channels: Vec<OutPoint>,
}

impl From<QueryChannelsWithinBlockRangeResult>
    for molecule_fiber::QueryChannelsWithinBlockRangeResult
{
    fn from(query_channels_within_block_range_result: QueryChannelsWithinBlockRangeResult) -> Self {
        molecule_fiber::QueryChannelsWithinBlockRangeResult::new_builder()
            .id(query_channels_within_block_range_result.id.pack())
            .next_block(query_channels_within_block_range_result.next_block.pack())
            .is_finished(
                (if query_channels_within_block_range_result.is_finished {
                    1u8
                } else {
                    0
                })
                .into(),
            )
            .channels(
                molecule_fiber::OutPoints::new_builder()
                    .set(
                        query_channels_within_block_range_result
                            .channels
                            .into_iter()
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::QueryChannelsWithinBlockRangeResult>
    for QueryChannelsWithinBlockRangeResult
{
    type Error = Error;

    fn try_from(
        query_channels_within_block_range_result: molecule_fiber::QueryChannelsWithinBlockRangeResult,
    ) -> Result<Self, Self::Error> {
        Ok(QueryChannelsWithinBlockRangeResult {
            id: query_channels_within_block_range_result.id().unpack(),
            next_block: query_channels_within_block_range_result
                .next_block()
                .unpack(),
            is_finished: u8::from(query_channels_within_block_range_result.is_finished()) != 0u8,
            channels: query_channels_within_block_range_result
                .channels()
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryBroadcastMessagesWithinTimeRange {
    pub id: u64,
    pub chain_hash: Hash256,
    pub start_time: u64,
    pub end_time: u64,
}

impl From<QueryBroadcastMessagesWithinTimeRange>
    for molecule_fiber::QueryBroadcastMessagesWithinTimeRange
{
    fn from(
        query_broadcast_messages_within_time_range: QueryBroadcastMessagesWithinTimeRange,
    ) -> Self {
        molecule_fiber::QueryBroadcastMessagesWithinTimeRange::new_builder()
            .id(query_broadcast_messages_within_time_range.id.pack())
            .chain_hash(query_broadcast_messages_within_time_range.chain_hash.into())
            .start_time(query_broadcast_messages_within_time_range.start_time.pack())
            .end_time(query_broadcast_messages_within_time_range.end_time.pack())
            .build()
    }
}

impl TryFrom<molecule_fiber::QueryBroadcastMessagesWithinTimeRange>
    for QueryBroadcastMessagesWithinTimeRange
{
    type Error = Error;

    fn try_from(
        query_broadcast_messages_within_time_range: molecule_fiber::QueryBroadcastMessagesWithinTimeRange,
    ) -> Result<Self, Self::Error> {
        Ok(QueryBroadcastMessagesWithinTimeRange {
            id: query_broadcast_messages_within_time_range.id().unpack(),
            chain_hash: query_broadcast_messages_within_time_range
                .chain_hash()
                .into(),
            start_time: query_broadcast_messages_within_time_range
                .start_time()
                .unpack(),
            end_time: query_broadcast_messages_within_time_range
                .end_time()
                .unpack(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryBroadcastMessagesWithinTimeRangeResult {
    pub id: u64,
    pub next_time: u64,
    pub is_finished: bool,
    pub queries: Vec<FiberBroadcastMessageQuery>,
}

impl From<QueryBroadcastMessagesWithinTimeRangeResult>
    for molecule_fiber::QueryBroadcastMessagesWithinTimeRangeResult
{
    fn from(
        query_broadcast_messages_within_time_range_result: QueryBroadcastMessagesWithinTimeRangeResult,
    ) -> Self {
        molecule_fiber::QueryBroadcastMessagesWithinTimeRangeResult::new_builder()
            .id(query_broadcast_messages_within_time_range_result.id.pack())
            .next_time(
                query_broadcast_messages_within_time_range_result
                    .next_time
                    .pack(),
            )
            .is_finished(
                (if query_broadcast_messages_within_time_range_result.is_finished {
                    1u8
                } else {
                    0
                })
                .into(),
            )
            .queries(
                molecule_fiber::BroadcastMessageQueries::new_builder()
                    .set(
                        query_broadcast_messages_within_time_range_result
                            .queries
                            .into_iter()
                            .map(|query| query.into())
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::QueryBroadcastMessagesWithinTimeRangeResult>
    for QueryBroadcastMessagesWithinTimeRangeResult
{
    type Error = Error;

    fn try_from(
        query_broadcast_messages_within_time_range_result: molecule_fiber::QueryBroadcastMessagesWithinTimeRangeResult,
    ) -> Result<Self, Self::Error> {
        Ok(QueryBroadcastMessagesWithinTimeRangeResult {
            id: query_broadcast_messages_within_time_range_result
                .id()
                .unpack(),
            next_time: query_broadcast_messages_within_time_range_result
                .next_time()
                .unpack(),
            is_finished: u8::from(query_broadcast_messages_within_time_range_result.is_finished())
                != 0,
            queries: query_broadcast_messages_within_time_range_result
                .queries()
                .into_iter()
                .map(|message| message.try_into())
                .collect::<Result<Vec<_>, Error>>()?,
        })
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
            FiberMessage::QueryInformation(query) => match query {
                FiberQueryInformation::GetBroadcastMessages(get_broadcast_messages) => {
                    molecule_fiber::FiberMessageUnion::GetBroadcastMessages(
                        get_broadcast_messages.into(),
                    )
                }
                FiberQueryInformation::GetBroadcastMessagesResult(
                    get_broadcast_messages_result,
                ) => molecule_fiber::FiberMessageUnion::GetBroadcastMessagesResult(
                    get_broadcast_messages_result.into(),
                ),
                FiberQueryInformation::QueryChannelsWithinBlockRange(
                    query_channels_within_block_range,
                ) => molecule_fiber::FiberMessageUnion::QueryChannelsWithinBlockRange(
                    query_channels_within_block_range.into(),
                ),
                FiberQueryInformation::QueryChannelsWithinBlockRangeResult(
                    query_channels_within_block_range_result,
                ) => molecule_fiber::FiberMessageUnion::QueryChannelsWithinBlockRangeResult(
                    query_channels_within_block_range_result.into(),
                ),
                FiberQueryInformation::QueryBroadcastMessagesWithinTimeRange(
                    query_broadcast_messages_within_time_range,
                ) => molecule_fiber::FiberMessageUnion::QueryBroadcastMessagesWithinTimeRange(
                    query_broadcast_messages_within_time_range.into(),
                ),
                FiberQueryInformation::QueryBroadcastMessagesWithinTimeRangeResult(
                    query_broadcast_messages_within_time_range_result,
                ) => {
                    molecule_fiber::FiberMessageUnion::QueryBroadcastMessagesWithinTimeRangeResult(
                        query_broadcast_messages_within_time_range_result.into(),
                    )
                }
            },
        }
    }
}

impl TryFrom<molecule_fiber::FiberMessageUnion> for FiberMessage {
    type Error = Error;

    fn try_from(fiber_message: molecule_fiber::FiberMessageUnion) -> Result<Self, Self::Error> {
        Ok(match fiber_message {
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
            molecule_fiber::FiberMessageUnion::GetBroadcastMessages(get_broadcast_messages) => {
                FiberMessage::QueryInformation(FiberQueryInformation::GetBroadcastMessages(
                    get_broadcast_messages.try_into()?,
                ))
            }
            molecule_fiber::FiberMessageUnion::GetBroadcastMessagesResult(
                get_broadcast_messages_result,
            ) => FiberMessage::QueryInformation(FiberQueryInformation::GetBroadcastMessagesResult(
                get_broadcast_messages_result.try_into()?,
            )),
            molecule_fiber::FiberMessageUnion::QueryChannelsWithinBlockRange(
                query_channels_within_block_range,
            ) => FiberMessage::QueryInformation(
                FiberQueryInformation::QueryChannelsWithinBlockRange(
                    query_channels_within_block_range.try_into()?,
                ),
            ),
            molecule_fiber::FiberMessageUnion::QueryChannelsWithinBlockRangeResult(
                query_channels_within_block_range_result,
            ) => FiberMessage::QueryInformation(
                FiberQueryInformation::QueryChannelsWithinBlockRangeResult(
                    query_channels_within_block_range_result.try_into()?,
                ),
            ),
            molecule_fiber::FiberMessageUnion::QueryBroadcastMessagesWithinTimeRange(
                query_broadcast_messages_within_time_range,
            ) => FiberMessage::QueryInformation(
                FiberQueryInformation::QueryBroadcastMessagesWithinTimeRange(
                    query_broadcast_messages_within_time_range.try_into()?,
                ),
            ),
            molecule_fiber::FiberMessageUnion::QueryBroadcastMessagesWithinTimeRangeResult(
                query_broadcast_messages_within_time_range_result,
            ) => FiberMessage::QueryInformation(
                FiberQueryInformation::QueryBroadcastMessagesWithinTimeRangeResult(
                    query_broadcast_messages_within_time_range_result.try_into()?,
                ),
            ),
        })
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
        fiber_message.to_enum().try_into()
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

#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaymentHopData {
    pub payment_hash: Hash256,
    // this is only specified in the last hop in the keysend mode
    pub preimage: Option<Hash256>,
    pub tlc_hash_algorithm: HashAlgorithm,
    pub amount: u128,
    pub expiry: u64,
    pub next_hop: Option<Pubkey>,
    #[serde_as(as = "Option<EntityHex>")]
    pub channel_outpoint: Option<OutPoint>,
}

/// Trait for hop data
pub trait HopData: Sized {
    const PACKET_DATA_LEN: usize;
    fn next_hop(&self) -> Option<Pubkey>;
    fn assoc_data(&self) -> Option<Vec<u8>>;
    fn serialize(&self) -> Vec<u8>;
    fn deserialize(data: &[u8]) -> Option<Self>;
}

impl HopData for PaymentHopData {
    const PACKET_DATA_LEN: usize = 1300;

    fn next_hop(&self) -> Option<Pubkey> {
        self.next_hop.clone()
    }

    fn assoc_data(&self) -> Option<Vec<u8>> {
        Some(self.payment_hash.as_ref().to_vec())
    }

    fn serialize(&self) -> Vec<u8> {
        deterministically_serialize(self)
    }

    fn deserialize(data: &[u8]) -> Option<Self> {
        serde_json::from_slice(data).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OnionPacket<T> {
    _phantom: PhantomData<T>,
    // The encrypted packet
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeeledOnionPacket<T> {
    // The decrypted hop data for the current hop
    pub current: T,
    // The packet for the next hop
    pub next: Option<OnionPacket<T>>,
}

pub type PaymentOnionPacket = OnionPacket<PaymentHopData>;
pub type PeeledPaymentOnionPacket = PeeledOnionPacket<PaymentHopData>;

impl<T> OnionPacket<T> {
    pub fn new(data: Vec<u8>) -> Self {
        OnionPacket {
            _phantom: PhantomData,
            data,
        }
    }
}

impl<T: HopData> OnionPacket<T> {
    /// Peels the next layer of the onion packet using the privkey of the current node.
    ///
    /// Returns errors when:
    /// - This is the packet for the last hop.
    /// - Fail to peel the packet using the given private key.
    pub fn peel<C: Verification>(
        self,
        privkey: &Privkey,
        assoc_data: Option<&[u8]>,
        secp_ctx: &Secp256k1<C>,
    ) -> Result<PeeledOnionPacket<T>, Error> {
        let sphinx_packet = fiber_sphinx::OnionPacket::from_bytes(self.data)
            .map_err(|err| Error::OnionPacket(err.into()))?;

        let (new_current, new_next) = sphinx_packet
            .peel(&privkey.0, assoc_data, secp_ctx, get_hop_data_len)
            .map_err(|err| Error::OnionPacket(err.into()))?;

        let current = unpack_hop_data(&new_current)
            .ok_or_else(|| Error::OnionPacket(OnionPacketError::InvalidHopData))?;
        // All zeros hmac indicates the last hop
        let next = new_next
            .hmac
            .iter()
            .any(|b| *b != 0)
            .then(|| OnionPacket::new(new_next.into_bytes()));

        Ok(PeeledOnionPacket { current, next })
    }
}

impl<T: HopData> PeeledOnionPacket<T> {
    /// - `hops_info`: the first is the instruction for the origin node itself.
    ///                Remaining elements are for each node to receive the packet.
    pub fn create<C: Signing>(
        session_key: Privkey,
        mut hops_infos: Vec<T>,
        secp_ctx: &Secp256k1<C>,
    ) -> Result<Self, Error> {
        if hops_infos.is_empty() {
            return Err(Error::OnionPacket(SphinxError::HopsIsEmpty.into()));
        }

        let hops_path: Vec<PublicKey> = hops_infos
            .iter()
            .map(HopData::next_hop)
            .take_while(Option::is_some)
            .map(|opt| opt.expect("must be some").into())
            .collect();

        // Add length as the header
        let hops_data = hops_infos.iter().skip(1).map(pack_hop_data).collect();

        let current = hops_infos.swap_remove(0);
        let assoc_data = current.assoc_data();

        let next = if !hops_path.is_empty() {
            Some(OnionPacket::new(
                fiber_sphinx::OnionPacket::create(
                    session_key.into(),
                    hops_path,
                    hops_data,
                    assoc_data,
                    T::PACKET_DATA_LEN,
                    secp_ctx,
                )
                .map_err(|err| Error::OnionPacket(err.into()))?
                .into_bytes(),
            ))
        } else {
            None
        };

        Ok(PeeledOnionPacket { current, next })
    }

    /// Returns true if this is the peeled packet for the last destination.
    pub fn is_last(&self) -> bool {
        self.next.is_none()
    }

    /// Peels the next layer of the onion packet using the privkey of the current node.
    ///
    /// Returns errors when:
    /// - This is the packet for the last hop.
    /// - Fail to peel the packet using the given private key.
    pub fn peel<C: Verification>(
        self,
        privkey: &Privkey,
        secp_ctx: &Secp256k1<C>,
    ) -> Result<Self, Error> {
        let next = self
            .next
            .ok_or_else(|| Error::OnionPacket(OnionPacketError::PeelingLastHop))?;

        next.peel(privkey, self.current.assoc_data().as_deref(), secp_ctx)
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut res = pack_hop_data(&self.current);
        if let Some(ref next) = self.next {
            res.append(&mut (next.data.clone()));
        }
        res
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, Error> {
        let current_len = get_hop_data_len(data)
            .ok_or_else(|| Error::OnionPacket(OnionPacketError::InvalidHopData))?;
        let current = unpack_hop_data(data)
            .ok_or_else(|| Error::OnionPacket(OnionPacketError::InvalidHopData))?;
        let next = if current_len < data.len() {
            Some(OnionPacket::new(data[current_len..].to_vec()))
        } else {
            None
        };
        Ok(Self { current, next })
    }
}

const HOP_DATA_HEAD_LEN: usize = std::mem::size_of::<u64>();

/// TODO: when JSON is replaced, this function may return `data` directly.
fn pack_hop_data<T: HopData>(hop_data: &T) -> Vec<u8> {
    let mut serialized = hop_data.serialize();
    // A temporary solution to prepend the length as the header
    let mut packed = (serialized.len() as u64).to_be_bytes().to_vec();
    packed.append(&mut serialized);
    packed
}

/// TODO: when JSON is replaced, this function may return `data` directly.
fn unpack_hop_data<T: HopData>(buf: &[u8]) -> Option<T> {
    let len = get_hop_data_len(buf)?;
    if buf.len() < len {
        return None;
    }
    T::deserialize(&buf[HOP_DATA_HEAD_LEN..len])
}

/// TODO: when JSON is replaced, this function may return `data` directly.
fn get_hop_data_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < HOP_DATA_HEAD_LEN {
        return None;
    }
    Some(
        u64::from_be_bytes(
            buf[0..HOP_DATA_HEAD_LEN]
                .try_into()
                .expect("u64 from slice"),
        ) as usize
            + HOP_DATA_HEAD_LEN,
    )
}
