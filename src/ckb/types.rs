use std::str::FromStr;

use super::gen::cfn::{self as molecule_cfn, PubNonce as Byte66};
use super::hash_algorithm::{HashAlgorithm, UnknownHashAlgorithmError};
use super::serde_utils::SliceHex;
use anyhow::anyhow;
use ckb_sdk::{Since, SinceType};
use ckb_types::core::FeeRate;
use ckb_types::packed::Uint64;
use ckb_types::{
    packed::{Byte32 as MByte32, BytesVec, Script, Transaction},
    prelude::{Pack, Unpack},
};
use molecule::prelude::{Builder, Byte, Entity};
use musig2::errors::DecodeError;
use musig2::secp::{Point, Scalar};
use musig2::{BinaryEncoding, PartialSignature, PubNonce};
use once_cell::sync::OnceCell;
use secp256k1::{ecdsa::Signature as Secp256k1Signature, All, PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use thiserror::Error;

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
}

#[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct Signature(pub Secp256k1Signature);

impl From<Signature> for Secp256k1Signature {
    fn from(sig: Signature) -> Self {
        sig.0
    }
}

impl From<Secp256k1Signature> for Signature {
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

impl From<Pubkey> for molecule_cfn::Pubkey {
    fn from(pk: Pubkey) -> molecule_cfn::Pubkey {
        molecule_cfn::Pubkey::new_builder()
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

impl TryFrom<molecule_cfn::Pubkey> for Pubkey {
    type Error = Error;

    fn try_from(pubkey: molecule_cfn::Pubkey) -> Result<Self, Self::Error> {
        let pubkey = pubkey.as_slice();
        PublicKey::from_slice(pubkey)
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<Signature> for molecule_cfn::Signature {
    fn from(signature: Signature) -> molecule_cfn::Signature {
        molecule_cfn::Signature::new_builder()
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

impl TryFrom<molecule_cfn::Signature> for Signature {
    type Error = Error;

    fn try_from(signature: molecule_cfn::Signature) -> Result<Self, Self::Error> {
        let signature = signature.as_slice();
        Secp256k1Signature::from_compact(signature)
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
    pub max_accept_tlcs: u64,
    pub min_tlc_value: u128,
    pub to_local_delay: LockTime,
    pub funding_pubkey: Pubkey,
    pub revocation_basepoint: Pubkey,
    pub payment_basepoint: Pubkey,
    pub delayed_payment_basepoint: Pubkey,
    pub tlc_basepoint: Pubkey,
    pub first_per_commitment_point: Pubkey,
    pub second_per_commitment_point: Pubkey,
    pub next_local_nonce: PubNonce,
    pub channel_flags: u8,
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

impl From<OpenChannel> for molecule_cfn::OpenChannel {
    fn from(open_channel: OpenChannel) -> Self {
        molecule_cfn::OpenChannel::new_builder()
            .chain_hash(open_channel.chain_hash.into())
            .channel_id(open_channel.channel_id.into())
            .funding_udt_type_script(open_channel.funding_udt_type_script.pack())
            .funding_amount(open_channel.funding_amount.pack())
            .reserved_ckb_amount(open_channel.reserved_ckb_amount.pack())
            .funding_fee_rate(open_channel.funding_fee_rate.pack())
            .commitment_fee_rate(open_channel.commitment_fee_rate.pack())
            .max_tlc_value_in_flight(open_channel.max_tlc_value_in_flight.pack())
            .max_accept_tlcs(open_channel.max_accept_tlcs.pack())
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
            .channel_flags(open_channel.channel_flags.into())
            .build()
    }
}

impl TryFrom<molecule_cfn::OpenChannel> for OpenChannel {
    type Error = Error;

    fn try_from(open_channel: molecule_cfn::OpenChannel) -> Result<Self, Self::Error> {
        Ok(OpenChannel {
            chain_hash: open_channel.chain_hash().into(),
            channel_id: open_channel.channel_id().into(),
            funding_udt_type_script: open_channel.funding_udt_type_script().to_opt(),
            funding_amount: open_channel.funding_amount().unpack(),
            reserved_ckb_amount: open_channel.reserved_ckb_amount().unpack(),
            funding_fee_rate: open_channel.funding_fee_rate().unpack(),
            commitment_fee_rate: open_channel.commitment_fee_rate().unpack(),
            max_tlc_value_in_flight: open_channel.max_tlc_value_in_flight().unpack(),
            max_accept_tlcs: open_channel.max_accept_tlcs().unpack(),
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
            channel_flags: open_channel.channel_flags().into(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct AcceptChannel {
    pub channel_id: Hash256,
    pub funding_amount: u128,
    pub reserved_ckb_amount: u64,
    pub max_tlc_value_in_flight: u128,
    pub max_accept_tlcs: u64,
    pub min_tlc_value: u128,
    pub to_local_delay: LockTime,
    pub funding_pubkey: Pubkey,
    pub revocation_basepoint: Pubkey,
    pub payment_basepoint: Pubkey,
    pub delayed_payment_basepoint: Pubkey,
    pub tlc_basepoint: Pubkey,
    pub first_per_commitment_point: Pubkey,
    pub second_per_commitment_point: Pubkey,
    pub next_local_nonce: PubNonce,
}

impl From<AcceptChannel> for molecule_cfn::AcceptChannel {
    fn from(accept_channel: AcceptChannel) -> Self {
        molecule_cfn::AcceptChannel::new_builder()
            .channel_id(accept_channel.channel_id.into())
            .funding_amount(accept_channel.funding_amount.pack())
            .reserved_ckb_amount(accept_channel.reserved_ckb_amount.pack())
            .max_tlc_value_in_flight(accept_channel.max_tlc_value_in_flight.pack())
            .max_accept_tlcs(accept_channel.max_accept_tlcs.pack())
            .min_tlc_value(accept_channel.min_tlc_value.pack())
            .to_self_delay(accept_channel.to_local_delay.into())
            .funding_pubkey(accept_channel.funding_pubkey.into())
            .revocation_basepoint(accept_channel.revocation_basepoint.into())
            .payment_basepoint(accept_channel.payment_basepoint.into())
            .delayed_payment_basepoint(accept_channel.delayed_payment_basepoint.into())
            .tlc_basepoint(accept_channel.tlc_basepoint.into())
            .first_per_commitment_point(accept_channel.first_per_commitment_point.into())
            .second_per_commitment_point(accept_channel.second_per_commitment_point.into())
            .next_local_nonce((&accept_channel.next_local_nonce).into())
            .build()
    }
}

impl TryFrom<molecule_cfn::AcceptChannel> for AcceptChannel {
    type Error = Error;

    fn try_from(accept_channel: molecule_cfn::AcceptChannel) -> Result<Self, Self::Error> {
        Ok(AcceptChannel {
            channel_id: accept_channel.channel_id().into(),
            funding_amount: accept_channel.funding_amount().unpack(),
            reserved_ckb_amount: accept_channel.reserved_ckb_amount().unpack(),
            max_tlc_value_in_flight: accept_channel.max_tlc_value_in_flight().unpack(),
            max_accept_tlcs: accept_channel.max_accept_tlcs().unpack(),
            min_tlc_value: accept_channel.min_tlc_value().unpack(),
            to_local_delay: accept_channel.to_self_delay().try_into()?,
            funding_pubkey: accept_channel.funding_pubkey().try_into()?,
            revocation_basepoint: accept_channel.revocation_basepoint().try_into()?,
            payment_basepoint: accept_channel.payment_basepoint().try_into()?,
            delayed_payment_basepoint: accept_channel.delayed_payment_basepoint().try_into()?,
            tlc_basepoint: accept_channel.tlc_basepoint().try_into()?,
            first_per_commitment_point: accept_channel.first_per_commitment_point().try_into()?,
            second_per_commitment_point: accept_channel.second_per_commitment_point().try_into()?,
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

impl From<CommitmentSigned> for molecule_cfn::CommitmentSigned {
    fn from(commitment_signed: CommitmentSigned) -> Self {
        molecule_cfn::CommitmentSigned::new_builder()
            .channel_id(commitment_signed.channel_id.into())
            .partial_signature(partial_signature_to_molecule(
                commitment_signed.partial_signature,
            ))
            .next_local_nonce((&commitment_signed.next_local_nonce).into())
            .build()
    }
}

impl TryFrom<molecule_cfn::CommitmentSigned> for CommitmentSigned {
    type Error = Error;

    fn try_from(commitment_signed: molecule_cfn::CommitmentSigned) -> Result<Self, Self::Error> {
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

impl From<TxSignatures> for molecule_cfn::TxSignatures {
    fn from(tx_signatures: TxSignatures) -> Self {
        molecule_cfn::TxSignatures::new_builder()
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

impl TryFrom<molecule_cfn::TxSignatures> for TxSignatures {
    type Error = Error;

    fn try_from(tx_signatures: molecule_cfn::TxSignatures) -> Result<Self, Self::Error> {
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

impl From<ChannelReady> for molecule_cfn::ChannelReady {
    fn from(channel_ready: ChannelReady) -> Self {
        molecule_cfn::ChannelReady::new_builder()
            .channel_id(channel_ready.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_cfn::ChannelReady> for ChannelReady {
    type Error = Error;

    fn try_from(channel_ready: molecule_cfn::ChannelReady) -> Result<Self, Self::Error> {
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

impl From<TxUpdate> for molecule_cfn::TxUpdate {
    fn from(tx_update: TxUpdate) -> Self {
        molecule_cfn::TxUpdate::new_builder()
            .channel_id(tx_update.channel_id.into())
            .tx(tx_update.tx)
            .build()
    }
}

impl TryFrom<molecule_cfn::TxUpdate> for TxUpdate {
    type Error = Error;

    fn try_from(tx_update: molecule_cfn::TxUpdate) -> Result<Self, Self::Error> {
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

impl From<TxComplete> for molecule_cfn::TxComplete {
    fn from(tx_complete: TxComplete) -> Self {
        molecule_cfn::TxComplete::new_builder()
            .channel_id(tx_complete.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_cfn::TxComplete> for TxComplete {
    type Error = Error;

    fn try_from(tx_complete: molecule_cfn::TxComplete) -> Result<Self, Self::Error> {
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

impl From<TxAbort> for molecule_cfn::TxAbort {
    fn from(tx_abort: TxAbort) -> Self {
        molecule_cfn::TxAbort::new_builder()
            .channel_id(tx_abort.channel_id.into())
            .message(tx_abort.message.pack())
            .build()
    }
}

impl TryFrom<molecule_cfn::TxAbort> for TxAbort {
    type Error = Error;

    fn try_from(tx_abort: molecule_cfn::TxAbort) -> Result<Self, Self::Error> {
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

impl From<TxInitRBF> for molecule_cfn::TxInitRBF {
    fn from(tx_init_rbf: TxInitRBF) -> Self {
        molecule_cfn::TxInitRBF::new_builder()
            .channel_id(tx_init_rbf.channel_id.into())
            .fee_rate(tx_init_rbf.fee_rate.pack())
            .build()
    }
}

impl TryFrom<molecule_cfn::TxInitRBF> for TxInitRBF {
    type Error = Error;

    fn try_from(tx_init_rbf: molecule_cfn::TxInitRBF) -> Result<Self, Self::Error> {
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

impl From<TxAckRBF> for molecule_cfn::TxAckRBF {
    fn from(tx_ack_rbf: TxAckRBF) -> Self {
        molecule_cfn::TxAckRBF::new_builder()
            .channel_id(tx_ack_rbf.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_cfn::TxAckRBF> for TxAckRBF {
    type Error = Error;

    fn try_from(tx_ack_rbf: molecule_cfn::TxAckRBF) -> Result<Self, Self::Error> {
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

impl From<Shutdown> for molecule_cfn::Shutdown {
    fn from(shutdown: Shutdown) -> Self {
        molecule_cfn::Shutdown::new_builder()
            .channel_id(shutdown.channel_id.into())
            .close_script(shutdown.close_script)
            .fee_rate(shutdown.fee_rate.as_u64().pack())
            .force(if shutdown.force { 1_u8 } else { 0_u8 }.into())
            .build()
    }
}

impl TryFrom<molecule_cfn::Shutdown> for Shutdown {
    type Error = Error;

    fn try_from(shutdown: molecule_cfn::Shutdown) -> Result<Self, Self::Error> {
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

impl From<ClosingSigned> for molecule_cfn::ClosingSigned {
    fn from(closing_signed: ClosingSigned) -> Self {
        molecule_cfn::ClosingSigned::new_builder()
            .channel_id(closing_signed.channel_id.into())
            .partial_signature(partial_signature_to_molecule(
                closing_signed.partial_signature,
            ))
            .build()
    }
}

impl TryFrom<molecule_cfn::ClosingSigned> for ClosingSigned {
    type Error = Error;

    fn try_from(closing_signed: molecule_cfn::ClosingSigned) -> Result<Self, Self::Error> {
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

impl From<AddTlc> for molecule_cfn::AddTlc {
    fn from(add_tlc: AddTlc) -> Self {
        molecule_cfn::AddTlc::new_builder()
            .channel_id(add_tlc.channel_id.into())
            .tlc_id(add_tlc.tlc_id.pack())
            .amount(add_tlc.amount.pack())
            .payment_hash(add_tlc.payment_hash.into())
            .expiry(add_tlc.expiry.into())
            .hash_algorithm(Byte::new(add_tlc.hash_algorithm as u8))
            .build()
    }
}

impl TryFrom<molecule_cfn::AddTlc> for AddTlc {
    type Error = Error;

    fn try_from(add_tlc: molecule_cfn::AddTlc) -> Result<Self, Self::Error> {
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

impl From<RevokeAndAck> for molecule_cfn::RevokeAndAck {
    fn from(revoke_and_ack: RevokeAndAck) -> Self {
        molecule_cfn::RevokeAndAck::new_builder()
            .channel_id(revoke_and_ack.channel_id.into())
            .per_commitment_secret(revoke_and_ack.per_commitment_secret.into())
            .next_per_commitment_point(revoke_and_ack.next_per_commitment_point.into())
            .build()
    }
}

impl TryFrom<molecule_cfn::RevokeAndAck> for RevokeAndAck {
    type Error = Error;

    fn try_from(revoke_and_ack: molecule_cfn::RevokeAndAck) -> Result<Self, Self::Error> {
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

impl From<RemoveTlcFulfill> for molecule_cfn::RemoveTlcFulfill {
    fn from(remove_tlc_fulfill: RemoveTlcFulfill) -> Self {
        molecule_cfn::RemoveTlcFulfill::new_builder()
            .payment_preimage(remove_tlc_fulfill.payment_preimage.into())
            .build()
    }
}

impl TryFrom<molecule_cfn::RemoveTlcFulfill> for RemoveTlcFulfill {
    type Error = Error;

    fn try_from(remove_tlc_fulfill: molecule_cfn::RemoveTlcFulfill) -> Result<Self, Self::Error> {
        Ok(RemoveTlcFulfill {
            payment_preimage: remove_tlc_fulfill.payment_preimage().into(),
        })
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveTlcFail {
    pub error_code: u32,
}

impl From<RemoveTlcFail> for molecule_cfn::RemoveTlcFail {
    fn from(remove_tlc_fail: RemoveTlcFail) -> Self {
        molecule_cfn::RemoveTlcFail::new_builder()
            .error_code(remove_tlc_fail.error_code.pack())
            .build()
    }
}

impl TryFrom<molecule_cfn::RemoveTlcFail> for RemoveTlcFail {
    type Error = Error;

    fn try_from(remove_tlc_fail: molecule_cfn::RemoveTlcFail) -> Result<Self, Self::Error> {
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

impl From<RemoveTlcReason> for molecule_cfn::RemoveTlcReasonUnion {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        match remove_tlc_reason {
            RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill) => {
                molecule_cfn::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill.into())
            }
            RemoveTlcReason::RemoveTlcFail(remove_tlc_fail) => {
                molecule_cfn::RemoveTlcReasonUnion::RemoveTlcFail(remove_tlc_fail.into())
            }
        }
    }
}

impl From<RemoveTlcReason> for molecule_cfn::RemoveTlcReason {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        molecule_cfn::RemoveTlcReason::new_builder()
            .set(remove_tlc_reason)
            .build()
    }
}

impl TryFrom<molecule_cfn::RemoveTlcReason> for RemoveTlcReason {
    type Error = Error;

    fn try_from(remove_tlc_reason: molecule_cfn::RemoveTlcReason) -> Result<Self, Self::Error> {
        match remove_tlc_reason.to_enum() {
            molecule_cfn::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill) => Ok(
                RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill.try_into()?),
            ),
            molecule_cfn::RemoveTlcReasonUnion::RemoveTlcFail(remove_tlc_fail) => {
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

impl From<RemoveTlc> for molecule_cfn::RemoveTlc {
    fn from(remove_tlc: RemoveTlc) -> Self {
        molecule_cfn::RemoveTlc::new_builder()
            .channel_id(remove_tlc.channel_id.into())
            .tlc_id(remove_tlc.tlc_id.pack())
            .reason(
                molecule_cfn::RemoveTlcReason::new_builder()
                    .set(remove_tlc.reason)
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_cfn::RemoveTlc> for RemoveTlc {
    type Error = Error;

    fn try_from(remove_tlc: molecule_cfn::RemoveTlc) -> Result<Self, Self::Error> {
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

impl From<ReestablishChannel> for molecule_cfn::ReestablishChannel {
    fn from(reestablish_channel: ReestablishChannel) -> Self {
        molecule_cfn::ReestablishChannel::new_builder()
            .channel_id(reestablish_channel.channel_id.into())
            .local_commitment_number(reestablish_channel.local_commitment_number.pack())
            .remote_commitment_number(reestablish_channel.remote_commitment_number.pack())
            .build()
    }
}

impl TryFrom<molecule_cfn::ReestablishChannel> for ReestablishChannel {
    type Error = Error;

    fn try_from(
        reestablish_channel: molecule_cfn::ReestablishChannel,
    ) -> Result<Self, Self::Error> {
        Ok(ReestablishChannel {
            channel_id: reestablish_channel.channel_id().into(),
            local_commitment_number: reestablish_channel.local_commitment_number().unpack(),
            remote_commitment_number: reestablish_channel.remote_commitment_number().unpack(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum CFNMessage {
    OpenChannel(OpenChannel),
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
}

impl CFNMessage {
    pub fn get_channel_id(&self) -> Hash256 {
        match &self {
            CFNMessage::OpenChannel(open_channel) => open_channel.channel_id,
            CFNMessage::AcceptChannel(accept_channel) => accept_channel.channel_id,
            CFNMessage::CommitmentSigned(commitment_signed) => commitment_signed.channel_id,
            CFNMessage::TxSignatures(tx_signatures) => tx_signatures.channel_id,
            CFNMessage::ChannelReady(channel_ready) => channel_ready.channel_id,
            CFNMessage::TxUpdate(tx_update) => tx_update.channel_id,
            CFNMessage::TxComplete(tx_complete) => tx_complete.channel_id,
            CFNMessage::TxAbort(tx_abort) => tx_abort.channel_id,
            CFNMessage::TxInitRBF(tx_init_rbf) => tx_init_rbf.channel_id,
            CFNMessage::TxAckRBF(tx_ack_rbf) => tx_ack_rbf.channel_id,
            CFNMessage::Shutdown(shutdown) => shutdown.channel_id,
            CFNMessage::ClosingSigned(closing_signed) => closing_signed.channel_id,
            CFNMessage::AddTlc(add_tlc) => add_tlc.channel_id,
            CFNMessage::RevokeAndAck(revoke_and_ack) => revoke_and_ack.channel_id,
            CFNMessage::RemoveTlc(remove_tlc) => remove_tlc.channel_id,
            CFNMessage::ReestablishChannel(reestablish_channel) => reestablish_channel.channel_id,
        }
    }
}

impl From<CFNMessage> for molecule_cfn::CFNMessageUnion {
    fn from(cfn_message: CFNMessage) -> Self {
        match cfn_message {
            CFNMessage::OpenChannel(open_channel) => {
                molecule_cfn::CFNMessageUnion::OpenChannel(open_channel.into())
            }
            CFNMessage::AcceptChannel(accept_channel) => {
                molecule_cfn::CFNMessageUnion::AcceptChannel(accept_channel.into())
            }
            CFNMessage::CommitmentSigned(commitment_signed) => {
                molecule_cfn::CFNMessageUnion::CommitmentSigned(commitment_signed.into())
            }
            CFNMessage::TxSignatures(tx_signatures) => {
                molecule_cfn::CFNMessageUnion::TxSignatures(tx_signatures.into())
            }
            CFNMessage::ChannelReady(channel_ready) => {
                molecule_cfn::CFNMessageUnion::ChannelReady(channel_ready.into())
            }
            CFNMessage::TxUpdate(tx_update) => {
                molecule_cfn::CFNMessageUnion::TxUpdate(tx_update.into())
            }
            CFNMessage::TxComplete(tx_complete) => {
                molecule_cfn::CFNMessageUnion::TxComplete(tx_complete.into())
            }
            CFNMessage::TxAbort(tx_abort) => {
                molecule_cfn::CFNMessageUnion::TxAbort(tx_abort.into())
            }
            CFNMessage::TxInitRBF(tx_init_rbf) => {
                molecule_cfn::CFNMessageUnion::TxInitRBF(tx_init_rbf.into())
            }
            CFNMessage::TxAckRBF(tx_ack_rbf) => {
                molecule_cfn::CFNMessageUnion::TxAckRBF(tx_ack_rbf.into())
            }
            CFNMessage::Shutdown(shutdown) => {
                molecule_cfn::CFNMessageUnion::Shutdown(shutdown.into())
            }
            CFNMessage::ClosingSigned(closing_signed) => {
                molecule_cfn::CFNMessageUnion::ClosingSigned(closing_signed.into())
            }
            CFNMessage::AddTlc(add_tlc) => molecule_cfn::CFNMessageUnion::AddTlc(add_tlc.into()),
            CFNMessage::RemoveTlc(remove_tlc) => {
                molecule_cfn::CFNMessageUnion::RemoveTlc(remove_tlc.into())
            }
            CFNMessage::RevokeAndAck(revoke_and_ack) => {
                molecule_cfn::CFNMessageUnion::RevokeAndAck(revoke_and_ack.into())
            }
            CFNMessage::ReestablishChannel(reestablish_channel) => {
                molecule_cfn::CFNMessageUnion::ReestablishChannel(reestablish_channel.into())
            }
        }
    }
}

impl From<CFNMessage> for molecule_cfn::CFNMessage {
    fn from(cfn_message: CFNMessage) -> Self {
        molecule_cfn::CFNMessage::new_builder()
            .set(cfn_message)
            .build()
    }
}

impl TryFrom<molecule_cfn::CFNMessage> for CFNMessage {
    type Error = Error;

    fn try_from(cfn_message: molecule_cfn::CFNMessage) -> Result<Self, Self::Error> {
        Ok(match cfn_message.to_enum() {
            molecule_cfn::CFNMessageUnion::OpenChannel(open_channel) => {
                CFNMessage::OpenChannel(open_channel.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::AcceptChannel(accept_channel) => {
                CFNMessage::AcceptChannel(accept_channel.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::CommitmentSigned(commitment_signed) => {
                CFNMessage::CommitmentSigned(commitment_signed.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::TxSignatures(tx_signatures) => {
                CFNMessage::TxSignatures(tx_signatures.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::ChannelReady(channel_ready) => {
                CFNMessage::ChannelReady(channel_ready.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::TxUpdate(tx_update) => {
                CFNMessage::TxUpdate(tx_update.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::TxComplete(tx_complete) => {
                CFNMessage::TxComplete(tx_complete.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::TxAbort(tx_abort) => {
                CFNMessage::TxAbort(tx_abort.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::TxInitRBF(tx_init_rbf) => {
                CFNMessage::TxInitRBF(tx_init_rbf.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::TxAckRBF(tx_ack_rbf) => {
                CFNMessage::TxAckRBF(tx_ack_rbf.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::Shutdown(shutdown) => {
                CFNMessage::Shutdown(shutdown.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::ClosingSigned(closing_signed) => {
                CFNMessage::ClosingSigned(closing_signed.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::AddTlc(add_tlc) => {
                CFNMessage::AddTlc(add_tlc.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::RemoveTlc(remove_tlc) => {
                CFNMessage::RemoveTlc(remove_tlc.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::RevokeAndAck(revoke_and_ack) => {
                CFNMessage::RevokeAndAck(revoke_and_ack.try_into()?)
            }
            molecule_cfn::CFNMessageUnion::ReestablishChannel(reestablish_channel) => {
                CFNMessage::ReestablishChannel(reestablish_channel.try_into()?)
            }
        })
    }
}

macro_rules! impl_traits {
    ($t:ident) => {
        impl $t {
            pub fn to_molecule_bytes(self) -> molecule::bytes::Bytes {
                molecule_cfn::$t::from(self).as_bytes()
            }
        }

        impl $t {
            pub fn from_molecule_slice(data: &[u8]) -> Result<Self, Error> {
                molecule_cfn::$t::from_slice(data)
                    .map_err(Into::into)
                    .and_then(TryInto::try_into)
            }
        }
    };
}

impl_traits!(CFNMessage);

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
        let add_tlc_mol: super::molecule_cfn::AddTlc = add_tlc.clone().into();
        let add_tlc2 = add_tlc_mol.try_into().expect("decode");
        assert_eq!(add_tlc, add_tlc2);
    }
}
