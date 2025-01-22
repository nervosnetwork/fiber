use super::channel::{ChannelFlags, ChannelTlcInfo, ProcessingChannelError};
use super::config::AnnouncedNodeName;
use super::gen::fiber::{
    self as molecule_fiber, ChannelUpdateOpt, PaymentPreimageOpt, PubNonce as Byte66, PubkeyOpt,
    TlcErrDataOpt, UdtCellDeps, Uint128Opt,
};
use super::gen::gossip::{self as molecule_gossip};
use super::hash_algorithm::{HashAlgorithm, UnknownHashAlgorithmError};
use super::network::get_chain_hash;
use super::r#gen::fiber::PubNonceOpt;
use super::serde_utils::{EntityHex, SliceHex};
use crate::ckb::config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtScript};
use crate::ckb::contracts::get_udt_whitelist;
use ckb_jsonrpc_types::CellOutput;
use num_enum::IntoPrimitive;
use num_enum::TryFromPrimitive;
use std::convert::TryFrom;
use std::fmt::Debug;

use anyhow::anyhow;
use ckb_types::{
    core::FeeRate,
    packed::{Byte32 as MByte32, BytesVec, OutPoint, Script, Transaction},
    prelude::{Pack, Unpack},
};
use core::fmt::{self, Formatter};
use fiber_sphinx::{OnionErrorPacket, SphinxError};
use molecule::prelude::{Builder, Byte, Entity};
use musig2::errors::DecodeError;
use musig2::secp::{Point, Scalar};
use musig2::{BinaryEncoding, PartialSignature, PubNonce};
use once_cell::sync::OnceCell;
use ractor::concurrency::Duration;
use secp256k1::{
    ecdsa::Signature as Secp256k1Signature, schnorr::Signature as SchnorrSignature, All, PublicKey,
    Secp256k1, SecretKey, Signing,
};
use secp256k1::{Verification, XOnlyPublicKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::cmp::Ordering;
use std::fmt::Display;
use std::marker::PhantomData;
use std::str::FromStr;
use strum::{AsRefStr, EnumString};
use tentacle::multiaddr::MultiAddr;
use tentacle::secio::PeerId;
use thiserror::Error;
use tracing::{error, trace};

pub fn secp256k1_instance() -> &'static Secp256k1<All> {
    static INSTANCE: OnceCell<Secp256k1<All>> = OnceCell::new();
    INSTANCE.get_or_init(Secp256k1::new)
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

/// A 256-bit hash digest, used as identifier of channnel, payment, transaction hash etc.
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

fn u8_32_as_byte_32(value: &[u8; 32]) -> MByte32 {
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

/// The public key for a Node
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

const PUBKEY_SIZE: usize = 33;
impl Pubkey {
    pub const fn serialization_len() -> usize {
        PUBKEY_SIZE
    }

    pub fn serialize(&self) -> [u8; PUBKEY_SIZE] {
        PublicKey::from(self).serialize()
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self, secp256k1::Error> {
        PublicKey::from_slice(slice).map(Into::into)
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
        Ok(Self::from_slice(pubkey)?)
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

impl From<XOnlyPublicKey> for molecule_gossip::SchnorrXOnlyPubkey {
    fn from(pk: XOnlyPublicKey) -> molecule_gossip::SchnorrXOnlyPubkey {
        molecule_gossip::SchnorrXOnlyPubkey::new_builder()
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

impl TryFrom<molecule_gossip::SchnorrXOnlyPubkey> for XOnlyPublicKey {
    type Error = Error;

    fn try_from(pubkey: molecule_gossip::SchnorrXOnlyPubkey) -> Result<Self, Self::Error> {
        let pubkey = pubkey.as_slice();
        XOnlyPublicKey::from_slice(pubkey).map_err(Into::into)
    }
}

impl From<SchnorrSignature> for molecule_gossip::SchnorrSignature {
    fn from(signature: SchnorrSignature) -> molecule_gossip::SchnorrSignature {
        molecule_gossip::SchnorrSignature::new_builder()
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

impl TryFrom<molecule_gossip::SchnorrSignature> for SchnorrSignature {
    type Error = Error;

    fn try_from(signature: molecule_gossip::SchnorrSignature) -> Result<Self, Self::Error> {
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
    pub funding_pubkey: Pubkey,
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
    MByte32::from_slice(partial_signature.serialize().as_ref()).expect("[Byte; 32] from [u8; 32]")
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
    pub witnesses: Vec<Vec<u8>>,
}

impl From<TxSignatures> for molecule_fiber::TxSignatures {
    fn from(tx_signatures: TxSignatures) -> Self {
        molecule_fiber::TxSignatures::new_builder()
            .channel_id(tx_signatures.channel_id.into())
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
    pub commitment_tx_partial_signature: PartialSignature,
}

impl From<TxComplete> for molecule_fiber::TxComplete {
    fn from(tx_complete: TxComplete) -> Self {
        molecule_fiber::TxComplete::new_builder()
            .channel_id(tx_complete.channel_id.into())
            .commitment_tx_partial_signature(partial_signature_to_molecule(
                tx_complete.commitment_tx_partial_signature,
            ))
            .build()
    }
}

impl TryFrom<molecule_fiber::TxComplete> for TxComplete {
    type Error = Error;

    fn try_from(tx_complete: molecule_fiber::TxComplete) -> Result<Self, Self::Error> {
        Ok(TxComplete {
            channel_id: tx_complete.channel_id().into(),
            commitment_tx_partial_signature: PartialSignature::from_slice(
                tx_complete.commitment_tx_partial_signature().as_slice(),
            )
            .map_err(|e| anyhow!(e))?,
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

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpdateTlcInfo {
    pub channel_id: Hash256,
    pub timestamp: u64,
    pub channel_flags: ChannelUpdateChannelFlags,
    pub tlc_expiry_delta: u64,
    pub tlc_minimum_value: u128,
    pub tlc_maximum_value: u128,
    pub tlc_fee_proportional_millionths: u128,
}

impl From<UpdateTlcInfo> for molecule_fiber::UpdateTlcInfo {
    fn from(update_tlc_info: UpdateTlcInfo) -> Self {
        molecule_fiber::UpdateTlcInfo::new_builder()
            .channel_id(update_tlc_info.channel_id.into())
            .timestamp(update_tlc_info.timestamp.pack())
            .channel_flags(update_tlc_info.channel_flags.bits().pack())
            .tlc_expiry_delta(update_tlc_info.tlc_expiry_delta.pack())
            .tlc_minimum_value(update_tlc_info.tlc_minimum_value.pack())
            .tlc_maximum_value(update_tlc_info.tlc_maximum_value.pack())
            .tlc_fee_proportional_millionths(update_tlc_info.tlc_fee_proportional_millionths.pack())
            .build()
    }
}

impl From<molecule_fiber::UpdateTlcInfo> for UpdateTlcInfo {
    fn from(update_tlc_info: molecule_fiber::UpdateTlcInfo) -> Self {
        UpdateTlcInfo {
            channel_id: update_tlc_info.channel_id().into(),
            timestamp: update_tlc_info.timestamp().unpack(),
            channel_flags: ChannelUpdateChannelFlags::from_bits_truncate(
                update_tlc_info.channel_flags().unpack(),
            ),
            tlc_expiry_delta: update_tlc_info.tlc_expiry_delta().unpack(),
            tlc_minimum_value: update_tlc_info.tlc_minimum_value().unpack(),
            tlc_maximum_value: update_tlc_info.tlc_maximum_value().unpack(),
            tlc_fee_proportional_millionths: update_tlc_info
                .tlc_fee_proportional_millionths()
                .unpack(),
        }
    }
}

impl From<UpdateTlcInfo> for ChannelTlcInfo {
    fn from(update_tlc_info: UpdateTlcInfo) -> Self {
        ChannelTlcInfo {
            timestamp: update_tlc_info.timestamp,
            enabled: !update_tlc_info
                .channel_flags
                .contains(ChannelUpdateChannelFlags::DISABLED),
            tlc_expiry_delta: update_tlc_info.tlc_expiry_delta,
            tlc_minimum_value: update_tlc_info.tlc_minimum_value,
            tlc_maximum_value: update_tlc_info.tlc_maximum_value,
            tlc_fee_proportional_millionths: update_tlc_info.tlc_fee_proportional_millionths,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AddTlc {
    pub channel_id: Hash256,
    pub tlc_id: u64,
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    pub onion_packet: Option<PaymentOnionPacket>,
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
            .onion_packet(
                add_tlc
                    .onion_packet
                    .map(|p| p.into_bytes())
                    .unwrap_or_default()
                    .pack(),
            )
            .build()
    }
}

impl TryFrom<molecule_fiber::AddTlc> for AddTlc {
    type Error = Error;

    fn try_from(add_tlc: molecule_fiber::AddTlc) -> Result<Self, Self::Error> {
        let onion_packet_bytes: Vec<u8> = add_tlc.onion_packet().unpack();
        let onion_packet =
            (onion_packet_bytes.len() > 0).then(|| PaymentOnionPacket::new(onion_packet_bytes));
        Ok(AddTlc {
            onion_packet,
            channel_id: add_tlc.channel_id().into(),
            tlc_id: add_tlc.tlc_id().unpack(),
            amount: add_tlc.amount().unpack(),
            payment_hash: add_tlc.payment_hash().into(),
            expiry: add_tlc.expiry().unpack(),
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
    pub revocation_partial_signature: PartialSignature,
    pub commitment_tx_partial_signature: PartialSignature,
    pub next_per_commitment_point: Pubkey,
}

impl From<RevokeAndAck> for molecule_fiber::RevokeAndAck {
    fn from(revoke_and_ack: RevokeAndAck) -> Self {
        molecule_fiber::RevokeAndAck::new_builder()
            .channel_id(revoke_and_ack.channel_id.into())
            .revocation_partial_signature(partial_signature_to_molecule(
                revoke_and_ack.revocation_partial_signature,
            ))
            .commitment_tx_partial_signature(partial_signature_to_molecule(
                revoke_and_ack.commitment_tx_partial_signature,
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
            revocation_partial_signature: PartialSignature::from_slice(
                revoke_and_ack.revocation_partial_signature().as_slice(),
            )
            .map_err(|e| anyhow!(e))?,
            commitment_tx_partial_signature: PartialSignature::from_slice(
                revoke_and_ack.commitment_tx_partial_signature().as_slice(),
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
        node_id: Pubkey,
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

impl Display for TlcErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error_code_as_str())
    }
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
        node_id: Pubkey,
        channel_outpoint: OutPoint,
        channel_update: Option<ChannelUpdate>,
    ) -> Self {
        TlcErr {
            error_code: error_code.into(),
            extra_data: Some(TlcErrData::ChannelFailed {
                node_id,
                channel_outpoint,
                channel_update,
            }),
        }
    }

    pub fn error_node_id(&self) -> Option<Pubkey> {
        match &self.extra_data {
            Some(TlcErrData::NodeFailed { node_id }) => Some(*node_id),
            Some(TlcErrData::ChannelFailed { node_id, .. }) => Some(*node_id),
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

    pub fn error_code_as_u16(&self) -> u16 {
        self.error_code.into()
    }

    pub fn set_extra_data(&mut self, extra_data: TlcErrData) {
        self.extra_data = Some(extra_data);
    }

    fn serialize(&self) -> Vec<u8> {
        molecule_fiber::TlcErr::from(self.clone())
            .as_slice()
            .to_vec()
    }

    fn deserialize(data: &[u8]) -> Option<Self> {
        molecule_fiber::TlcErr::from_slice(data)
            .map(TlcErr::from)
            .ok()
    }
}

impl TryFrom<TlcErrData> for molecule_fiber::TlcErrData {
    type Error = Error;

    fn try_from(tlc_err_data: TlcErrData) -> Result<Self, Self::Error> {
        match tlc_err_data {
            TlcErrData::ChannelFailed {
                channel_outpoint,
                channel_update,
                node_id,
            } => Ok(molecule_fiber::ChannelFailed::new_builder()
                .channel_outpoint(channel_outpoint.into())
                .channel_update(
                    ChannelUpdateOpt::new_builder()
                        .set(channel_update.map(|x| x.into()))
                        .build(),
                )
                .node_id(node_id.into())
                .build()
                .into()),
            TlcErrData::NodeFailed { node_id } => Ok(molecule_fiber::NodeFailed::new_builder()
                .node_id(node_id.into())
                .build()
                .into()),
        }
    }
}

impl TryFrom<molecule_fiber::TlcErrData> for TlcErrData {
    type Error = Error;

    fn try_from(tlc_err_data: molecule_fiber::TlcErrData) -> Result<Self, Self::Error> {
        match tlc_err_data.to_enum() {
            molecule_fiber::TlcErrDataUnion::ChannelFailed(channel_failed) => {
                Ok(TlcErrData::ChannelFailed {
                    channel_outpoint: channel_failed.channel_outpoint().into(),
                    channel_update: channel_failed
                        .channel_update()
                        .to_opt()
                        .map(|x| x.try_into().unwrap()),
                    node_id: channel_failed.node_id().try_into()?,
                })
            }
            molecule_fiber::TlcErrDataUnion::NodeFailed(node_failed) => {
                Ok(TlcErrData::NodeFailed {
                    node_id: node_failed.node_id().try_into()?,
                })
            }
        }
    }
}

impl From<TlcErr> for molecule_fiber::TlcErr {
    fn from(tlc_err: TlcErr) -> Self {
        molecule_fiber::TlcErr::new_builder()
            .error_code(tlc_err.error_code_as_u16().into())
            .extra_data(
                TlcErrDataOpt::new_builder()
                    .set(tlc_err.extra_data.map(|data| data.try_into().unwrap()))
                    .build(),
            )
            .build()
    }
}

impl From<molecule_fiber::TlcErr> for TlcErr {
    fn from(tlc_err: molecule_fiber::TlcErr) -> Self {
        TlcErr {
            error_code: {
                let code: u16 = tlc_err.error_code().into();
                TlcErrorCode::try_from(code).expect("tlc_errror_code failed")
            },
            extra_data: tlc_err
                .extra_data()
                .to_opt()
                .map(|data| data.try_into().unwrap()),
        }
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

pub const NO_SHARED_SECRET: [u8; 32] = [0u8; 32];
const NO_ERROR_PACKET_HMAC: [u8; 32] = [0u8; 32];

/// Always decrypting 27 times so the erroring node cannot learn its relative position in the route
/// by performing a timing analysis if the sender were to retry the same route multiple times.
const ERROR_DECODING_PASSES: usize = 27;

impl TlcErrPacket {
    /// Erring node creates the error packet using the shared secret used in forwarding onion packet.
    /// Use all zeros for the origin node.
    pub fn new(tlc_fail: TlcErr, shared_secret: &[u8; 32]) -> Self {
        let payload = tlc_fail.serialize();

        let onion_packet = if shared_secret != &NO_SHARED_SECRET {
            OnionErrorPacket::create(shared_secret, payload)
        } else {
            OnionErrorPacket::concat(NO_ERROR_PACKET_HMAC.clone(), payload)
        }
        .into_bytes();
        TlcErrPacket { onion_packet }
    }

    pub fn is_plaintext(&self) -> bool {
        self.onion_packet.len() >= 32 && self.onion_packet[0..32] == NO_ERROR_PACKET_HMAC
    }

    /// Intermediate node backwards the error to the previous hop using the shared secret used in forwarding
    /// the onion packet.
    pub fn backward(self, shared_secret: &[u8; 32]) -> Self {
        if !self.is_plaintext() {
            let onion_packet = OnionErrorPacket::from_bytes(self.onion_packet)
                .xor_cipher_stream(shared_secret)
                .into_bytes();
            TlcErrPacket { onion_packet }
        } else {
            // If it is not encrypted, just send back as it is.
            self
        }
    }

    pub fn decode(&self, session_key: &[u8; 32], hops_public_keys: Vec<Pubkey>) -> Option<TlcErr> {
        if self.is_plaintext() {
            let error = TlcErr::deserialize(&self.onion_packet[32..]);
            if error.is_some() {
                return error;
            }
        }

        let hops_public_keys: Vec<PublicKey> =
            hops_public_keys.iter().map(|k| k.0.clone()).collect();
        let session_key = SecretKey::from_slice(session_key).inspect_err(|err|
            error!(target: "fnn::fiber::types::TlcErrPacket", "decode session_key error={} key={}", err, hex::encode(session_key))
        ).ok()?;
        OnionErrorPacket::from_bytes(self.onion_packet.clone())
            .parse(hops_public_keys, session_key, TlcErr::deserialize)
            .map(|(error, hop_index)| {
                for _ in hop_index..ERROR_DECODING_PASSES {
                    OnionErrorPacket::from_bytes(self.onion_packet.clone())
                        .xor_cipher_stream(&NO_SHARED_SECRET);
                }
                error
            })
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
#[derive(
    Debug,
    Copy,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    AsRefStr,
    EnumString,
    TryFromPrimitive,
    IntoPrimitive,
)]
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
    InvalidOnionError = BADONION | PERM | 25,
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
            | TlcErrorCode::ExpiryTooSoon => true,
            _ => false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoveTlcReason {
    RemoveTlcFulfill(RemoveTlcFulfill),
    RemoveTlcFail(TlcErrPacket),
}

impl Debug for RemoveTlcReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RemoveTlcReason::RemoveTlcFulfill(_fulfill) => {
                write!(f, "RemoveTlcFulfill")
            }
            RemoveTlcReason::RemoveTlcFail(_fail) => {
                write!(f, "RemoveTlcFail")
            }
        }
    }
}

impl RemoveTlcReason {
    /// Intermediate node backwards the error to the previous hop using the shared secret used in forwarding
    /// the onion packet.
    pub fn backward(self, shared_secret: &[u8; 32]) -> Self {
        match self {
            RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill) => {
                RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill)
            }
            RemoveTlcReason::RemoveTlcFail(remove_tlc_fail) => {
                RemoveTlcReason::RemoveTlcFail(remove_tlc_fail.backward(shared_secret))
            }
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct ForwardTlcResult {
    pub channel_id: Hash256,
    pub payment_hash: Hash256,
    pub tlc_id: u64,
    pub error_info: Option<(ProcessingChannelError, TlcErr)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct NodeAnnouncement {
    // Signature to this message, may be empty the message is not signed yet.
    pub signature: Option<EcdsaSignature>,
    // Tentatively using 64 bits for features. May change the type later while developing.
    // rust-lightning uses a Vec<u8> here.
    pub features: u64,
    // Timestamp for current NodeAnnouncement. Later updates should have larger timestamp.
    pub timestamp: u64,
    pub node_id: Pubkey,
    // Must be a valid utf-8 string of length maximal length 32 bytes.
    // If the length is less than 32 bytes, it will be padded with 0.
    // If the length is more than 32 bytes, it should be truncated.
    pub node_name: AnnouncedNodeName,
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
        node_name: AnnouncedNodeName,
        addresses: Vec<MultiAddr>,
        node_id: Pubkey,
        timestamp: u64,
        auto_accept_min_ckb_funding_amount: u64,
    ) -> Self {
        Self {
            signature: None,
            features: Default::default(),
            timestamp,
            node_id,
            node_name,
            chain_hash: get_chain_hash(),
            addresses,
            auto_accept_min_ckb_funding_amount,
            udt_cfg_infos: get_udt_whitelist(),
        }
    }

    pub fn new(
        node_name: AnnouncedNodeName,
        addresses: Vec<MultiAddr>,
        private_key: &Privkey,
        timestamp: u64,
        auto_accept_min_ckb_funding_amount: u64,
    ) -> NodeAnnouncement {
        let mut unsigned = NodeAnnouncement::new_unsigned(
            node_name,
            addresses,
            private_key.pubkey(),
            timestamp,
            auto_accept_min_ckb_funding_amount,
        );
        unsigned.signature = Some(private_key.sign(unsigned.message_to_sign()));
        unsigned
    }

    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_announcement = NodeAnnouncement {
            signature: None,
            features: self.features,
            timestamp: self.timestamp,
            node_id: self.node_id,
            node_name: self.node_name,
            chain_hash: self.chain_hash,
            addresses: self.addresses.clone(),
            auto_accept_min_ckb_funding_amount: self.auto_accept_min_ckb_funding_amount,
            udt_cfg_infos: self.udt_cfg_infos.clone(),
        };
        deterministically_hash(&molecule_gossip::NodeAnnouncement::from(
            unsigned_announcement,
        ))
    }

    pub fn peer_id(&self) -> PeerId {
        PeerId::from_public_key(&self.node_id.into())
    }

    pub fn cursor(&self) -> Cursor {
        Cursor::new(
            self.timestamp,
            BroadcastMessageID::NodeAnnouncement(self.node_id),
        )
    }

    pub fn verify(&self) -> bool {
        let message = self.message_to_sign();
        match self.signature {
            Some(ref signature) => signature.verify(&self.node_id, &message),
            _ => false,
        }
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

impl From<NodeAnnouncement> for molecule_gossip::NodeAnnouncement {
    fn from(node_announcement: NodeAnnouncement) -> Self {
        let builder = molecule_gossip::NodeAnnouncement::new_builder()
            .features(node_announcement.features.pack())
            .timestamp(node_announcement.timestamp.pack())
            .node_id(node_announcement.node_id.into())
            .node_name(u8_32_as_byte_32(&node_announcement.node_name.0))
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
            );

        let builder = if let Some(signature) = node_announcement.signature {
            builder.signature(signature.into())
        } else {
            builder
        };

        builder.build()
    }
}

impl TryFrom<molecule_gossip::NodeAnnouncement> for NodeAnnouncement {
    type Error = Error;

    fn try_from(node_announcement: molecule_gossip::NodeAnnouncement) -> Result<Self, Self::Error> {
        Ok(NodeAnnouncement {
            signature: Some(node_announcement.signature().try_into()?),
            features: node_announcement.features().unpack(),
            timestamp: node_announcement.timestamp().unpack(),
            node_id: node_announcement.node_id().try_into()?,
            chain_hash: node_announcement.chain_hash().into(),
            auto_accept_min_ckb_funding_amount: node_announcement
                .auto_accept_min_ckb_funding_amount()
                .unpack(),
            node_name: AnnouncedNodeName::from_slice(node_announcement.node_name().as_slice())
                .map_err(|e| Error::AnyHow(anyhow!("Invalid node_name: {}", e)))?,
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
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
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
        ckb_pubkey: &XOnlyPublicKey,
        capacity: u128,
        udt_type_script: Option<Script>,
    ) -> Self {
        Self {
            node1_signature: None,
            node2_signature: None,
            ckb_signature: None,
            features: Default::default(),
            chain_hash: get_chain_hash(),
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
        deterministically_hash(&molecule_gossip::ChannelAnnouncement::from(
            unsigned_announcement,
        ))
    }

    pub fn out_point(&self) -> &OutPoint {
        &self.channel_outpoint
    }
}

impl From<ChannelAnnouncement> for molecule_gossip::ChannelAnnouncement {
    fn from(channel_announcement: ChannelAnnouncement) -> Self {
        let builder = molecule_gossip::ChannelAnnouncement::new_builder()
            .features(channel_announcement.features.pack())
            .chain_hash(channel_announcement.chain_hash.into())
            .channel_outpoint(channel_announcement.channel_outpoint)
            .node1_id(channel_announcement.node1_id.into())
            .node2_id(channel_announcement.node2_id.into())
            .capacity(channel_announcement.capacity.pack())
            .udt_type_script(channel_announcement.udt_type_script.pack())
            .ckb_key(channel_announcement.ckb_key.into());

        let builder = if let Some(signature) = channel_announcement.node1_signature {
            builder.node1_signature(signature.into())
        } else {
            builder
        };

        let builder = if let Some(signature) = channel_announcement.node2_signature {
            builder.node2_signature(signature.into())
        } else {
            builder
        };

        let builder = if let Some(signature) = channel_announcement.ckb_signature {
            builder.ckb_signature(signature.into())
        } else {
            builder
        };

        builder.build()
    }
}

impl TryFrom<molecule_gossip::ChannelAnnouncement> for ChannelAnnouncement {
    type Error = Error;

    fn try_from(
        channel_announcement: molecule_gossip::ChannelAnnouncement,
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
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct ChannelUpdate {
    // Signature of the node that wants to update the channel information.
    pub signature: Option<EcdsaSignature>,
    pub chain_hash: Hash256,
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    pub timestamp: u64,
    // Currently only the first bit is used to indicate the direction of the channel.
    // If it is 0, it means this channel message is from node 1 (thus applies to tlcs
    // sent from node 2 to node 1). Otherwise, it is from node 2.
    pub message_flags: ChannelUpdateMessageFlags,
    // Currently only the first bit is used to indicate if the channel is disabled.
    // If the first bit is set, the channel is disabled.
    pub channel_flags: ChannelUpdateChannelFlags,
    pub tlc_expiry_delta: u64,
    pub tlc_minimum_value: u128,
    pub tlc_fee_proportional_millionths: u128,
}

impl ChannelUpdate {
    pub fn new_unsigned(
        channel_outpoint: OutPoint,
        timestamp: u64,
        message_flags: ChannelUpdateMessageFlags,
        channel_flags: ChannelUpdateChannelFlags,
        tlc_expiry_delta: u64,
        tlc_minimum_value: u128,
        tlc_fee_proportional_millionths: u128,
    ) -> Self {
        // To avoid having the same timestamp for both channel updates, we will use an even
        // timestamp number for node1 and an odd timestamp number for node2.
        let timestamp = if message_flags.contains(ChannelUpdateMessageFlags::UPDATE_OF_NODE2) {
            timestamp | 1u64
        } else {
            timestamp & !1u64
        };
        Self {
            signature: None,
            chain_hash: get_chain_hash(),
            channel_outpoint,
            timestamp,
            message_flags,
            channel_flags,
            tlc_expiry_delta,
            tlc_minimum_value,
            tlc_fee_proportional_millionths,
        }
    }

    pub fn message_to_sign(&self) -> [u8; 32] {
        let unsigned_update = ChannelUpdate {
            signature: None,
            chain_hash: self.chain_hash,
            channel_outpoint: self.channel_outpoint.clone(),
            timestamp: self.timestamp,
            message_flags: self.message_flags,
            channel_flags: self.channel_flags,
            tlc_expiry_delta: self.tlc_expiry_delta,
            tlc_minimum_value: self.tlc_minimum_value,
            tlc_fee_proportional_millionths: self.tlc_fee_proportional_millionths,
        };
        deterministically_hash(&molecule_fiber::ChannelUpdate::from(unsigned_update))
    }

    pub fn is_update_of_node_1(&self) -> bool {
        !self.is_update_of_node_2()
    }

    pub fn is_update_of_node_2(&self) -> bool {
        self.message_flags
            .contains(ChannelUpdateMessageFlags::UPDATE_OF_NODE2)
    }

    pub fn is_disabled(&self) -> bool {
        self.channel_flags
            .contains(ChannelUpdateChannelFlags::DISABLED)
    }

    pub fn cursor(&self) -> Cursor {
        Cursor::new(
            self.timestamp,
            BroadcastMessageID::ChannelUpdate(self.channel_outpoint.clone()),
        )
    }
}

impl From<ChannelUpdate> for molecule_fiber::ChannelUpdate {
    fn from(channel_update: ChannelUpdate) -> Self {
        let builder = molecule_fiber::ChannelUpdate::new_builder()
            .chain_hash(channel_update.chain_hash.into())
            .channel_outpoint(channel_update.channel_outpoint)
            .timestamp(channel_update.timestamp.pack())
            .message_flags(channel_update.message_flags.bits().pack())
            .channel_flags(channel_update.channel_flags.bits().pack())
            .tlc_expiry_delta(channel_update.tlc_expiry_delta.pack())
            .tlc_minimum_value(channel_update.tlc_minimum_value.pack())
            .tlc_fee_proportional_millionths(channel_update.tlc_fee_proportional_millionths.pack());

        let builder = if let Some(signature) = channel_update.signature {
            builder.signature(signature.into())
        } else {
            builder
        };

        builder.build()
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
            message_flags: ChannelUpdateMessageFlags::from_bits_truncate(
                channel_update.message_flags().unpack(),
            ),
            channel_flags: ChannelUpdateChannelFlags::from_bits_truncate(
                channel_update.channel_flags().unpack(),
            ),
            tlc_expiry_delta: channel_update.tlc_expiry_delta().unpack(),
            tlc_minimum_value: channel_update.tlc_minimum_value().unpack(),
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
}

#[derive(Debug, Clone)]
pub enum FiberMessage {
    ChannelInitialization(OpenChannel),
    ChannelNormalOperation(FiberChannelMessage),
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
    UpdateTlcInfo(UpdateTlcInfo),
    AddTlc(AddTlc),
    RevokeAndAck(RevokeAndAck),
    RemoveTlc(RemoveTlc),
    ReestablishChannel(ReestablishChannel),
    AnnouncementSignatures(AnnouncementSignatures),
}

impl Display for FiberChannelMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FiberChannelMessage::AcceptChannel(_) => write!(f, "AcceptChannel"),
            FiberChannelMessage::CommitmentSigned(_) => write!(f, "CommitmentSigned"),
            FiberChannelMessage::TxSignatures(_) => write!(f, "TxSignatures"),
            FiberChannelMessage::ChannelReady(_) => write!(f, "ChannelReady"),
            FiberChannelMessage::TxUpdate(_) => write!(f, "TxUpdate"),
            FiberChannelMessage::TxComplete(_) => write!(f, "TxComplete"),
            FiberChannelMessage::TxAbort(_) => write!(f, "TxAbort"),
            FiberChannelMessage::TxInitRBF(_) => write!(f, "TxInitRBF"),
            FiberChannelMessage::TxAckRBF(_) => write!(f, "TxAckRBF"),
            FiberChannelMessage::Shutdown(_) => write!(f, "Shutdown"),
            FiberChannelMessage::ClosingSigned(_) => write!(f, "ClosingSigned"),
            FiberChannelMessage::UpdateTlcInfo(_) => write!(f, "UpdateTlcInfo"),
            FiberChannelMessage::AddTlc(_) => write!(f, "AddTlc"),
            FiberChannelMessage::RevokeAndAck(_) => write!(f, "RevokeAndAck"),
            FiberChannelMessage::RemoveTlc(_) => write!(f, "RemoveTlc"),
            FiberChannelMessage::ReestablishChannel(_) => write!(f, "ReestablishChannel"),
            FiberChannelMessage::AnnouncementSignatures(_) => write!(f, "AnnouncementSignatures"),
        }
    }
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
            FiberChannelMessage::UpdateTlcInfo(update_tlc_info) => update_tlc_info.channel_id,
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
pub enum GossipMessage {
    BroadcastMessagesFilter(BroadcastMessagesFilter),
    BroadcastMessagesFilterResult(BroadcastMessagesFilterResult),
    GetBroadcastMessages(GetBroadcastMessages),
    GetBroadcastMessagesResult(GetBroadcastMessagesResult),
    QueryBroadcastMessages(QueryBroadcastMessages),
    QueryBroadcastMessagesResult(QueryBroadcastMessagesResult),
}

impl GossipMessage {
    pub fn to_molecule_bytes(self) -> molecule::bytes::Bytes {
        molecule_gossip::GossipMessage::from(self).as_bytes()
    }

    pub fn from_molecule_slice(data: &[u8]) -> Result<Self, Error> {
        molecule_gossip::GossipMessage::from_slice(data)
            .map_err(Into::into)
            .and_then(TryInto::try_into)
    }
}

impl From<GossipMessage> for molecule_gossip::GossipMessageUnion {
    fn from(gossip_message: GossipMessage) -> Self {
        match gossip_message {
            GossipMessage::BroadcastMessagesFilter(broadcast_messages_filter) => {
                molecule_gossip::GossipMessageUnion::BroadcastMessagesFilter(
                    broadcast_messages_filter.into(),
                )
            }
            GossipMessage::BroadcastMessagesFilterResult(broadcast_messages_filter_result) => {
                molecule_gossip::GossipMessageUnion::BroadcastMessagesFilterResult(
                    broadcast_messages_filter_result.into(),
                )
            }
            GossipMessage::GetBroadcastMessages(get_broadcast_messages) => {
                molecule_gossip::GossipMessageUnion::GetBroadcastMessages(
                    get_broadcast_messages.into(),
                )
            }
            GossipMessage::GetBroadcastMessagesResult(get_broadcast_messages_result) => {
                molecule_gossip::GossipMessageUnion::GetBroadcastMessagesResult(
                    get_broadcast_messages_result.into(),
                )
            }
            GossipMessage::QueryBroadcastMessages(query_broadcast_messages) => {
                molecule_gossip::GossipMessageUnion::QueryBroadcastMessages(
                    query_broadcast_messages.into(),
                )
            }
            GossipMessage::QueryBroadcastMessagesResult(query_broadcast_messages_result) => {
                molecule_gossip::GossipMessageUnion::QueryBroadcastMessagesResult(
                    query_broadcast_messages_result.into(),
                )
            }
        }
    }
}

impl From<GossipMessage> for molecule_gossip::GossipMessage {
    fn from(gossip_message: GossipMessage) -> Self {
        molecule_gossip::GossipMessage::new_builder()
            .set(gossip_message)
            .build()
    }
}

impl TryFrom<molecule_gossip::GossipMessageUnion> for GossipMessage {
    type Error = Error;

    fn try_from(gossip_message: molecule_gossip::GossipMessageUnion) -> Result<Self, Self::Error> {
        match gossip_message {
            molecule_gossip::GossipMessageUnion::BroadcastMessagesFilter(
                broadcast_messages_filter,
            ) => Ok(GossipMessage::BroadcastMessagesFilter(
                broadcast_messages_filter.try_into()?,
            )),
            molecule_gossip::GossipMessageUnion::BroadcastMessagesFilterResult(
                broadcast_messages_result,
            ) => Ok(GossipMessage::BroadcastMessagesFilterResult(
                broadcast_messages_result.try_into()?,
            )),
            molecule_gossip::GossipMessageUnion::GetBroadcastMessages(get_broadcast_messages) => {
                Ok(GossipMessage::GetBroadcastMessages(
                    get_broadcast_messages.try_into()?,
                ))
            }
            molecule_gossip::GossipMessageUnion::GetBroadcastMessagesResult(
                broadcast_messages_result,
            ) => Ok(GossipMessage::GetBroadcastMessagesResult(
                broadcast_messages_result.try_into()?,
            )),
            molecule_gossip::GossipMessageUnion::QueryBroadcastMessages(
                query_broadcast_messages,
            ) => Ok(GossipMessage::QueryBroadcastMessages(
                query_broadcast_messages.try_into()?,
            )),
            molecule_gossip::GossipMessageUnion::QueryBroadcastMessagesResult(
                broadcast_messages_result,
            ) => Ok(GossipMessage::QueryBroadcastMessagesResult(
                broadcast_messages_result.try_into()?,
            )),
        }
    }
}

impl TryFrom<molecule_gossip::GossipMessage> for GossipMessage {
    type Error = Error;
    fn try_from(value: molecule_gossip::GossipMessage) -> Result<Self, Self::Error> {
        value.to_enum().try_into()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub enum BroadcastMessage {
    NodeAnnouncement(NodeAnnouncement),
    ChannelAnnouncement(ChannelAnnouncement),
    ChannelUpdate(ChannelUpdate),
}

impl BroadcastMessage {
    pub fn create_broadcast_messages_filter_result(&self) -> GossipMessage {
        GossipMessage::BroadcastMessagesFilterResult(BroadcastMessagesFilterResult {
            messages: vec![self.clone()],
        })
    }

    pub fn cursor(&self) -> Option<Cursor> {
        match self {
            BroadcastMessage::ChannelAnnouncement(_) => None,
            BroadcastMessage::ChannelUpdate(channel_update) => Some(channel_update.cursor()),
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                Some(node_announcement.cursor())
            }
        }
    }

    pub(crate) fn message_id(&self) -> BroadcastMessageID {
        match self {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                BroadcastMessageID::NodeAnnouncement(node_announcement.node_id)
            }
            BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                BroadcastMessageID::ChannelAnnouncement(
                    channel_announcement.channel_outpoint.clone(),
                )
            }
            BroadcastMessage::ChannelUpdate(channel_update) => {
                BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone())
            }
        }
    }

    pub(crate) fn timestamp(&self) -> Option<u64> {
        match self {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                Some(node_announcement.timestamp)
            }
            BroadcastMessage::ChannelAnnouncement(_) => None,
            BroadcastMessage::ChannelUpdate(channel_update) => Some(channel_update.timestamp),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChannelOnchainInfo {
    pub timestamp: u64,
    pub first_output: CellOutput,
}

// Augment the broadcast message with timestamp so that we can easily obtain the cursor of the message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BroadcastMessageWithTimestamp {
    NodeAnnouncement(NodeAnnouncement),
    ChannelAnnouncement(u64, ChannelAnnouncement),
    ChannelUpdate(ChannelUpdate),
}

impl BroadcastMessageWithTimestamp {
    pub fn chain_hash(&self) -> Hash256 {
        match self {
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                node_announcement.chain_hash
            }
            BroadcastMessageWithTimestamp::ChannelAnnouncement(_, channel_announcement) => {
                channel_announcement.chain_hash
            }
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                channel_update.chain_hash
            }
        }
    }

    pub fn cursor(&self) -> Cursor {
        match self {
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => Cursor::new(
                node_announcement.timestamp,
                BroadcastMessageID::NodeAnnouncement(node_announcement.node_id),
            ),
            BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement) => {
                Cursor::new(
                    *timestamp,
                    BroadcastMessageID::ChannelAnnouncement(
                        channel_announcement.channel_outpoint.clone(),
                    ),
                )
            }
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => Cursor::new(
                channel_update.timestamp,
                BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone()),
            ),
        }
    }

    pub fn timestamp(&self) -> u64 {
        match self {
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                node_announcement.timestamp
            }
            BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, _) => *timestamp,
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                channel_update.timestamp
            }
        }
    }

    pub fn message_id(&self) -> BroadcastMessageID {
        match self {
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                BroadcastMessageID::NodeAnnouncement(node_announcement.node_id)
            }
            BroadcastMessageWithTimestamp::ChannelAnnouncement(_, channel_announcement) => {
                BroadcastMessageID::ChannelAnnouncement(
                    channel_announcement.channel_outpoint.clone(),
                )
            }
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone())
            }
        }
    }

    pub fn create_broadcast_messages_filter_result(&self) -> BroadcastMessagesFilterResult {
        BroadcastMessagesFilterResult {
            messages: vec![BroadcastMessage::from(self.clone())],
        }
    }
}

impl Ord for BroadcastMessage {
    fn cmp(&self, other: &Self) -> Ordering {
        self.message_id()
            .cmp(&other.message_id())
            .then(self.timestamp().cmp(&other.timestamp()))
    }
}

impl PartialOrd for BroadcastMessage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<(BroadcastMessage, Option<ChannelOnchainInfo>)> for BroadcastMessageWithTimestamp {
    fn from(
        (broadcast_message, channel_onchain_info): (BroadcastMessage, Option<ChannelOnchainInfo>),
    ) -> Self {
        match broadcast_message {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement)
            }
            BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                let timestamp = channel_onchain_info
                    .expect("Channel onchain info is required for channel announcement")
                    .timestamp;
                BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement)
            }
            BroadcastMessage::ChannelUpdate(channel_update) => {
                BroadcastMessageWithTimestamp::ChannelUpdate(channel_update)
            }
        }
    }
}

impl From<BroadcastMessageWithTimestamp> for BroadcastMessage {
    fn from(broadcast_message_with_timestamp: BroadcastMessageWithTimestamp) -> Self {
        match broadcast_message_with_timestamp {
            BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                BroadcastMessage::NodeAnnouncement(node_announcement)
            }
            BroadcastMessageWithTimestamp::ChannelAnnouncement(_, channel_announcement) => {
                BroadcastMessage::ChannelAnnouncement(channel_announcement)
            }
            BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                BroadcastMessage::ChannelUpdate(channel_update)
            }
        }
    }
}

impl From<(BroadcastMessage, u64)> for BroadcastMessageWithTimestamp {
    fn from((broadcast_message, timestamp): (BroadcastMessage, u64)) -> Self {
        match broadcast_message {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                debug_assert_eq!(timestamp, node_announcement.timestamp);
                BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement)
            }
            BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                BroadcastMessageWithTimestamp::ChannelAnnouncement(timestamp, channel_announcement)
            }
            BroadcastMessage::ChannelUpdate(channel_update) => {
                debug_assert_eq!(timestamp, channel_update.timestamp);
                BroadcastMessageWithTimestamp::ChannelUpdate(channel_update)
            }
        }
    }
}

impl From<BroadcastMessage> for molecule_gossip::BroadcastMessageUnion {
    fn from(fiber_broadcast_message: BroadcastMessage) -> Self {
        match fiber_broadcast_message {
            BroadcastMessage::NodeAnnouncement(node_announcement) => {
                molecule_gossip::BroadcastMessageUnion::NodeAnnouncement(node_announcement.into())
            }
            BroadcastMessage::ChannelAnnouncement(channel_announcement) => {
                molecule_gossip::BroadcastMessageUnion::ChannelAnnouncement(
                    channel_announcement.into(),
                )
            }
            BroadcastMessage::ChannelUpdate(channel_update) => {
                molecule_gossip::BroadcastMessageUnion::ChannelUpdate(channel_update.into())
            }
        }
    }
}

impl TryFrom<molecule_gossip::BroadcastMessageUnion> for BroadcastMessage {
    type Error = Error;

    fn try_from(
        fiber_broadcast_message: molecule_gossip::BroadcastMessageUnion,
    ) -> Result<Self, Self::Error> {
        match fiber_broadcast_message {
            molecule_gossip::BroadcastMessageUnion::NodeAnnouncement(node_announcement) => Ok(
                BroadcastMessage::NodeAnnouncement(node_announcement.try_into()?),
            ),
            molecule_gossip::BroadcastMessageUnion::ChannelAnnouncement(channel_announcement) => {
                Ok(BroadcastMessage::ChannelAnnouncement(
                    channel_announcement.try_into()?,
                ))
            }
            molecule_gossip::BroadcastMessageUnion::ChannelUpdate(channel_update) => {
                Ok(BroadcastMessage::ChannelUpdate(channel_update.try_into()?))
            }
        }
    }
}

impl From<BroadcastMessage> for molecule_gossip::BroadcastMessage {
    fn from(fiber_broadcast_message: BroadcastMessage) -> Self {
        molecule_gossip::BroadcastMessage::new_builder()
            .set(fiber_broadcast_message)
            .build()
    }
}

impl TryFrom<molecule_gossip::BroadcastMessage> for BroadcastMessage {
    type Error = Error;

    fn try_from(
        fiber_broadcast_message: molecule_gossip::BroadcastMessage,
    ) -> Result<Self, Self::Error> {
        fiber_broadcast_message.to_enum().try_into()
    }
}

/// Note that currently we only allow querying for one type of broadcast message at a time.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BroadcastMessageQueryFlags {
    ChannelAnnouncement,
    ChannelUpdateOfNode1,
    ChannelUpdateOfNode2,
    NodeAnnouncementNode1,
    NodeAnnouncementNode2,
}

impl From<BroadcastMessageQueryFlags> for u8 {
    fn from(value: BroadcastMessageQueryFlags) -> u8 {
        match value {
            // The numbers are chosen to be powers of 2 so that they can be combined using bitwise OR.
            // But we disallow querying for multiple types of broadcast messages at a time for now.
            BroadcastMessageQueryFlags::ChannelAnnouncement => 0,
            BroadcastMessageQueryFlags::ChannelUpdateOfNode1 => 1,
            BroadcastMessageQueryFlags::ChannelUpdateOfNode2 => 2,
            BroadcastMessageQueryFlags::NodeAnnouncementNode1 => 4,
            BroadcastMessageQueryFlags::NodeAnnouncementNode2 => 8,
        }
    }
}

impl TryFrom<u8> for BroadcastMessageQueryFlags {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(BroadcastMessageQueryFlags::ChannelAnnouncement),
            1 => Ok(BroadcastMessageQueryFlags::ChannelUpdateOfNode1),
            2 => Ok(BroadcastMessageQueryFlags::ChannelUpdateOfNode2),
            4 => Ok(BroadcastMessageQueryFlags::NodeAnnouncementNode1),
            8 => Ok(BroadcastMessageQueryFlags::NodeAnnouncementNode2),
            _ => Err(Error::AnyHow(anyhow!(
                "Invalid broadcast message query flags: {}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BroadcastMessageQuery {
    pub channel_outpoint: OutPoint,
    pub flags: BroadcastMessageQueryFlags,
}

impl From<BroadcastMessageQuery> for molecule_gossip::BroadcastMessageQuery {
    fn from(broadcast_message_query: BroadcastMessageQuery) -> Self {
        molecule_gossip::BroadcastMessageQuery::new_builder()
            .channel_outpoint(broadcast_message_query.channel_outpoint)
            .flags(u8::from(broadcast_message_query.flags).into())
            .build()
    }
}

impl TryFrom<molecule_gossip::BroadcastMessageQuery> for BroadcastMessageQuery {
    type Error = Error;

    fn try_from(
        broadcast_message_query: molecule_gossip::BroadcastMessageQuery,
    ) -> Result<Self, Self::Error> {
        Ok(BroadcastMessageQuery {
            channel_outpoint: broadcast_message_query.channel_outpoint(),
            flags: u8::from(broadcast_message_query.flags()).try_into()?,
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum BroadcastMessageID {
    ChannelAnnouncement(OutPoint),
    ChannelUpdate(OutPoint),
    NodeAnnouncement(Pubkey),
}

// We need to implement Ord for BroadcastMessageID to make sure that a ChannelUpdate message is always ordered after ChannelAnnouncement,
// so that we can use it as the sorting key in fn prune_messages_to_be_saved to simplify the logic.
impl Ord for BroadcastMessageID {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (
                BroadcastMessageID::ChannelAnnouncement(outpoint1),
                BroadcastMessageID::ChannelAnnouncement(outpoint2),
            ) => outpoint1.cmp(outpoint2),
            (
                BroadcastMessageID::ChannelUpdate(outpoint1),
                BroadcastMessageID::ChannelUpdate(outpoint2),
            ) => outpoint1.cmp(outpoint2),
            (
                BroadcastMessageID::NodeAnnouncement(pubkey1),
                BroadcastMessageID::NodeAnnouncement(pubkey2),
            ) => pubkey1.cmp(pubkey2),
            (BroadcastMessageID::NodeAnnouncement(_), _) => Ordering::Less,
            (BroadcastMessageID::ChannelUpdate(_), _) => Ordering::Greater,
            (
                BroadcastMessageID::ChannelAnnouncement(_),
                BroadcastMessageID::NodeAnnouncement(_),
            ) => Ordering::Greater,
            (BroadcastMessageID::ChannelAnnouncement(_), BroadcastMessageID::ChannelUpdate(_)) => {
                Ordering::Less
            }
        }
    }
}

impl PartialOrd for BroadcastMessageID {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// 1 byte for message type, 36 bytes for message id
const MESSAGE_ID_SIZE: usize = 1 + 36;
// 8 bytes for timestamp, MESSAGE_ID_SIZE bytes for message id
pub(crate) const CURSOR_SIZE: usize = 8 + MESSAGE_ID_SIZE;

impl BroadcastMessageID {
    pub(crate) fn to_bytes(&self) -> [u8; MESSAGE_ID_SIZE] {
        let mut result = [0u8; MESSAGE_ID_SIZE];
        match self {
            BroadcastMessageID::ChannelAnnouncement(channel_outpoint) => {
                result[0] = 0;
                result[1..].copy_from_slice(&channel_outpoint.as_bytes());
            }
            BroadcastMessageID::ChannelUpdate(channel_outpoint) => {
                result[0] = 1;
                result[1..].copy_from_slice(&channel_outpoint.as_bytes());
            }
            BroadcastMessageID::NodeAnnouncement(node_id) => {
                result[0] = 2;
                let node_id = node_id.serialize();
                result[1..1 + node_id.len()].copy_from_slice(&node_id);
            }
        };
        result
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != MESSAGE_ID_SIZE {
            return Err(Error::AnyHow(anyhow!(
                "Invalid message id size: {}",
                bytes.len()
            )));
        }
        match bytes[0] {
            0 => Ok(BroadcastMessageID::ChannelAnnouncement(
                OutPoint::from_slice(&bytes[1..])?,
            )),
            1 => Ok(BroadcastMessageID::ChannelUpdate(OutPoint::from_slice(
                &bytes[1..],
            )?)),
            2 => Ok(BroadcastMessageID::NodeAnnouncement(Pubkey::from_slice(
                &bytes[1..1 + Pubkey::serialization_len()],
            )?)),
            _ => Err(Error::AnyHow(anyhow!(
                "Invalid message id type: {}",
                bytes[0]
            ))),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Cursor {
    pub(crate) timestamp: u64,
    pub(crate) message_id: BroadcastMessageID,
}

impl Cursor {
    pub fn new(timestamp: u64, message_id: BroadcastMessageID) -> Self {
        Self {
            timestamp,
            message_id,
        }
    }

    /// Create a new cursor which is the same as the current cursor but with a smaller timestamp.
    /// This is useful when we want to query messages back from this cursor for a certain duration.
    /// For example, sometimes we aren't particularly sure about whether we have already seen all messages before
    /// the latest cursor in our broadcast message store, because it is possible that we that messages are not
    /// saved in strictly increasing order of their timestamps. In this case, we can go back for some time
    /// (e.g. one week) to make sure we don't miss any messages.
    pub fn go_back_for_some_time(&self, duration: Duration) -> Self {
        let current_timestamp = self.timestamp;
        let duration_millis = duration.as_millis() as u64;
        if current_timestamp > duration_millis {
            Self {
                timestamp: current_timestamp - duration_millis,
                message_id: self.message_id.clone(),
            }
        } else {
            Default::default()
        }
    }

    pub fn to_bytes(&self) -> [u8; 45] {
        self.timestamp
            .to_be_bytes()
            .into_iter()
            .chain(self.message_id.to_bytes())
            .collect::<Vec<_>>()
            .try_into()
            .expect("Must serialize cursor to 45 bytes")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != CURSOR_SIZE {
            return Err(Error::AnyHow(anyhow!(
                "Invalid cursor size: {}, want {}",
                bytes.len(),
                CURSOR_SIZE
            )));
        }
        let timestamp = u64::from_be_bytes(bytes[..8].try_into().expect("Cursor timestamp to u64"));
        let message_id = BroadcastMessageID::from_bytes(&bytes[8..])?;
        Ok(Cursor {
            timestamp,
            message_id,
        })
    }

    // A dummy cursor with the maximum timestamp and a dummy message id. This is useful when
    // we want to create a cursor after which none of the messages should be included.
    pub fn max() -> Self {
        Self {
            timestamp: u64::MAX,
            message_id: BroadcastMessageID::ChannelAnnouncement(OutPoint::default()),
        }
    }

    pub fn is_max(&self) -> bool {
        self.timestamp == u64::MAX
    }
}

impl Ord for Cursor {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_bytes().cmp(&other.to_bytes())
    }
}

impl PartialOrd for Cursor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Default for Cursor {
    fn default() -> Self {
        molecule_gossip::Cursor::new_builder()
            .set([Byte::new(0); CURSOR_SIZE])
            .build()
            .try_into()
            .expect("Default cursor")
    }
}

impl From<Cursor> for molecule_gossip::Cursor {
    fn from(cursor: Cursor) -> Self {
        let serialized = cursor
            .timestamp
            .to_be_bytes()
            .into_iter()
            .chain(cursor.message_id.to_bytes())
            .map(Byte::new)
            .collect::<Vec<_>>()
            .try_into()
            .expect("Must serialize cursor to 45 bytes");

        molecule_gossip::Cursor::new_builder()
            .set(serialized)
            .build()
    }
}

impl TryFrom<molecule_gossip::Cursor> for Cursor {
    type Error = Error;

    fn try_from(cursor: molecule_gossip::Cursor) -> Result<Self, Self::Error> {
        let slice = cursor.as_slice();
        if slice.len() != CURSOR_SIZE {
            return Err(Error::AnyHow(anyhow!(
                "Invalid cursor size: {}, want {}",
                slice.len(),
                CURSOR_SIZE
            )));
        }
        let timestamp = u64::from_be_bytes(slice[..8].try_into().expect("Cursor timestamp to u64"));
        let message_id = BroadcastMessageID::from_bytes(&slice[8..])?;
        Ok(Cursor {
            timestamp,
            message_id,
        })
    }
}

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

#[derive(Debug, Clone)]
pub struct BroadcastMessagesFilter {
    pub chain_hash: Hash256,
    pub after_cursor: Cursor,
}

impl From<BroadcastMessagesFilter> for molecule_gossip::BroadcastMessagesFilter {
    fn from(broadcast_messages_filter: BroadcastMessagesFilter) -> Self {
        molecule_gossip::BroadcastMessagesFilter::new_builder()
            .chain_hash(broadcast_messages_filter.chain_hash.into())
            .after_cursor(broadcast_messages_filter.after_cursor.into())
            .build()
    }
}

impl TryFrom<molecule_gossip::BroadcastMessagesFilter> for BroadcastMessagesFilter {
    type Error = Error;

    fn try_from(
        broadcast_messages_filter: molecule_gossip::BroadcastMessagesFilter,
    ) -> Result<Self, Self::Error> {
        Ok(BroadcastMessagesFilter {
            chain_hash: broadcast_messages_filter.chain_hash().into(),
            after_cursor: broadcast_messages_filter.after_cursor().try_into()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct BroadcastMessagesFilterResult {
    pub messages: Vec<BroadcastMessage>,
}

impl BroadcastMessagesFilterResult {
    pub fn new(messages: Vec<BroadcastMessage>) -> Self {
        Self { messages }
    }
}

impl From<BroadcastMessagesFilterResult> for molecule_gossip::BroadcastMessagesFilterResult {
    fn from(broadcast_messages_filter_result: BroadcastMessagesFilterResult) -> Self {
        molecule_gossip::BroadcastMessagesFilterResult::new_builder()
            .messages(
                molecule_gossip::BroadcastMessages::new_builder()
                    .set(
                        broadcast_messages_filter_result
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

impl TryFrom<molecule_gossip::BroadcastMessagesFilterResult> for BroadcastMessagesFilterResult {
    type Error = Error;

    fn try_from(
        broadcast_messages_filter_result: molecule_gossip::BroadcastMessagesFilterResult,
    ) -> Result<Self, Self::Error> {
        Ok(BroadcastMessagesFilterResult {
            messages: broadcast_messages_filter_result
                .messages()
                .into_iter()
                .map(|message| message.try_into())
                .collect::<Result<Vec<BroadcastMessage>, Error>>()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct GetBroadcastMessages {
    pub id: u64,
    pub chain_hash: Hash256,
    pub after_cursor: Cursor,
    pub count: u16,
}

impl From<GetBroadcastMessages> for molecule_gossip::GetBroadcastMessages {
    fn from(get_broadcast_messages: GetBroadcastMessages) -> Self {
        molecule_gossip::GetBroadcastMessages::new_builder()
            .id(get_broadcast_messages.id.pack())
            .chain_hash(get_broadcast_messages.chain_hash.into())
            .after_cursor(get_broadcast_messages.after_cursor.into())
            .count(get_broadcast_messages.count.into())
            .build()
    }
}

impl TryFrom<molecule_gossip::GetBroadcastMessages> for GetBroadcastMessages {
    type Error = Error;

    fn try_from(
        get_broadcast_messages: molecule_gossip::GetBroadcastMessages,
    ) -> Result<Self, Self::Error> {
        Ok(GetBroadcastMessages {
            id: get_broadcast_messages.id().unpack(),
            chain_hash: get_broadcast_messages.chain_hash().into(),
            after_cursor: get_broadcast_messages.after_cursor().try_into()?,
            count: get_broadcast_messages.count().into(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GetBroadcastMessagesResult {
    pub id: u64,
    pub messages: Vec<BroadcastMessage>,
}

impl From<GetBroadcastMessagesResult> for molecule_gossip::GetBroadcastMessagesResult {
    fn from(get_broadcast_messages_result: GetBroadcastMessagesResult) -> Self {
        molecule_gossip::GetBroadcastMessagesResult::new_builder()
            .id(get_broadcast_messages_result.id.pack())
            .messages(
                molecule_gossip::BroadcastMessages::new_builder()
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

impl TryFrom<molecule_gossip::GetBroadcastMessagesResult> for GetBroadcastMessagesResult {
    type Error = Error;

    fn try_from(
        get_broadcast_messages_result: molecule_gossip::GetBroadcastMessagesResult,
    ) -> Result<Self, Self::Error> {
        Ok(GetBroadcastMessagesResult {
            id: get_broadcast_messages_result.id().unpack(),
            messages: get_broadcast_messages_result
                .messages()
                .into_iter()
                .map(|message| message.try_into())
                .collect::<Result<Vec<BroadcastMessage>, Error>>()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryBroadcastMessages {
    pub id: u64,
    pub chain_hash: Hash256,
    pub queries: Vec<BroadcastMessageQuery>,
}

impl From<QueryBroadcastMessages> for molecule_gossip::QueryBroadcastMessages {
    fn from(query_broadcast_messages: QueryBroadcastMessages) -> Self {
        molecule_gossip::QueryBroadcastMessages::new_builder()
            .id(query_broadcast_messages.id.pack())
            .chain_hash(query_broadcast_messages.chain_hash.into())
            .queries(
                molecule_gossip::BroadcastMessageQueries::new_builder()
                    .set(
                        query_broadcast_messages
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

impl TryFrom<molecule_gossip::QueryBroadcastMessages> for QueryBroadcastMessages {
    type Error = Error;

    fn try_from(
        query_broadcast_messages: molecule_gossip::QueryBroadcastMessages,
    ) -> Result<Self, Self::Error> {
        Ok(QueryBroadcastMessages {
            id: query_broadcast_messages.id().unpack(),
            chain_hash: query_broadcast_messages.chain_hash().into(),
            queries: query_broadcast_messages
                .queries()
                .into_iter()
                .map(|query| query.try_into())
                .collect::<Result<Vec<_>, Error>>()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryBroadcastMessagesResult {
    pub id: u64,
    pub messages: Vec<BroadcastMessage>,
    pub missing_queries: Vec<u16>,
}

impl From<QueryBroadcastMessagesResult> for molecule_gossip::QueryBroadcastMessagesResult {
    fn from(query_broadcast_messages_result: QueryBroadcastMessagesResult) -> Self {
        molecule_gossip::QueryBroadcastMessagesResult::new_builder()
            .id(query_broadcast_messages_result.id.pack())
            .messages(
                molecule_gossip::BroadcastMessages::new_builder()
                    .set(
                        query_broadcast_messages_result
                            .messages
                            .into_iter()
                            .map(|message| message.into())
                            .collect(),
                    )
                    .build(),
            )
            .missing_queries(
                query_broadcast_messages_result
                    .missing_queries
                    .into_iter()
                    .map(|x| x.into())
                    .collect(),
            )
            .build()
    }
}

impl TryFrom<molecule_gossip::QueryBroadcastMessagesResult> for QueryBroadcastMessagesResult {
    type Error = Error;

    fn try_from(
        query_broadcast_messages_result: molecule_gossip::QueryBroadcastMessagesResult,
    ) -> Result<Self, Self::Error> {
        Ok(QueryBroadcastMessagesResult {
            id: query_broadcast_messages_result.id().unpack(),
            messages: query_broadcast_messages_result
                .messages()
                .into_iter()
                .map(|message| message.try_into())
                .collect::<Result<Vec<BroadcastMessage>, Error>>()?,
            missing_queries: query_broadcast_messages_result
                .missing_queries()
                .into_iter()
                .map(|x| u16::from(x))
                .collect(),
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
                FiberChannelMessage::UpdateTlcInfo(update_tlc_info) => {
                    molecule_fiber::FiberMessageUnion::UpdateTlcInfo(update_tlc_info.into())
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
            molecule_fiber::FiberMessageUnion::UpdateTlcInfo(update_tlc_info) => {
                FiberMessage::ChannelNormalOperation(FiberChannelMessage::UpdateTlcInfo(
                    update_tlc_info.into(),
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

pub(crate) fn deterministically_hash<T: Entity>(v: &T) -> [u8; 32] {
    ckb_hash::blake2b_256(v.as_slice()).into()
}

#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaymentHopData {
    pub amount: u128,
    pub expiry: u64,
    // this is only specified in the last hop in the keysend mode
    pub payment_preimage: Option<Hash256>,
    pub hash_algorithm: HashAlgorithm,
    pub funding_tx_hash: Hash256,
    pub next_hop: Option<Pubkey>,
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
    const PACKET_DATA_LEN: usize = 6500;

    fn next_hop(&self) -> Option<Pubkey> {
        self.next_hop.clone()
    }

    fn assoc_data(&self) -> Option<Vec<u8>> {
        None
    }

    fn serialize(&self) -> Vec<u8> {
        molecule_fiber::PaymentHopData::from(self.clone())
            .as_bytes()
            .to_vec()
    }

    fn deserialize(data: &[u8]) -> Option<Self> {
        molecule_fiber::PaymentHopData::from_slice(data)
            .ok()
            .map(|x| x.into())
    }
}

impl From<PaymentHopData> for molecule_fiber::PaymentHopData {
    fn from(payment_hop_data: PaymentHopData) -> Self {
        molecule_fiber::PaymentHopData::new_builder()
            .amount(payment_hop_data.amount.pack())
            .expiry(payment_hop_data.expiry.pack())
            .payment_preimage(
                PaymentPreimageOpt::new_builder()
                    .set(payment_hop_data.payment_preimage.map(|x| x.into()))
                    .build(),
            )
            .hash_algorithm(Byte::new(payment_hop_data.hash_algorithm as u8))
            .funding_tx_hash(payment_hop_data.funding_tx_hash.into())
            .next_hop(
                PubkeyOpt::new_builder()
                    .set(payment_hop_data.next_hop.map(|x| x.into()))
                    .build(),
            )
            .build()
    }
}

impl From<molecule_fiber::PaymentHopData> for PaymentHopData {
    fn from(payment_hop_data: molecule_fiber::PaymentHopData) -> Self {
        PaymentHopData {
            amount: payment_hop_data.amount().unpack(),
            expiry: payment_hop_data.expiry().unpack(),
            payment_preimage: payment_hop_data
                .payment_preimage()
                .to_opt()
                .map(|x| x.into()),
            hash_algorithm: payment_hop_data
                .hash_algorithm()
                .try_into()
                .expect("valid hash algorithm"),
            funding_tx_hash: payment_hop_data.funding_tx_hash().into(),
            next_hop: payment_hop_data
                .next_hop()
                .to_opt()
                .map(|x| x.try_into().expect("invalid pubkey")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OnionPacket<T> {
    _phantom: PhantomData<T>,
    // The encrypted packet
    data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeeledOnionPacket<T> {
    // The decrypted hop data for the current hop
    pub current: T,
    // The shared secret for `current` used for returning error. Set to all zeros for the origin node
    // who has no shared secret.
    pub shared_secret: [u8; 32],
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

    pub fn into_sphinx_onion_packet(self) -> Result<fiber_sphinx::OnionPacket, Error> {
        fiber_sphinx::OnionPacket::from_bytes(self.data)
            .map_err(|err| Error::OnionPacket(err.into()))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
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
        let sphinx_packet = self.into_sphinx_onion_packet()?;
        let shared_secret = sphinx_packet.shared_secret(&privkey.0);

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

        Ok(PeeledOnionPacket {
            current,
            next,
            shared_secret,
        })
    }
}

impl<T: HopData> PeeledOnionPacket<T> {
    /// - `hops_info`: the first is the instruction for the origin node itself.
    ///                Remaining elements are for each node to receive the packet.
    pub fn create<C: Signing>(
        session_key: Privkey,
        mut hops_infos: Vec<T>,
        assoc_data: Option<Vec<u8>>,
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
        let assoc_data = if assoc_data.is_some() {
            assoc_data
        } else {
            current.assoc_data()
        };

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

        Ok(PeeledOnionPacket {
            current,
            next,
            // Use all zeros for the sender
            shared_secret: NO_SHARED_SECRET.clone(),
        })
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
        res.extend(self.shared_secret);
        if let Some(ref next) = self.next {
            res.extend(&next.data[..]);
        }
        res
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, Error> {
        let mut read_bytes = get_hop_data_len(data)
            .ok_or_else(|| Error::OnionPacket(OnionPacketError::InvalidHopData))?;
        let current = unpack_hop_data(data)
            .ok_or_else(|| Error::OnionPacket(OnionPacketError::InvalidHopData))?;

        // Ensure backward compatibility
        let mut shared_secret = NO_SHARED_SECRET.clone();
        if data.len() >= read_bytes + 32 && data.len() != read_bytes + T::PACKET_DATA_LEN {
            shared_secret.copy_from_slice(&data[read_bytes..read_bytes + 32]);
            read_bytes += 32;
        }

        let next = if read_bytes < data.len() {
            Some(OnionPacket::new(data[read_bytes..].to_vec()))
        } else {
            None
        };
        Ok(Self {
            current,
            shared_secret,
            next,
        })
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
