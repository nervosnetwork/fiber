pub use ckb_crypto::secp::{Pubkey as CkbPubkey, Signature as CkbSignature};
use ckb_types::{
    packed::{Byte32, BytesVec, Script, Transaction},
    prelude::{Pack, Unpack},
};
use molecule::prelude::{Builder, Byte, Entity};
use serde::{Deserialize, Serialize};
use serde_utils::{EntityWrapperBase64, EntityWrapperHex};
use serde_with::base64::Base64;
use serde_with::serde_as;
use thiserror::Error;

use super::gen::pcn::{self as molecule_pcn, SignatureVec};

#[derive(Debug, Clone)]
pub struct Pubkey(pub CkbPubkey);

impl From<Pubkey> for CkbPubkey {
    fn from(pk: Pubkey) -> CkbPubkey {
        pk.0
    }
}

impl From<CkbPubkey> for Pubkey {
    fn from(pk: CkbPubkey) -> Pubkey {
        Pubkey(pk)
    }
}

impl Serialize for Pubkey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        base64::encode(&self.0.serialize()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Pubkey {
    fn deserialize<D>(deserializer: D) -> Result<Pubkey, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = base64::decode(s).map_err(serde::de::Error::custom)?;
        CkbPubkey::from_slice(&bytes)
            .map(Into::into)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone)]
pub struct Signature(pub CkbSignature);

impl From<Signature> for CkbSignature {
    fn from(sig: Signature) -> CkbSignature {
        sig.0
    }
}

impl From<CkbSignature> for Signature {
    fn from(sig: CkbSignature) -> Signature {
        Signature(sig)
    }
}

impl Serialize for Signature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        base64::encode(&self.0.serialize()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D>(deserializer: D) -> Result<Signature, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = base64::decode(s).map_err(serde::de::Error::custom)?;
        CkbSignature::from_slice(&bytes)
            .map(Into::into)
            .map_err(serde::de::Error::custom)
    }
}

