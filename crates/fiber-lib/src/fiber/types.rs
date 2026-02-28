use super::channel::ProcessingChannelError;
use crate::ckb::contracts::get_udt_whitelist;
use anyhow::anyhow;
use ckb_jsonrpc_types::CellOutput;
use ckb_types::{
    core::FeeRate,
    packed::{Byte32 as MByte32, BytesVec, OutPoint, Script, Transaction},
    prelude::{Pack, Unpack},
};
use core::fmt::{self, Formatter};
use fiber_sphinx::SphinxError;
use fiber_types::get_chain_hash;
use fiber_types::molecule_table_data_len;
use fiber_types::protocol::AnnouncedNodeName;
pub use fiber_types::{
    gen::fiber::{self as molecule_fiber, CustomRecordsOpt, PaymentPreimageOpt, PubNonceOpt},
    gen::gossip::{self as molecule_gossip},
    BasicMppPaymentData, BroadcastMessage, BroadcastMessageID, ChannelAnnouncement, ChannelFlags,
    ChannelTlcInfo, ChannelUpdate, ChannelUpdateChannelFlags, ChannelUpdateMessageFlags, Cursor,
    EcdsaSignature, FeatureVector, Hash256, HashAlgorithm, NodeAnnouncement, OnionPacketError,
    PartialSignatureAsBytes, PaymentCustomRecords, PaymentOnionPacket, PeeledPaymentOnionPacket,
    Privkey, PubNonceAsBytes, Pubkey, RemoveTlcReason, RevokeAndAck, TlcErr,
    UnknownHashAlgorithmError, CURSOR_SIZE, ONION_PACKET_VERSION_V1,
};

