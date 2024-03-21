use ckb_types::{
    packed::{Byte32, BytesVec, Script, ScriptOpt, Transaction},
    prelude::Pack,
};
use molecule::prelude::{Builder, Entity};

use super::gen::pcn::{self as molecule_pcn, Byte64Vec};

pub struct Point(pub Vec<u8>);

impl From<Point> for molecule_pcn::Byte33 {
    fn from(_point: Point) -> molecule_pcn::Byte33 {
        unimplemented!("Point to Byte33");
    }
}

pub struct OpenChannel {
    chain_hash: Byte32,
    channel_id: Byte32,
    funding_type_script: ScriptOpt,
    funding_amount: u64,
    funding_fee_rate: u64,
    commitment_fee_rate: u64,
    max_tlc_value_in_flight: u64,
    max_accept_tlcs: u64,
    min_tlc_value: u64,
    to_self_delay: u64,
    funding_pubkey: Point,
    revocation_basepoint: Point,
    payment_basepoint: Point,
    delayed_payment_basepoint: Point,
    tlc_basepoint: Point,
    first_per_commitment_point: Point,
    second_per_commitment_point: Point,
    channel_flags: u8,
}

impl From<OpenChannel> for molecule_pcn::OpenChannel {
    fn from(open_channel: OpenChannel) -> Self {
        molecule_pcn::OpenChannel::new_builder()
            .chain_hash(open_channel.chain_hash.into())
            .channel_id(open_channel.channel_id.into())
            .funding_type_script(open_channel.funding_type_script.into())
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

pub struct AcceptChannel {
    channel_id: Byte32,
    funding_amount: u64,
    max_tlc_value_in_flight: u64,
    max_accept_tlcs: u64,
    min_tlc_value: u64,
    to_self_delay: u64,
    funding_pubkey: Point,
    revocation_basepoint: Point,
    payment_basepoint: Point,
    delayed_payment_basepoint: Point,
    tlc_basepoint: Point,
    first_per_commitment_point: Point,
    second_per_commitment_point: Point,
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

pub struct Signature(pub Vec<u8>);

impl From<Signature> for molecule_pcn::Byte64 {
    fn from(_signature: Signature) -> molecule_pcn::Byte64 {
        unimplemented!("Signature to Byte64");
    }
}

pub struct CommitmentSigned {
    channel_id: Byte32,
    signature: Signature,
}

impl From<CommitmentSigned> for molecule_pcn::CommitmentSigned {
    fn from(commitment_signed: CommitmentSigned) -> Self {
        molecule_pcn::CommitmentSigned::new_builder()
            .channel_id(commitment_signed.channel_id.into())
            .signature(commitment_signed.signature.into())
            .build()
    }
}

pub struct TxSignatures {
    channel_id: Byte32,
    tx_hash: Byte32,
    witnesses: Vec<Vec<u8>>,
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

pub struct ChannelReady {
    channel_id: Byte32,
}

impl From<ChannelReady> for molecule_pcn::ChannelReady {
    fn from(channel_ready: ChannelReady) -> Self {
        molecule_pcn::ChannelReady::new_builder()
            .channel_id(channel_ready.channel_id.into())
            .build()
    }
}

pub struct TxAdd {
    channel_id: Byte32,
    tx: Transaction,
}

impl From<TxAdd> for molecule_pcn::TxAdd {
    fn from(tx_add: TxAdd) -> Self {
        molecule_pcn::TxAdd::new_builder()
            .channel_id(tx_add.channel_id.into())
            .tx(tx_add.tx)
            .build()
    }
}

pub struct TxRemove {
    channel_id: Byte32,
    tx: Transaction,
}

impl From<TxRemove> for molecule_pcn::TxRemove {
    fn from(tx_remove: TxRemove) -> Self {
        molecule_pcn::TxRemove::new_builder()
            .channel_id(tx_remove.channel_id.into())
            .tx(tx_remove.tx)
            .build()
    }
}

pub struct TxComplete {
    channel_id: Byte32,
}

impl From<TxComplete> for molecule_pcn::TxComplete {
    fn from(tx_complete: TxComplete) -> Self {
        molecule_pcn::TxComplete::new_builder()
            .channel_id(tx_complete.channel_id.into())
            .build()
    }
}

pub struct TxAbort {
    channel_id: Byte32,
    message: Vec<u8>,
}

impl From<TxAbort> for molecule_pcn::TxAbort {
    fn from(tx_abort: TxAbort) -> Self {
        molecule_pcn::TxAbort::new_builder()
            .channel_id(tx_abort.channel_id.into())
            .message(tx_abort.message.pack())
            .build()
    }
}

pub struct TxInitRBF {
    channel_id: Byte32,
    fee_rate: u64,
}

impl From<TxInitRBF> for molecule_pcn::TxInitRBF {
    fn from(tx_init_rbf: TxInitRBF) -> Self {
        molecule_pcn::TxInitRBF::new_builder()
            .channel_id(tx_init_rbf.channel_id.into())
            .fee_rate(tx_init_rbf.fee_rate.pack())
            .build()
    }
}

pub struct TxAckRBF {
    channel_id: Byte32,
}

impl From<TxAckRBF> for molecule_pcn::TxAckRBF {
    fn from(tx_ack_rbf: TxAckRBF) -> Self {
        molecule_pcn::TxAckRBF::new_builder()
            .channel_id(tx_ack_rbf.channel_id.into())
            .build()
    }
}

pub struct Shutdown {
    channel_id: Byte32,
    close_script: Script,
}

impl From<Shutdown> for molecule_pcn::Shutdown {
    fn from(shutdown: Shutdown) -> Self {
        molecule_pcn::Shutdown::new_builder()
            .channel_id(shutdown.channel_id.into())
            .close_script(shutdown.close_script.into())
            .build()
    }
}
pub struct ClosingSigned {
    channel_id: Byte32,
    fee: u64,
    signature: Signature,
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

pub struct AddTlc {
    channel_id: Byte32,
    tlc_id: u64,
    amount: u64,
    payment_hash: Byte32,
    expiry: u64,
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

pub struct TlcsSigned {
    channel_id: Byte32,
    signature: Signature,
    tlc_signatures: Vec<Signature>,
}

impl From<TlcsSigned> for molecule_pcn::TlcsSigned {
    fn from(tlcs_signed: TlcsSigned) -> Self {
        molecule_pcn::TlcsSigned::new_builder()
            .channel_id(tlcs_signed.channel_id.into())
            .signature(tlcs_signed.signature.into())
            .tlc_signatures(
                Byte64Vec::new_builder()
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

pub struct RevokeAndAck {
    channel_id: Byte32,
    per_commitment_secret: Byte32,
    next_per_commitment_point: Point,
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

pub struct RemoveTlcFulfill {
    payment_preimage: Byte32,
}

impl From<RemoveTlcFulfill> for molecule_pcn::RemoveTlcFulfill {
    fn from(remove_tlc_fulfill: RemoveTlcFulfill) -> Self {
        molecule_pcn::RemoveTlcFulfill::new_builder()
            .payment_preimage(remove_tlc_fulfill.payment_preimage.into())
            .build()
    }
}

pub struct RemoveTlcFail {
    error_code: u32,
}

impl From<RemoveTlcFail> for molecule_pcn::RemoveTlcFail {
    fn from(remove_tlc_fail: RemoveTlcFail) -> Self {
        molecule_pcn::RemoveTlcFail::new_builder()
            .error_code(remove_tlc_fail.error_code.pack())
            .build()
    }
}

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

pub struct RemoveTlc {
    channel_id: Byte32,
    tlc_id: u64,
    reason: RemoveTlcReason,
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

pub struct TestMessage {
    bytes: Vec<u8>,
}

impl From<TestMessage> for molecule_pcn::TestMessage {
    fn from(test_message: TestMessage) -> Self {
        molecule_pcn::TestMessage::new_builder()
            .bytes(test_message.bytes.pack())
            .build()
    }
}
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