/// The error type wrap various ser/de errors.
#[derive(Error, Debug)]
pub enum Error {
    /// Invalid pubkey/signature format
    #[error("Secp error: {0}")]
    Secp(#[from] ckb_crypto::secp::Error),
    #[error("Molecule error: {0}")]
    Molecule(#[from] molecule::error::VerificationError),
}

impl From<Pubkey> for molecule_pcn::Pubkey {
    fn from(pk: Pubkey) -> molecule_pcn::Pubkey {
        molecule_pcn::Pubkey::new_builder()
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

impl TryFrom<molecule_pcn::Pubkey> for Pubkey {
    type Error = Error;

    fn try_from(pubkey: molecule_pcn::Pubkey) -> Result<Self, Self::Error> {
        let pubkey = pubkey.as_slice();
        CkbPubkey::from_slice(pubkey)
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<Signature> for molecule_pcn::Signature {
    fn from(signature: Signature) -> molecule_pcn::Signature {
        molecule_pcn::Signature::new_builder()
            .set(
                signature
                    .0
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

impl TryFrom<molecule_pcn::Signature> for Signature {
    type Error = Error;

    fn try_from(signature: molecule_pcn::Signature) -> Result<Self, Self::Error> {
        let signature = signature.as_slice();
        CkbSignature::from_slice(signature)
            .map(Into::into)
            .map_err(Into::into)
    }
}

pub mod serde_utils {
    use molecule::prelude::Entity;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};

    pub fn from_base64<'de, D, E>(deserializer: D) -> Result<E, D::Error>
    where
        D: Deserializer<'de>,
        E: Entity,
    {
        use serde::de::Error;

        String::deserialize(deserializer)
            .and_then(|string| {
                base64::decode(&string)
                    .map_err(|err| Error::custom(format!("failed to decode base64: {:?}", err)))
            })
            .and_then(|vec| {
                E::from_slice(&vec).map_err(|err| {
                    serde::de::Error::custom(format!(
                        "failed to deserialize molecule entity: {:?}",
                        err
                    ))
                })
            })
    }

    pub fn to_base64<E, S>(e: &E, serializer: S) -> Result<S::Ok, S::Error>
    where
        E: Entity,
        S: Serializer,
    {
        serializer.serialize_str(&base64::encode(e.as_slice()))
    }

    pub fn from_hex<'de, D, E>(deserializer: D) -> Result<E, D::Error>
    where
        D: Deserializer<'de>,
        E: Entity,
    {
        use serde::de::Error;

        String::deserialize(deserializer)
            .and_then(|string| {
                if &string[..2].to_lowercase() != "0x" {
                    return Err(Error::custom("hex string should start with 0x"));
                };
                hex::decode(&string[2..])
                    .map_err(|err| Error::custom(format!("failed to decode hex: {:?}", err)))
            })
            .and_then(|vec| {
                E::from_slice(&vec).map_err(|err| {
                    serde::de::Error::custom(format!(
                        "failed to deserialize molecule entity: {:?}",
                        err
                    ))
                })
            })
    }

    pub fn to_hex<E, S>(e: &E, serializer: S) -> Result<S::Ok, S::Error>
    where
        E: Entity,
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", &hex::encode(e.as_slice())))
    }

    pub struct EntityWrapperBase64<E: Entity>(pub E);

    impl<E> SerializeAs<E> for EntityWrapperBase64<E>
    where
        E: Entity,
    {
        fn serialize_as<S>(e: &E, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            to_base64(e, serializer)
        }
    }

    impl<'de, E> DeserializeAs<'de, E> for EntityWrapperBase64<E>
    where
        E: Entity,
    {
        fn deserialize_as<D>(deserializer: D) -> Result<E, D::Error>
        where
            D: Deserializer<'de>,
        {
            from_base64(deserializer)
        }
    }

    pub struct EntityWrapperHex<E: Entity>(pub E);

    impl<E> SerializeAs<E> for EntityWrapperHex<E>
    where
        E: Entity,
    {
        fn serialize_as<S>(e: &E, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            to_hex(e, serializer)
        }
    }

    impl<'de, E> DeserializeAs<'de, E> for EntityWrapperHex<E>
    where
        E: Entity,
    {
        fn deserialize_as<D>(deserializer: D) -> Result<E, D::Error>
        where
            D: Deserializer<'de>,
        {
            from_hex(deserializer)
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenChannel {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub chain_hash: Byte32,
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    #[serde_as(as = "Option<EntityWrapperBase64<Script>>")]
    pub funding_type_script: Option<Script>,
    pub funding_amount: u64,
    pub funding_fee_rate: u64,
    pub commitment_fee_rate: u64,
    pub max_tlc_value_in_flight: u64,
    pub max_accept_tlcs: u64,
    pub min_tlc_value: u64,
    pub to_self_delay: u64,
    pub funding_pubkey: Pubkey,
    pub revocation_basepoint: Pubkey,
    pub payment_basepoint: Pubkey,
    pub delayed_payment_basepoint: Pubkey,
    pub tlc_basepoint: Pubkey,
    pub first_per_commitment_point: Pubkey,
    pub second_per_commitment_point: Pubkey,
    pub channel_flags: u8,
}

impl From<OpenChannel> for molecule_pcn::OpenChannel {
    fn from(open_channel: OpenChannel) -> Self {
        molecule_pcn::OpenChannel::new_builder()
            .chain_hash(open_channel.chain_hash.into())
            .channel_id(open_channel.channel_id.into())
            .funding_type_script(open_channel.funding_type_script.pack())
            .funding_amount(open_channel.funding_amount.pack())
            .funding_fee_rate(open_channel.funding_fee_rate.pack())
            .commitment_fee_rate(open_channel.commitment_fee_rate.pack())
            .max_tlc_value_in_flight(open_channel.max_tlc_value_in_flight.pack())
            .max_accept_tlcs(open_channel.max_accept_tlcs.pack())
            .min_tlc_value(open_channel.min_tlc_value.pack())
            .to_self_delay(open_channel.to_self_delay.pack())
            .funding_pubkey(open_channel.funding_pubkey.into())
            .revocation_basepoint(open_channel.revocation_basepoint.into())
            .payment_basepoint(open_channel.payment_basepoint.into())
            .delayed_payment_basepoint(open_channel.delayed_payment_basepoint.into())
            .tlc_basepoint(open_channel.tlc_basepoint.into())
            .first_per_commitment_point(open_channel.first_per_commitment_point.into())
            .second_per_commitment_point(open_channel.second_per_commitment_point.into())
            .channel_flags(open_channel.channel_flags.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::OpenChannel> for OpenChannel {
    type Error = Error;

    fn try_from(open_channel: molecule_pcn::OpenChannel) -> Result<Self, Self::Error> {
        Ok(OpenChannel {
            chain_hash: open_channel.chain_hash(),
            channel_id: open_channel.channel_id(),
            funding_type_script: open_channel.funding_type_script().to_opt(),
            funding_amount: open_channel.funding_amount().unpack(),
            funding_fee_rate: open_channel.funding_fee_rate().unpack(),
            commitment_fee_rate: open_channel.commitment_fee_rate().unpack(),
            max_tlc_value_in_flight: open_channel.max_tlc_value_in_flight().unpack(),
            max_accept_tlcs: open_channel.max_accept_tlcs().unpack(),
            min_tlc_value: open_channel.min_tlc_value().unpack(),
            to_self_delay: open_channel.to_self_delay().unpack(),
            funding_pubkey: open_channel.funding_pubkey().try_into()?,
            revocation_basepoint: open_channel.revocation_basepoint().try_into()?,
            payment_basepoint: open_channel.payment_basepoint().try_into()?,
            delayed_payment_basepoint: open_channel.delayed_payment_basepoint().try_into()?,
            tlc_basepoint: open_channel.tlc_basepoint().try_into()?,
            first_per_commitment_point: open_channel.first_per_commitment_point().try_into()?,
            second_per_commitment_point: open_channel.second_per_commitment_point().try_into()?,
            channel_flags: open_channel.channel_flags().into(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptChannel {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub funding_amount: u64,
    pub max_tlc_value_in_flight: u64,
    pub max_accept_tlcs: u64,
    pub min_tlc_value: u64,
    pub to_self_delay: u64,
    pub funding_pubkey: Pubkey,
    pub revocation_basepoint: Pubkey,
    pub payment_basepoint: Pubkey,
    pub delayed_payment_basepoint: Pubkey,
    pub tlc_basepoint: Pubkey,
    pub first_per_commitment_point: Pubkey,
    pub second_per_commitment_point: Pubkey,
}

impl From<AcceptChannel> for molecule_pcn::AcceptChannel {
    fn from(accept_channel: AcceptChannel) -> Self {
        molecule_pcn::AcceptChannel::new_builder()
            .channel_id(accept_channel.channel_id.into())
            .funding_amount(accept_channel.funding_amount.pack())
            .max_tlc_value_in_flight(accept_channel.max_tlc_value_in_flight.pack())
            .max_accept_tlcs(accept_channel.max_accept_tlcs.pack())
            .min_tlc_value(accept_channel.min_tlc_value.pack())
            .to_self_delay(accept_channel.to_self_delay.pack())
            .funding_pubkey(accept_channel.funding_pubkey.into())
            .revocation_basepoint(accept_channel.revocation_basepoint.into())
            .payment_basepoint(accept_channel.payment_basepoint.into())
            .delayed_payment_basepoint(accept_channel.delayed_payment_basepoint.into())
            .tlc_basepoint(accept_channel.tlc_basepoint.into())
            .first_per_commitment_point(accept_channel.first_per_commitment_point.into())
            .second_per_commitment_point(accept_channel.second_per_commitment_point.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::AcceptChannel> for AcceptChannel {
    type Error = Error;

    fn try_from(accept_channel: molecule_pcn::AcceptChannel) -> Result<Self, Self::Error> {
        Ok(AcceptChannel {
            channel_id: accept_channel.channel_id(),
            funding_amount: accept_channel.funding_amount().unpack(),
            max_tlc_value_in_flight: accept_channel.max_tlc_value_in_flight().unpack(),
            max_accept_tlcs: accept_channel.max_accept_tlcs().unpack(),
            min_tlc_value: accept_channel.min_tlc_value().unpack(),
            to_self_delay: accept_channel.to_self_delay().unpack(),
            funding_pubkey: accept_channel.funding_pubkey().try_into()?,
            revocation_basepoint: accept_channel.revocation_basepoint().try_into()?,
            payment_basepoint: accept_channel.payment_basepoint().try_into()?,
            delayed_payment_basepoint: accept_channel.delayed_payment_basepoint().try_into()?,
            tlc_basepoint: accept_channel.tlc_basepoint().try_into()?,
            first_per_commitment_point: accept_channel.first_per_commitment_point().try_into()?,
            second_per_commitment_point: accept_channel.second_per_commitment_point().try_into()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitmentSigned {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub signature: Signature,
}

impl From<CommitmentSigned> for molecule_pcn::CommitmentSigned {
    fn from(commitment_signed: CommitmentSigned) -> Self {
        molecule_pcn::CommitmentSigned::new_builder()
            .channel_id(commitment_signed.channel_id.into())
            .signature(commitment_signed.signature.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::CommitmentSigned> for CommitmentSigned {
    type Error = Error;

    fn try_from(commitment_signed: molecule_pcn::CommitmentSigned) -> Result<Self, Self::Error> {
        Ok(CommitmentSigned {
            channel_id: commitment_signed.channel_id(),
            signature: commitment_signed.signature().try_into()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxSignatures {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub tx_hash: Byte32,
    pub witnesses: Vec<Vec<u8>>,
}

impl From<TxSignatures> for molecule_pcn::TxSignatures {
    fn from(tx_signatures: TxSignatures) -> Self {
        molecule_pcn::TxSignatures::new_builder()
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

impl TryFrom<molecule_pcn::TxSignatures> for TxSignatures {
    type Error = Error;

    fn try_from(tx_signatures: molecule_pcn::TxSignatures) -> Result<Self, Self::Error> {
        Ok(TxSignatures {
            channel_id: tx_signatures.channel_id(),
            tx_hash: tx_signatures.tx_hash(),
            witnesses: tx_signatures
                .witnesses()
                .into_iter()
                .map(|witness| witness.unpack())
                .collect(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelReady {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
}

impl From<ChannelReady> for molecule_pcn::ChannelReady {
    fn from(channel_ready: ChannelReady) -> Self {
        molecule_pcn::ChannelReady::new_builder()
            .channel_id(channel_ready.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::ChannelReady> for ChannelReady {
    type Error = Error;

    fn try_from(channel_ready: molecule_pcn::ChannelReady) -> Result<Self, Self::Error> {
        Ok(ChannelReady {
            channel_id: channel_ready.channel_id(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAdd {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    #[serde_as(as = "EntityWrapperBase64<Transaction>")]
    pub tx: Transaction,
}

impl From<TxAdd> for molecule_pcn::TxAdd {
    fn from(tx_add: TxAdd) -> Self {
        molecule_pcn::TxAdd::new_builder()
            .channel_id(tx_add.channel_id.into())
            .tx(tx_add.tx)
            .build()
    }
}

impl TryFrom<molecule_pcn::TxAdd> for TxAdd {
    type Error = Error;

    fn try_from(tx_add: molecule_pcn::TxAdd) -> Result<Self, Self::Error> {
        Ok(TxAdd {
            channel_id: tx_add.channel_id(),
            tx: tx_add.tx(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRemove {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    #[serde_as(as = "EntityWrapperBase64<Transaction>")]
    pub tx: Transaction,
}

impl From<TxRemove> for molecule_pcn::TxRemove {
    fn from(tx_remove: TxRemove) -> Self {
        molecule_pcn::TxRemove::new_builder()
            .channel_id(tx_remove.channel_id.into())
            .tx(tx_remove.tx)
            .build()
    }
}

impl TryFrom<molecule_pcn::TxRemove> for TxRemove {
    type Error = Error;

    fn try_from(tx_remove: molecule_pcn::TxRemove) -> Result<Self, Self::Error> {
        Ok(TxRemove {
            channel_id: tx_remove.channel_id(),
            tx: tx_remove.tx(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxComplete {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
}

impl From<TxComplete> for molecule_pcn::TxComplete {
    fn from(tx_complete: TxComplete) -> Self {
        molecule_pcn::TxComplete::new_builder()
            .channel_id(tx_complete.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::TxComplete> for TxComplete {
    type Error = Error;

    fn try_from(tx_complete: molecule_pcn::TxComplete) -> Result<Self, Self::Error> {
        Ok(TxComplete {
            channel_id: tx_complete.channel_id(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAbort {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub message: Vec<u8>,
}

impl From<TxAbort> for molecule_pcn::TxAbort {
    fn from(tx_abort: TxAbort) -> Self {
        molecule_pcn::TxAbort::new_builder()
            .channel_id(tx_abort.channel_id.into())
            .message(tx_abort.message.pack())
            .build()
    }
}

impl TryFrom<molecule_pcn::TxAbort> for TxAbort {
    type Error = Error;

    fn try_from(tx_abort: molecule_pcn::TxAbort) -> Result<Self, Self::Error> {
        Ok(TxAbort {
            channel_id: tx_abort.channel_id(),
            message: tx_abort.message().unpack(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxInitRBF {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub fee_rate: u64,
}

impl From<TxInitRBF> for molecule_pcn::TxInitRBF {
    fn from(tx_init_rbf: TxInitRBF) -> Self {
        molecule_pcn::TxInitRBF::new_builder()
            .channel_id(tx_init_rbf.channel_id.into())
            .fee_rate(tx_init_rbf.fee_rate.pack())
            .build()
    }
}

impl TryFrom<molecule_pcn::TxInitRBF> for TxInitRBF {
    type Error = Error;

    fn try_from(tx_init_rbf: molecule_pcn::TxInitRBF) -> Result<Self, Self::Error> {
        Ok(TxInitRBF {
            channel_id: tx_init_rbf.channel_id(),
            fee_rate: tx_init_rbf.fee_rate().unpack(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAckRBF {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
}

impl From<TxAckRBF> for molecule_pcn::TxAckRBF {
    fn from(tx_ack_rbf: TxAckRBF) -> Self {
        molecule_pcn::TxAckRBF::new_builder()
            .channel_id(tx_ack_rbf.channel_id.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::TxAckRBF> for TxAckRBF {
    type Error = Error;

    fn try_from(tx_ack_rbf: molecule_pcn::TxAckRBF) -> Result<Self, Self::Error> {
        Ok(TxAckRBF {
            channel_id: tx_ack_rbf.channel_id(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shutdown {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    #[serde_as(as = "EntityWrapperBase64<Script>")]
    pub close_script: Script,
}

impl From<Shutdown> for molecule_pcn::Shutdown {
    fn from(shutdown: Shutdown) -> Self {
        molecule_pcn::Shutdown::new_builder()
            .channel_id(shutdown.channel_id.into())
            .close_script(shutdown.close_script.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::Shutdown> for Shutdown {
    type Error = Error;

    fn try_from(shutdown: molecule_pcn::Shutdown) -> Result<Self, Self::Error> {
        Ok(Shutdown {
            channel_id: shutdown.channel_id(),
            close_script: shutdown.close_script(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosingSigned {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub fee: u64,
    pub signature: Signature,
}

impl From<ClosingSigned> for molecule_pcn::ClosingSigned {
    fn from(closing_signed: ClosingSigned) -> Self {
        molecule_pcn::ClosingSigned::new_builder()
            .channel_id(closing_signed.channel_id.into())
            .fee(closing_signed.fee.pack())
            .signature(closing_signed.signature.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::ClosingSigned> for ClosingSigned {
    type Error = Error;

    fn try_from(closing_signed: molecule_pcn::ClosingSigned) -> Result<Self, Self::Error> {
        Ok(ClosingSigned {
            channel_id: closing_signed.channel_id(),
            fee: closing_signed.fee().unpack(),
            signature: closing_signed.signature().try_into()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTlc {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub tlc_id: u64,
    pub amount: u64,
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub payment_hash: Byte32,
    pub expiry: u64,
}

impl From<AddTlc> for molecule_pcn::AddTlc {
    fn from(add_tlc: AddTlc) -> Self {
        molecule_pcn::AddTlc::new_builder()
            .channel_id(add_tlc.channel_id.into())
            .tlc_id(add_tlc.tlc_id.pack())
            .amount(add_tlc.amount.pack())
            .payment_hash(add_tlc.payment_hash.into())
            .expiry(add_tlc.expiry.pack())
            .build()
    }
}

impl TryFrom<molecule_pcn::AddTlc> for AddTlc {
    type Error = Error;

    fn try_from(add_tlc: molecule_pcn::AddTlc) -> Result<Self, Self::Error> {
        Ok(AddTlc {
            channel_id: add_tlc.channel_id(),
            tlc_id: add_tlc.tlc_id().unpack(),
            amount: add_tlc.amount().unpack(),
            payment_hash: add_tlc.payment_hash(),
            expiry: add_tlc.expiry().unpack(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlcsSigned {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub signature: Signature,
    pub tlc_signatures: Vec<Signature>,
}

impl From<TlcsSigned> for molecule_pcn::TlcsSigned {
    fn from(tlcs_signed: TlcsSigned) -> Self {
        molecule_pcn::TlcsSigned::new_builder()
            .channel_id(tlcs_signed.channel_id.into())
            .signature(tlcs_signed.signature.into())
            .tlc_signatures(
                SignatureVec::new_builder()
                    .set(
                        tlcs_signed
                            .tlc_signatures
                            .into_iter()
                            .map(|tlc_signature| tlc_signature.into())
                            .collect(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_pcn::TlcsSigned> for TlcsSigned {
    type Error = Error;

    fn try_from(tlcs_signed: molecule_pcn::TlcsSigned) -> Result<Self, Self::Error> {
        Ok(TlcsSigned {
            channel_id: tlcs_signed.channel_id(),
            signature: tlcs_signed.signature().try_into()?,
            tlc_signatures: tlcs_signed
                .tlc_signatures()
                .into_iter()
                .map(|tlc_signature| tlc_signature.try_into())
                .collect::<Result<Vec<Signature>, Error>>()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeAndAck {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub per_commitment_secret: Byte32,
    pub next_per_commitment_point: Pubkey,
}

impl From<RevokeAndAck> for molecule_pcn::RevokeAndAck {
    fn from(revoke_and_ack: RevokeAndAck) -> Self {
        molecule_pcn::RevokeAndAck::new_builder()
            .channel_id(revoke_and_ack.channel_id.into())
            .per_commitment_secret(revoke_and_ack.per_commitment_secret.into())
            .next_per_commitment_point(revoke_and_ack.next_per_commitment_point.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::RevokeAndAck> for RevokeAndAck {
    type Error = Error;

    fn try_from(revoke_and_ack: molecule_pcn::RevokeAndAck) -> Result<Self, Self::Error> {
        Ok(RevokeAndAck {
            channel_id: revoke_and_ack.channel_id(),
            per_commitment_secret: revoke_and_ack.per_commitment_secret(),
            next_per_commitment_point: revoke_and_ack.next_per_commitment_point().try_into()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveTlcFulfill {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub payment_preimage: Byte32,
}

impl From<RemoveTlcFulfill> for molecule_pcn::RemoveTlcFulfill {
    fn from(remove_tlc_fulfill: RemoveTlcFulfill) -> Self {
        molecule_pcn::RemoveTlcFulfill::new_builder()
            .payment_preimage(remove_tlc_fulfill.payment_preimage.into())
            .build()
    }
}

impl TryFrom<molecule_pcn::RemoveTlcFulfill> for RemoveTlcFulfill {
    type Error = Error;

    fn try_from(remove_tlc_fulfill: molecule_pcn::RemoveTlcFulfill) -> Result<Self, Self::Error> {
        Ok(RemoveTlcFulfill {
            payment_preimage: remove_tlc_fulfill.payment_preimage(),
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveTlcFail {
    pub error_code: u32,
}

impl From<RemoveTlcFail> for molecule_pcn::RemoveTlcFail {
    fn from(remove_tlc_fail: RemoveTlcFail) -> Self {
        molecule_pcn::RemoveTlcFail::new_builder()
            .error_code(remove_tlc_fail.error_code.pack())
            .build()
    }
}

impl TryFrom<molecule_pcn::RemoveTlcFail> for RemoveTlcFail {
    type Error = Error;

    fn try_from(remove_tlc_fail: molecule_pcn::RemoveTlcFail) -> Result<Self, Self::Error> {
        Ok(RemoveTlcFail {
            error_code: remove_tlc_fail.error_code().unpack(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoveTlcReason {
    RemoveTlcFulfill(RemoveTlcFulfill),
    RemoveTlcFail(RemoveTlcFail),
}

impl From<RemoveTlcReason> for molecule_pcn::RemoveTlcReasonUnion {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        match remove_tlc_reason {
            RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill) => {
                molecule_pcn::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill.into())
            }
            RemoveTlcReason::RemoveTlcFail(remove_tlc_fail) => {
                molecule_pcn::RemoveTlcReasonUnion::RemoveTlcFail(remove_tlc_fail.into())
            }
        }
    }
}

impl From<RemoveTlcReason> for molecule_pcn::RemoveTlcReason {
    fn from(remove_tlc_reason: RemoveTlcReason) -> Self {
        molecule_pcn::RemoveTlcReason::new_builder()
            .set(remove_tlc_reason)
            .build()
    }
}

impl TryFrom<molecule_pcn::RemoveTlcReason> for RemoveTlcReason {
    type Error = Error;

    fn try_from(remove_tlc_reason: molecule_pcn::RemoveTlcReason) -> Result<Self, Self::Error> {
        match remove_tlc_reason.to_enum() {
            molecule_pcn::RemoveTlcReasonUnion::RemoveTlcFulfill(remove_tlc_fulfill) => Ok(
                RemoveTlcReason::RemoveTlcFulfill(remove_tlc_fulfill.try_into()?),
            ),
            molecule_pcn::RemoveTlcReasonUnion::RemoveTlcFail(remove_tlc_fail) => {
                Ok(RemoveTlcReason::RemoveTlcFail(remove_tlc_fail.try_into()?))
            }
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveTlc {
    #[serde_as(as = "EntityWrapperHex<Byte32>")]
    pub channel_id: Byte32,
    pub tlc_id: u64,
    pub reason: RemoveTlcReason,
}

impl From<RemoveTlc> for molecule_pcn::RemoveTlc {
    fn from(remove_tlc: RemoveTlc) -> Self {
        molecule_pcn::RemoveTlc::new_builder()
            .channel_id(remove_tlc.channel_id.into())
            .tlc_id(remove_tlc.tlc_id.pack())
            .reason(
                molecule_pcn::RemoveTlcReason::new_builder()
                    .set(remove_tlc.reason)
                    .build(),
            )
            .build()
    }
}

impl TryFrom<molecule_pcn::RemoveTlc> for RemoveTlc {
    type Error = Error;

    fn try_from(remove_tlc: molecule_pcn::RemoveTlc) -> Result<Self, Self::Error> {
        Ok(RemoveTlc {
            channel_id: remove_tlc.channel_id(),
            tlc_id: remove_tlc.tlc_id().unpack(),
            reason: remove_tlc.reason().try_into()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestMessage {
    #[serde_as(as = "Base64")]
    pub bytes: Vec<u8>,
}

impl From<TestMessage> for molecule_pcn::TestMessage {
    fn from(test_message: TestMessage) -> Self {
        molecule_pcn::TestMessage::new_builder()
            .bytes(test_message.bytes.pack())
            .build()
    }
}

impl TryFrom<molecule_pcn::TestMessage> for TestMessage {
    type Error = Error;

    fn try_from(test_message: molecule_pcn::TestMessage) -> Result<Self, Self::Error> {
        Ok(TestMessage {
            bytes: test_message.bytes().unpack(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PCNMessage {
    TestMessage(TestMessage),
    OpenChannel(OpenChannel),
    AcceptChannel(AcceptChannel),
    CommitmentSigned(CommitmentSigned),
    TxSignatures(TxSignatures),
    ChannelReady(ChannelReady),
    TxAdd(TxAdd),
    TxRemove(TxRemove),
    TxComplete(TxComplete),
    TxAbort(TxAbort),
    TxInitRBF(TxInitRBF),
    TxAckRBF(TxAckRBF),
    Shutdown(Shutdown),
    ClosingSigned(ClosingSigned),
    AddTlc(AddTlc),
    TlcsSigned(TlcsSigned),
    RevokeAndAck(RevokeAndAck),
    RemoveTlc(RemoveTlc),
}

impl From<PCNMessage> for molecule_pcn::PCNMessageUnion {
    fn from(pcn_message: PCNMessage) -> Self {
        match pcn_message {
            PCNMessage::TestMessage(test_message) => {
                molecule_pcn::PCNMessageUnion::TestMessage(test_message.into())
            }
            PCNMessage::OpenChannel(open_channel) => {
                molecule_pcn::PCNMessageUnion::OpenChannel(open_channel.into())
            }
            PCNMessage::AcceptChannel(accept_channel) => {
                molecule_pcn::PCNMessageUnion::AcceptChannel(accept_channel.into())
            }
            PCNMessage::CommitmentSigned(commitment_signed) => {
                molecule_pcn::PCNMessageUnion::CommitmentSigned(commitment_signed.into())
            }
            PCNMessage::TxSignatures(tx_signatures) => {
                molecule_pcn::PCNMessageUnion::TxSignatures(tx_signatures.into())
            }
            PCNMessage::ChannelReady(channel_ready) => {
                molecule_pcn::PCNMessageUnion::ChannelReady(channel_ready.into())
            }
            PCNMessage::TxAdd(tx_add) => molecule_pcn::PCNMessageUnion::TxAdd(tx_add.into()),
            PCNMessage::TxRemove(tx_remove) => {
                molecule_pcn::PCNMessageUnion::TxRemove(tx_remove.into())
            }
            PCNMessage::TxComplete(tx_complete) => {
                molecule_pcn::PCNMessageUnion::TxComplete(tx_complete.into())
            }
            PCNMessage::TxAbort(tx_abort) => {
                molecule_pcn::PCNMessageUnion::TxAbort(tx_abort.into())
            }
            PCNMessage::TxInitRBF(tx_init_rbf) => {
                molecule_pcn::PCNMessageUnion::TxInitRBF(tx_init_rbf.into())
            }
            PCNMessage::TxAckRBF(tx_ack_rbf) => {
                molecule_pcn::PCNMessageUnion::TxAckRBF(tx_ack_rbf.into())
            }
            PCNMessage::Shutdown(shutdown) => {
                molecule_pcn::PCNMessageUnion::Shutdown(shutdown.into())
            }
            PCNMessage::ClosingSigned(closing_signed) => {
                molecule_pcn::PCNMessageUnion::ClosingSigned(closing_signed.into())
            }
            PCNMessage::AddTlc(add_tlc) => molecule_pcn::PCNMessageUnion::AddTlc(add_tlc.into()),
            PCNMessage::RemoveTlc(remove_tlc) => {
                molecule_pcn::PCNMessageUnion::RemoveTlc(remove_tlc.into())
            }
            PCNMessage::RevokeAndAck(revoke_and_ack) => {
                molecule_pcn::PCNMessageUnion::RevokeAndAck(revoke_and_ack.into())
            }
            PCNMessage::TlcsSigned(tlcs_signed) => {
                molecule_pcn::PCNMessageUnion::TlcsSigned(tlcs_signed.into())
            }
        }
    }
}

impl From<PCNMessage> for molecule_pcn::PCNMessage {
    fn from(pcn_message: PCNMessage) -> Self {
        molecule_pcn::PCNMessage::new_builder()
            .set(pcn_message)
            .build()
    }
}

impl TryFrom<molecule_pcn::PCNMessage> for PCNMessage {
    type Error = Error;

    fn try_from(pcn_message: molecule_pcn::PCNMessage) -> Result<Self, Self::Error> {
        Ok(match pcn_message.to_enum() {
            molecule_pcn::PCNMessageUnion::TestMessage(test_message) => {
                PCNMessage::TestMessage(test_message.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::OpenChannel(open_channel) => {
                PCNMessage::OpenChannel(open_channel.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::AcceptChannel(accept_channel) => {
                PCNMessage::AcceptChannel(accept_channel.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::CommitmentSigned(commitment_signed) => {
                PCNMessage::CommitmentSigned(commitment_signed.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::TxSignatures(tx_signatures) => {
                PCNMessage::TxSignatures(tx_signatures.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::ChannelReady(channel_ready) => {
                PCNMessage::ChannelReady(channel_ready.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::TxAdd(tx_add) => PCNMessage::TxAdd(tx_add.try_into()?),
            molecule_pcn::PCNMessageUnion::TxRemove(tx_remove) => {
                PCNMessage::TxRemove(tx_remove.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::TxComplete(tx_complete) => {
                PCNMessage::TxComplete(tx_complete.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::TxAbort(tx_abort) => {
                PCNMessage::TxAbort(tx_abort.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::TxInitRBF(tx_init_rbf) => {
                PCNMessage::TxInitRBF(tx_init_rbf.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::TxAckRBF(tx_ack_rbf) => {
                PCNMessage::TxAckRBF(tx_ack_rbf.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::Shutdown(shutdown) => {
                PCNMessage::Shutdown(shutdown.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::ClosingSigned(closing_signed) => {
                PCNMessage::ClosingSigned(closing_signed.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::AddTlc(add_tlc) => {
                PCNMessage::AddTlc(add_tlc.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::RemoveTlc(remove_tlc) => {
                PCNMessage::RemoveTlc(remove_tlc.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::TlcsSigned(tlcs_signed) => {
                PCNMessage::TlcsSigned(tlcs_signed.try_into()?)
            }
            molecule_pcn::PCNMessageUnion::RevokeAndAck(revoke_and_ack) => {
                PCNMessage::RevokeAndAck(revoke_and_ack.try_into()?)
            }
        })
    }
}

macro_rules! impl_traits {
    ($t:ident) => {
        impl $t {
            pub fn to_molecule_bytes(&self) -> molecule::bytes::Bytes {
                // TODO: we cloned twice here, both in self.clone and as_bytes.
                molecule_pcn::$t::from(self.clone()).as_bytes()
            }
        }

        impl $t {
            pub fn from_molecule_slice(data: &[u8]) -> Result<Self, Error> {
                molecule_pcn::$t::from_slice(data)
                    .map_err(Into::into)
                    .and_then(TryInto::try_into)
            }
        }
    };
}

impl_traits!(TestMessage);
impl_traits!(OpenChannel);
impl_traits!(AcceptChannel);
impl_traits!(CommitmentSigned);
impl_traits!(TxSignatures);
impl_traits!(ChannelReady);
impl_traits!(TxAdd);
impl_traits!(TxRemove);
impl_traits!(TxComplete);
impl_traits!(TxAbort);
impl_traits!(TxInitRBF);
impl_traits!(TxAckRBF);
impl_traits!(Shutdown);
impl_traits!(ClosingSigned);
impl_traits!(AddTlc);
impl_traits!(TlcsSigned);
impl_traits!(RevokeAndAck);
impl_traits!(RemoveTlc);
impl_traits!(PCNMessage);