use molecule::prelude::{Builder, Byte, Entity};
use musig2::{PartialSignature, PubNonce};
use secp256k1::{PublicKey, Secp256k1, Signing};
use secp256k1::{Verification, XOnlyPublicKey};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::convert::TryFrom;
use std::fmt::Debug;
use std::fmt::Display;
use tentacle::multiaddr::MultiAddr;
use thiserror::Error;

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
    OnionPacket(#[from] fiber_types::OnionPacketError),
    #[error("Error: {0}")]
    AnyHow(#[from] anyhow::Error),
}

impl From<musig2::errors::DecodeError<PubNonce>> for Error {
    fn from(e: musig2::errors::DecodeError<PubNonce>) -> Self {
        Error::Musig2(format!("{e}"))
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(e: std::convert::Infallible) -> Self {
        match e {}
    }
}

#[derive(Clone, Debug)]
pub struct Init {
    pub features: FeatureVector,
    pub chain_hash: Hash256,
}

impl From<Init> for molecule_fiber::Init {
    fn from(init: Init) -> Self {
        molecule_fiber::Init::new_builder()
            .features(init.features.bytes().pack())
            .chain_hash(init.chain_hash.into())
            .build()
    }
}
impl TryFrom<molecule_fiber::Init> for Init {
    type Error = Error;

    fn try_from(init: molecule_fiber::Init) -> Result<Self, Self::Error> {
        Ok(Init {
            features: FeatureVector::from(init.features().unpack()),
            chain_hash: init.chain_hash().into(),
        })
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
    pub next_commitment_nonce: PubNonce,
    pub next_revocation_nonce: PubNonce,
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

    pub fn is_one_way(&self) -> bool {
        self.channel_flags.contains(ChannelFlags::ONE_WAY)
    }

    pub fn mem_size(&self) -> usize {
        let static_size = std::mem::size_of_val(self);
        let funding_udt_type_script_size = self
            .funding_udt_type_script
            .as_ref()
            .map(|script| script.total_size())
            .unwrap_or_default();
        let shutdown_script_size = self.shutdown_script.total_size();
        static_size + funding_udt_type_script_size + shutdown_script_size
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
            .next_commitment_nonce(open_channel.next_commitment_nonce.into())
            .next_revocation_nonce(open_channel.next_revocation_nonce.into())
            .channel_announcement_nonce(
                PubNonceOpt::new_builder()
                    .set(open_channel.channel_announcement_nonce.map(Into::into))
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
            next_commitment_nonce: open_channel.next_commitment_nonce().try_into()?,
            next_revocation_nonce: open_channel.next_revocation_nonce().try_into()?,
            channel_announcement_nonce: open_channel
                .channel_announcement_nonce()
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
    pub next_commitment_nonce: PubNonce,
    pub next_revocation_nonce: PubNonce,
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
            .channel_announcement_nonce(
                PubNonceOpt::new_builder()
                    .set(accept_channel.channel_announcement_nonce.map(Into::into))
                    .build(),
            )
            .next_commitment_nonce(accept_channel.next_commitment_nonce.into())
            .next_revocation_nonce(accept_channel.next_revocation_nonce.into())
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
                .channel_announcement_nonce()
                .to_opt()
                .map(TryInto::try_into)
                .transpose()
                .map_err(|err| Error::Musig2(format!("{err}")))?,
            next_commitment_nonce: accept_channel.next_commitment_nonce().try_into()?,
            next_revocation_nonce: accept_channel.next_revocation_nonce().try_into()?,
        })
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitmentSigned {
    pub channel_id: Hash256,
    #[serde_as(as = "PartialSignatureAsBytes")]
    pub funding_tx_partial_signature: PartialSignature,
    #[serde_as(as = "PubNonceAsBytes")]
    pub next_commitment_nonce: PubNonce,
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
            .next_commitment_nonce(commitment_signed.next_commitment_nonce.into())
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
            next_commitment_nonce: commitment_signed.next_commitment_nonce().try_into()?,
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
    pub next_commitment_nonce: PubNonce,
}

impl From<TxComplete> for molecule_fiber::TxComplete {
    fn from(tx_complete: TxComplete) -> Self {
        molecule_fiber::TxComplete::new_builder()
            .channel_id(tx_complete.channel_id.into())
            .next_commitment_nonce(tx_complete.next_commitment_nonce.into())
            .build()
    }
}

impl TryFrom<molecule_fiber::TxComplete> for TxComplete {
    type Error = Error;

    fn try_from(tx_complete: molecule_fiber::TxComplete) -> Result<Self, Self::Error> {
        Ok(TxComplete {
            channel_id: tx_complete.channel_id().into(),
            next_commitment_nonce: tx_complete.next_commitment_nonce().try_into()?,
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
            tlc_fee_proportional_millionths: update_tlc_info.tlc_fee_proportional_millionths,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AddTlc {
    pub channel_id: Hash256,
    pub tlc_id: u64,
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    pub onion_packet: Option<PaymentOnionPacket>,
}

impl Debug for AddTlc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddTlc")
            .field("channel_id", &self.channel_id)
            .field("tlc_id", &self.tlc_id)
            .field("amount", &self.amount)
            .field("payment_hash", &self.payment_hash)
            .field("expiry", &self.expiry)
            .field("hash_algorithm", &self.hash_algorithm)
            .finish()
    }
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
            (!onion_packet_bytes.is_empty()).then(|| PaymentOnionPacket::new(onion_packet_bytes));
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
            reason: remove_tlc.reason().into(),
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
    pub add_tlc_result: Result<(Hash256, u64), (ProcessingChannelError, TlcErr)>,
}

pub fn new_node_announcement_unsigned(
    node_name: AnnouncedNodeName,
    features: FeatureVector,
    addresses: Vec<MultiAddr>,
    node_id: Pubkey,
    timestamp: u64,
    auto_accept_min_ckb_funding_amount: u64,
) -> NodeAnnouncement {
    NodeAnnouncement {
        signature: None,
        features,
        timestamp,
        node_id,
        version: env!("CARGO_PKG_VERSION").to_string(),
        node_name,
        chain_hash: get_chain_hash(),
        addresses,
        auto_accept_min_ckb_funding_amount,
        udt_cfg_infos: get_udt_whitelist(),
    }
}

pub fn new_node_announcement(
    node_name: AnnouncedNodeName,
    features: FeatureVector,
    addresses: Vec<MultiAddr>,
    private_key: &Privkey,
    timestamp: u64,
    auto_accept_min_ckb_funding_amount: u64,
) -> NodeAnnouncement {
    let mut unsigned = new_node_announcement_unsigned(
        node_name,
        features,
        addresses,
        private_key.pubkey(),
        timestamp,
        auto_accept_min_ckb_funding_amount,
    );
    unsigned.signature = Some(private_key.sign(unsigned.message_to_sign()));
    unsigned
}

pub fn new_channel_announcement_unsigned(
    node1_pubkey: &Pubkey,
    node2_pubkey: &Pubkey,
    channel_outpoint: OutPoint,
    ckb_pubkey: &XOnlyPublicKey,
    capacity: u128,
    udt_type_script: Option<Script>,
) -> ChannelAnnouncement {
    ChannelAnnouncement {
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

#[derive(Debug, Clone)]
pub enum FiberQueryInformation {
    GetBroadcastMessages(GetBroadcastMessages),
    GetBroadcastMessagesResult(GetBroadcastMessagesResult),
}

#[derive(Debug, Clone)]
pub enum FiberMessage {
    Init(Init),
    ChannelInitialization(OpenChannel),
    ChannelNormalOperation(FiberChannelMessage),
}

impl FiberMessage {
    pub fn init(init_message: Init) -> Self {
        FiberMessage::Init(init_message)
    }

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
            FiberChannelMessage::AnnouncementSignatures(announcement_signatures) => {
                announcement_signatures.channel_id
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

pub fn broadcast_message_to_gossip(msg: &BroadcastMessage) -> GossipMessage {
    GossipMessage::BroadcastMessagesFilterResult(BroadcastMessagesFilterResult {
        messages: vec![msg.clone()],
    })
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
                .map(|message| BroadcastMessage::try_from(message).map_err(Error::from))
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
                .map(|message| BroadcastMessage::try_from(message).map_err(Error::from))
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
                .map(|message| BroadcastMessage::try_from(message).map_err(Error::from))
                .collect::<Result<Vec<BroadcastMessage>, Error>>()?,
            missing_queries: query_broadcast_messages_result
                .missing_queries()
                .into_iter()
                .map(u16::from)
                .collect(),
        })
    }
}

impl From<FiberMessage> for molecule_fiber::FiberMessageUnion {
    fn from(fiber_message: FiberMessage) -> Self {
        match fiber_message {
            FiberMessage::Init(init) => molecule_fiber::FiberMessageUnion::Init(init.into()),
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
            molecule_fiber::FiberMessageUnion::Init(init) => FiberMessage::Init(init.try_into()?),
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

/// Trampoline onion hop payload.
///
/// This is carried inside the *inner* trampoline onion packet (which is itself embedded in
/// the `custom_records` of the *outer* payment onion hop payload via `TrampolineOnionData`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrampolineHopPayload {
    /// Payload for a trampoline node.
    Forward {
        /// Next node in the trampoline route (could be another trampoline or the final recipient).
        next_node_id: Pubkey,
        /// Amount that should be forwarded to `next_node_id` (excluding this node's fee).
        amount_to_forward: u128,
        /// Hash algorighmm used for the payment.
        hash_algorithm: HashAlgorithm,
        /// Fee budget used when building the *outer* route from this trampoline node to `next_node_id`.
        ///
        /// This is computed during the initial router build (trampoline onion construction), so
        /// trampoline forwarding does not need to re-derive fee allocation.
        build_max_fee_amount: u128,
        /// TLC expiry delta required at `next_node_id` to complete the remaining payment.
        ///
        /// For the final recipient this is the invoice's `final_tlc_expiry_delta`. For trampoline
        /// hops it also includes additional slack so the trampoline can forward further.
        tlc_expiry_delta: u64,

        /// Upper bound on the TLC expiry delta used when the next trampoline hop builds its outer
        /// route.
        ///
        /// This allows propagating the payer's expiry budget constraints across trampoline hops.
        tlc_expiry_limit: u64,

        /// whether trampoline node should use MPP when forwarding to next hop
        max_parts: Option<u64>,
    },
    /// Payload for the final recipient.
    Final {
        /// Amount that the final recipient should receive.
        final_amount: u128,
        /// Final hop expiry delta required by the invoice.
        final_tlc_expiry_delta: u64,
        /// Optional payment preimage (keysend).
        payment_preimage: Option<Hash256>,
        /// Custom records that must reach the final recipient (e.g. MPP records).
        custom_records: Option<PaymentCustomRecords>,
    },
}

impl TrampolineHopPayload {
    pub fn serialize(&self) -> Vec<u8> {
        molecule_fiber::TrampolineHopPayload::from(self.clone())
            .as_bytes()
            .to_vec()
    }

    pub fn deserialize(data: &[u8]) -> Option<Self> {
        molecule_fiber::TrampolineHopPayload::from_slice(data)
            .ok()
            .and_then(|p| TrampolineHopPayload::try_from(p).ok())
    }
}

impl From<TrampolineHopPayload> for molecule_fiber::TrampolineHopPayload {
    fn from(payload: TrampolineHopPayload) -> Self {
        match payload {
            TrampolineHopPayload::Forward {
                next_node_id,
                amount_to_forward,
                hash_algorithm,
                build_max_fee_amount,
                tlc_expiry_delta,
                tlc_expiry_limit,
                max_parts,
            } => {
                let forward = molecule_fiber::TrampolineForwardPayload::new_builder()
                    .next_node_id(next_node_id.into())
                    .amount_to_forward(amount_to_forward.pack())
                    .hash_algorithm(Byte::new(hash_algorithm as u8))
                    .build_max_fee_amount(build_max_fee_amount.pack())
                    .tlc_expiry_delta(tlc_expiry_delta.pack())
                    .tlc_expiry_limit(tlc_expiry_limit.pack())
                    .max_parts(
                        molecule_fiber::Uint64Opt::new_builder()
                            .set(max_parts.map(|x| x.pack()))
                            .build(),
                    )
                    .build();
                molecule_fiber::TrampolineHopPayload::new_builder()
                    .set(forward)
                    .build()
            }
            TrampolineHopPayload::Final {
                final_amount,
                final_tlc_expiry_delta,
                payment_preimage,
                custom_records,
            } => {
                let final_payload = molecule_fiber::TrampolineFinalPayload::new_builder()
                    .final_amount(final_amount.pack())
                    .final_tlc_expiry_delta(final_tlc_expiry_delta.pack())
                    .payment_preimage(
                        PaymentPreimageOpt::new_builder()
                            .set(payment_preimage.map(|x| x.into()))
                            .build(),
                    )
                    .custom_records(
                        CustomRecordsOpt::new_builder()
                            .set(custom_records.map(|x| x.into()))
                            .build(),
                    )
                    .build();
                molecule_fiber::TrampolineHopPayload::new_builder()
                    .set(final_payload)
                    .build()
            }
        }
    }
}

impl TryFrom<molecule_fiber::TrampolineHopPayload> for TrampolineHopPayload {
    type Error = Error;

    fn try_from(payload: molecule_fiber::TrampolineHopPayload) -> Result<Self, Self::Error> {
        match payload.to_enum() {
            molecule_fiber::TrampolineHopPayloadUnion::TrampolineForwardPayload(forward) => {
                Ok(TrampolineHopPayload::Forward {
                    next_node_id: forward.next_node_id().try_into()?,
                    amount_to_forward: forward.amount_to_forward().unpack(),
                    hash_algorithm: forward.hash_algorithm().try_into().unwrap_or_default(),
                    build_max_fee_amount: forward.build_max_fee_amount().unpack(),
                    tlc_expiry_delta: forward.tlc_expiry_delta().unpack(),
                    tlc_expiry_limit: forward.tlc_expiry_limit().unpack(),
                    max_parts: forward.max_parts().to_opt().map(|x| x.unpack()),
                })
            }
            molecule_fiber::TrampolineHopPayloadUnion::TrampolineFinalPayload(final_payload) => {
                Ok(TrampolineHopPayload::Final {
                    final_amount: final_payload.final_amount().unpack(),
                    final_tlc_expiry_delta: final_payload.final_tlc_expiry_delta().unpack(),
                    payment_preimage: final_payload.payment_preimage().to_opt().map(|x| x.into()),
                    custom_records: final_payload.custom_records().to_opt().map(|x| x.into()),
                })
            }
        }
    }
}

/// Inner trampoline onion packet bytes.
///
/// Uses the same Sphinx construction as the outer payment onion, but is intended to be smaller so
/// it can be embedded inside the outer onion's hop payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrampolineOnionPacket {
    data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeeledTrampolineOnionPacket {
    pub current: TrampolineHopPayload,
    pub shared_secret: [u8; 32],
    pub next: Option<TrampolineOnionPacket>,
}

const TRAMPOLINE_PACKET_DATA_LEN: usize = 1300;

trait SphinxOnionCodec {
    type Decoded;
    type Current;

    const PACKET_DATA_LEN: usize;
    /// The onion packet version used when creating new packets.
    const CURRENT_VERSION: u8;

    /// Packs the decoded data for transmission. Must use `CURRENT_VERSION` format.
    fn pack(decoded: &Self::Decoded) -> Vec<u8>;
    /// Unpacks data received from the network. Must handle all versions allowed by `is_version_allowed`.
    fn unpack(version: u8, buf: &[u8]) -> Option<Self::Decoded>;
    fn to_current(decoded: Self::Decoded) -> Self::Current;
    /// Returns true if the given version is allowed for this codec.
    fn is_version_allowed(version: u8) -> bool;
    /// Returns the total length of hop data (including any headers) for the specified version.
    fn hop_data_len(version: u8, buf: &[u8]) -> Option<usize>;
}

struct SphinxPeeled<Current> {
    current: Current,
    shared_secret: [u8; 32],
    next: Option<Vec<u8>>,
}

fn peel_sphinx_onion<C: Verification, Codec: SphinxOnionCodec>(
    packet_bytes: Vec<u8>,
    peeler: &Privkey,
    assoc_data: Option<&[u8]>,
    secp_ctx: &Secp256k1<C>,
) -> Result<SphinxPeeled<Codec::Current>, Error> {
    let sphinx_packet = fiber_sphinx::OnionPacket::from_bytes(packet_bytes)
        .map_err(|err| Error::OnionPacket(err.into()))?;
    let version = sphinx_packet.version;
    if !Codec::is_version_allowed(version) {
        return Err(Error::OnionPacket(OnionPacketError::UnknownVersion(
            version,
        )));
    }
    let shared_secret = sphinx_packet.shared_secret(&peeler.0);

    let (new_current, new_next) = sphinx_packet
        .peel(&peeler.0, assoc_data, secp_ctx, |buf| {
            Codec::hop_data_len(version, buf)
        })
        .map_err(|err| Error::OnionPacket(err.into()))?;

    let decoded = Codec::unpack(version, &new_current)
        .ok_or_else(|| Error::OnionPacket(OnionPacketError::InvalidHopData))?;
    let current = Codec::to_current(decoded);

    // All zeros hmac indicates the last hop.
    let next = new_next
        .hmac
        .iter()
        .any(|b| *b != 0)
        .then(|| new_next.into_bytes());

    Ok(SphinxPeeled {
        current,
        shared_secret,
        next,
    })
}

fn create_sphinx_onion<C: Signing, Codec: SphinxOnionCodec>(
    session_key: Privkey,
    hops_path: Vec<Pubkey>,
    payloads: Vec<Codec::Decoded>,
    assoc_data: Option<Vec<u8>>,
    secp_ctx: &Secp256k1<C>,
) -> Result<Vec<u8>, Error> {
    if hops_path.is_empty() {
        return Err(Error::OnionPacket(SphinxError::HopsIsEmpty.into()));
    }
    if hops_path.len() != payloads.len() {
        return Err(Error::OnionPacket(OnionPacketError::InvalidHopData));
    }

    let hops_path: Vec<PublicKey> = hops_path.into_iter().map(Into::into).collect();
    let hops_data = payloads.iter().map(Codec::pack).collect();

    let mut sphinx_packet = fiber_sphinx::OnionPacket::create(
        session_key.into(),
        hops_path,
        hops_data,
        assoc_data,
        Codec::PACKET_DATA_LEN,
        secp_ctx,
    )
    .map_err(|err| Error::OnionPacket(err.into()))?;
    // Set the version to indicate which hop data format is used
    sphinx_packet.version = Codec::CURRENT_VERSION;
    Ok(sphinx_packet.into_bytes())
}

struct TrampolineSphinxCodec;

impl SphinxOnionCodec for TrampolineSphinxCodec {
    type Decoded = TrampolineHopPayload;
    type Current = TrampolineHopPayload;

    const PACKET_DATA_LEN: usize = TRAMPOLINE_PACKET_DATA_LEN;
    // Trampoline uses molecule enum serialization (v1 format without u64 length header)
    const CURRENT_VERSION: u8 = ONION_PACKET_VERSION_V1;

    fn pack(decoded: &Self::Decoded) -> Vec<u8> {
        decoded.serialize()
    }

    fn unpack(version: u8, buf: &[u8]) -> Option<Self::Decoded> {
        if !Self::is_version_allowed(version) {
            return None;
        }
        let len = molecule_enum_of_tables_data_len(buf)?;
        if buf.len() < len {
            return None;
        }
        TrampolineHopPayload::deserialize(&buf[..len])
    }

    fn to_current(decoded: Self::Decoded) -> Self::Current {
        decoded
    }

    fn is_version_allowed(version: u8) -> bool {
        // Trampoline only supports v1 (molecule enum without u64 length header)
        version == ONION_PACKET_VERSION_V1
    }

    fn hop_data_len(version: u8, buf: &[u8]) -> Option<usize> {
        if !Self::is_version_allowed(version) {
            return None;
        }
        // Trampoline uses molecule enum where all branches are tables
        molecule_enum_of_tables_data_len(buf)
    }
}

impl TrampolineOnionPacket {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    pub fn into_sphinx_onion_packet(self) -> Result<fiber_sphinx::OnionPacket, Error> {
        fiber_sphinx::OnionPacket::from_bytes(self.data)
            .map_err(|err| Error::OnionPacket(err.into()))
    }

    pub fn peel<C: Verification>(
        self,
        peeler: &Privkey,
        assoc_data: Option<&[u8]>,
        secp_ctx: &Secp256k1<C>,
    ) -> Result<PeeledTrampolineOnionPacket, Error> {
        let peeled =
            peel_sphinx_onion::<C, TrampolineSphinxCodec>(self.data, peeler, assoc_data, secp_ctx)?;
        Ok(PeeledTrampolineOnionPacket {
            current: peeled.current,
            next: peeled.next.map(TrampolineOnionPacket::new),
            shared_secret: peeled.shared_secret,
        })
    }

    /// Create a trampoline onion destined to `hops_path[0]`.
    ///
    /// `hops_path.len()` must equal `payloads.len()` and must not be empty.
    pub fn create<C: Signing>(
        session_key: Privkey,
        hops_path: Vec<Pubkey>,
        payloads: Vec<TrampolineHopPayload>,
        assoc_data: Option<Vec<u8>>,
        secp_ctx: &Secp256k1<C>,
    ) -> Result<Self, Error> {
        Ok(TrampolineOnionPacket::new(create_sphinx_onion::<
            C,
            TrampolineSphinxCodec,
        >(
            session_key,
            hops_path,
            payloads,
            assoc_data,
            secp_ctx,
        )?))
    }
}

/// Helper free function to extract MPP custom records from a peeled payment onion packet.
pub fn peeled_packet_mpp_custom_records(
    peeled: &PeeledPaymentOnionPacket,
) -> Option<BasicMppPaymentData> {
    peeled.mpp_custom_records()
}

/// Returns the total length of a molecule enum where all branches are tables.
/// Layout: [item_id: u32 LE][table_data]. The table's first u32 LE is its total length.
/// Total length = 4 (item_id) + table_length.
/// Used by trampoline hop data (v1 format).
fn molecule_enum_of_tables_data_len(buf: &[u8]) -> Option<usize> {
    let table_len = molecule_table_data_len(buf.get(molecule::NUMBER_SIZE..)?)?;
    molecule::NUMBER_SIZE.checked_add(table_len)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HoldTlc {
    pub channel_id: Hash256,
    pub tlc_id: u64,
    pub hold_expire_at: u64,
}
