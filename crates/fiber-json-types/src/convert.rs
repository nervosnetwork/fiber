//! Conversion traits between fiber-json-types and fiber-types.
//!
//! This module provides `From` and `TryFrom` implementations for converting
//! between the JSON-RPC types in this crate and the internal domain types in
//! `fiber-types`. It is gated behind the `conversion` feature flag.
//!
//! ## Naming Convention
//!
//! - Infallible conversions use `From` trait impls
//! - Fallible conversions (pubkey validation, string parsing) use `TryFrom` with `Error = String`
//!
//! ## Usage
//!
//! ```ignore
//! use fiber_json_types::convert; // brings From/TryFrom impls into scope
//!
//! // Infallible: Hash256 -> JsonHash256
//! let json_hash: fiber_json_types::Hash256 = internal_hash.into();
//!
//! // Infallible: JsonHash256 -> Hash256
//! let internal_hash: fiber_types::Hash256 = json_hash.into();
//!
//! // Fallible: json::Pubkey -> fiber_types Pubkey
//! let pubkey: fiber_types::Pubkey = json_pubkey.try_into()?;
//! ```

use crate::channel::{
    ChannelState as JsonChannelState, InboundTlcStatus as JsonInboundTlcStatus,
    OutboundTlcStatus as JsonOutboundTlcStatus, TlcStatus as JsonTlcStatus,
};
use crate::graph::{
    ChannelUpdateInfo as JsonChannelUpdateInfo, UdtArgInfo as JsonUdtArgInfo,
    UdtCellDep as JsonUdtCellDep, UdtCfgInfos as JsonUdtCfgInfos, UdtDep as JsonUdtDep,
    UdtScript as JsonUdtScript,
};
use crate::invoice::{
    CkbInvoiceStatus as JsonCkbInvoiceStatus, Currency as JsonCurrency,
    HashAlgorithm as JsonHashAlgorithm,
};
use crate::payment::{
    PaymentStatus as JsonPaymentStatus, SessionRoute as JsonSessionRoute,
    SessionRouteNode as JsonSessionRouteNode,
};
use crate::serde_utils::{Hash256 as JsonHash256, Privkey as JsonPrivkey, Pubkey as JsonPubkey};

use ckb_types::prelude::Entity;
use fiber_types::{
    ChannelState as InternalChannelState, CloseFlags, Hash256,
    InboundTlcStatus as InternalInboundTlcStatus, OutboundTlcStatus as InternalOutboundTlcStatus,
    Pubkey, TlcStatus as InternalTlcStatus,
};

// ─── Primitive Converters ───────────────────────────────────────────────────

// Hash256 <-> JsonHash256

impl From<Hash256> for JsonHash256 {
    fn from(hash: Hash256) -> Self {
        JsonHash256(
            hash.as_ref()
                .try_into()
                .expect("Hash256 is always 32 bytes"),
        )
    }
}

impl From<JsonHash256> for Hash256 {
    fn from(jh: JsonHash256) -> Self {
        Hash256::from(jh.0)
    }
}

// Pubkey <-> JsonPubkey

impl From<Pubkey> for JsonPubkey {
    fn from(pubkey: Pubkey) -> Self {
        JsonPubkey(pubkey.serialize())
    }
}

impl TryFrom<JsonPubkey> for Pubkey {
    type Error = String;

    fn try_from(jp: JsonPubkey) -> Result<Self, Self::Error> {
        Pubkey::from_slice(&jp.0).map_err(|e| format!("invalid pubkey '{}': {}", jp, e))
    }
}

impl From<fiber_types::Privkey> for JsonPrivkey {
    fn from(privkey: fiber_types::Privkey) -> Self {
        JsonPrivkey::from(privkey.0.secret_bytes())
    }
}

impl TryFrom<JsonPrivkey> for fiber_types::Privkey {
    type Error = String;

    fn try_from(jp: JsonPrivkey) -> Result<Self, Self::Error> {
        Ok(fiber_types::Privkey::from_slice(jp.as_bytes()))
    }
}

// ─── ChannelState Conversions ───────────────────────────────────────────────

impl From<InternalChannelState> for JsonChannelState {
    fn from(state: InternalChannelState) -> Self {
        match state {
            InternalChannelState::NegotiatingFunding(flags) => {
                JsonChannelState::NegotiatingFunding(flags.bits().into())
            }
            InternalChannelState::CollaboratingFundingTx(flags) => {
                JsonChannelState::CollaboratingFundingTx(flags.bits().into())
            }
            InternalChannelState::SigningCommitment(flags) => {
                JsonChannelState::SigningCommitment(flags.bits().into())
            }
            InternalChannelState::AwaitingTxSignatures(flags) => {
                JsonChannelState::AwaitingTxSignatures(flags.bits().into())
            }
            InternalChannelState::AwaitingChannelReady(flags) => {
                JsonChannelState::AwaitingChannelReady(flags.bits().into())
            }
            InternalChannelState::ChannelReady => JsonChannelState::ChannelReady,
            InternalChannelState::ShuttingDown(flags) => {
                JsonChannelState::ShuttingDown(flags.bits().into())
            }
            InternalChannelState::Closed(flags) => JsonChannelState::Closed(flags.bits().into()),
        }
    }
}

impl JsonChannelState {
    /// Check if a JSON ChannelState represents a "pending" channel (still being opened or failed).
    pub fn is_pending(&self) -> bool {
        match self {
            JsonChannelState::NegotiatingFunding(_)
            | JsonChannelState::CollaboratingFundingTx(_)
            | JsonChannelState::SigningCommitment(_)
            | JsonChannelState::AwaitingTxSignatures(_)
            | JsonChannelState::AwaitingChannelReady(_) => true,
            JsonChannelState::ChannelReady | JsonChannelState::ShuttingDown(_) => false,
            JsonChannelState::Closed(bits) => {
                // FUNDING_ABORTED or ABANDONED are "pending-failed" states
                let flags = CloseFlags::from_bits_truncate(bits.0);
                flags.intersects(CloseFlags::FUNDING_ABORTED | CloseFlags::ABANDONED)
            }
        }
    }
}

// ─── TlcStatus Conversions ──────────────────────────────────────────────────

impl From<InternalTlcStatus> for JsonTlcStatus {
    fn from(status: InternalTlcStatus) -> Self {
        match status {
            InternalTlcStatus::Outbound(s) => JsonTlcStatus::Outbound(s.into()),
            InternalTlcStatus::Inbound(s) => JsonTlcStatus::Inbound(s.into()),
        }
    }
}

impl From<InternalOutboundTlcStatus> for JsonOutboundTlcStatus {
    fn from(status: InternalOutboundTlcStatus) -> Self {
        match status {
            InternalOutboundTlcStatus::LocalAnnounced => JsonOutboundTlcStatus::LocalAnnounced,
            InternalOutboundTlcStatus::Committed => JsonOutboundTlcStatus::Committed,
            InternalOutboundTlcStatus::RemoteRemoved => JsonOutboundTlcStatus::RemoteRemoved,
            InternalOutboundTlcStatus::RemoveWaitPrevAck => {
                JsonOutboundTlcStatus::RemoveWaitPrevAck
            }
            InternalOutboundTlcStatus::RemoveWaitAck => JsonOutboundTlcStatus::RemoveWaitAck,
            InternalOutboundTlcStatus::RemoveAckConfirmed => {
                JsonOutboundTlcStatus::RemoveAckConfirmed
            }
        }
    }
}

impl From<InternalInboundTlcStatus> for JsonInboundTlcStatus {
    fn from(status: InternalInboundTlcStatus) -> Self {
        match status {
            InternalInboundTlcStatus::RemoteAnnounced => JsonInboundTlcStatus::RemoteAnnounced,
            InternalInboundTlcStatus::AnnounceWaitPrevAck => {
                JsonInboundTlcStatus::AnnounceWaitPrevAck
            }
            InternalInboundTlcStatus::AnnounceWaitAck => JsonInboundTlcStatus::AnnounceWaitAck,
            InternalInboundTlcStatus::Committed => JsonInboundTlcStatus::Committed,
            InternalInboundTlcStatus::LocalRemoved => JsonInboundTlcStatus::LocalRemoved,
            InternalInboundTlcStatus::RemoveAckConfirmed => {
                JsonInboundTlcStatus::RemoveAckConfirmed
            }
        }
    }
}

// ─── PaymentStatus Conversions ──────────────────────────────────────────────

impl From<fiber_types::PaymentStatus> for JsonPaymentStatus {
    fn from(status: fiber_types::PaymentStatus) -> Self {
        match status {
            fiber_types::PaymentStatus::Created => JsonPaymentStatus::Created,
            fiber_types::PaymentStatus::Inflight => JsonPaymentStatus::Inflight,
            fiber_types::PaymentStatus::Success => JsonPaymentStatus::Success,
            fiber_types::PaymentStatus::Failed => JsonPaymentStatus::Failed,
        }
    }
}

impl From<JsonPaymentStatus> for fiber_types::PaymentStatus {
    fn from(status: JsonPaymentStatus) -> Self {
        match status {
            JsonPaymentStatus::Created => fiber_types::PaymentStatus::Created,
            JsonPaymentStatus::Inflight => fiber_types::PaymentStatus::Inflight,
            JsonPaymentStatus::Success => fiber_types::PaymentStatus::Success,
            JsonPaymentStatus::Failed => fiber_types::PaymentStatus::Failed,
        }
    }
}

// ─── Currency Conversions ───────────────────────────────────────────────────

impl From<fiber_types::Currency> for JsonCurrency {
    fn from(currency: fiber_types::Currency) -> Self {
        match currency {
            fiber_types::Currency::Fibb => JsonCurrency::Fibb,
            fiber_types::Currency::Fibt => JsonCurrency::Fibt,
            fiber_types::Currency::Fibd => JsonCurrency::Fibd,
        }
    }
}

impl From<JsonCurrency> for fiber_types::Currency {
    fn from(currency: JsonCurrency) -> Self {
        match currency {
            JsonCurrency::Fibb => fiber_types::Currency::Fibb,
            JsonCurrency::Fibt => fiber_types::Currency::Fibt,
            JsonCurrency::Fibd => fiber_types::Currency::Fibd,
        }
    }
}

// ─── HashAlgorithm Conversions ──────────────────────────────────────────────

impl From<fiber_types::HashAlgorithm> for JsonHashAlgorithm {
    fn from(algo: fiber_types::HashAlgorithm) -> Self {
        match algo {
            fiber_types::HashAlgorithm::CkbHash => JsonHashAlgorithm::CkbHash,
            fiber_types::HashAlgorithm::Sha256 => JsonHashAlgorithm::Sha256,
        }
    }
}

impl From<JsonHashAlgorithm> for fiber_types::HashAlgorithm {
    fn from(algo: JsonHashAlgorithm) -> Self {
        match algo {
            JsonHashAlgorithm::CkbHash => fiber_types::HashAlgorithm::CkbHash,
            JsonHashAlgorithm::Sha256 => fiber_types::HashAlgorithm::Sha256,
        }
    }
}

// ─── CkbInvoiceStatus Conversions ───────────────────────────────────────────

impl From<fiber_types::CkbInvoiceStatus> for JsonCkbInvoiceStatus {
    fn from(status: fiber_types::CkbInvoiceStatus) -> Self {
        match status {
            fiber_types::CkbInvoiceStatus::Open => JsonCkbInvoiceStatus::Open,
            fiber_types::CkbInvoiceStatus::Cancelled => JsonCkbInvoiceStatus::Cancelled,
            fiber_types::CkbInvoiceStatus::Expired => JsonCkbInvoiceStatus::Expired,
            fiber_types::CkbInvoiceStatus::Received => JsonCkbInvoiceStatus::Received,
            fiber_types::CkbInvoiceStatus::Paid => JsonCkbInvoiceStatus::Paid,
        }
    }
}

// ─── CchOrderStatus Conversions ─────────────────────────────────────────────

#[cfg(feature = "cch")]
mod cch_conversions {
    use crate::cch::{CchInvoice as JsonCchInvoice, CchOrderStatus as JsonCchOrderStatus};

    impl From<fiber_types::CchOrderStatus> for JsonCchOrderStatus {
        fn from(status: fiber_types::CchOrderStatus) -> Self {
            match status {
                fiber_types::CchOrderStatus::Pending => JsonCchOrderStatus::Pending,
                fiber_types::CchOrderStatus::IncomingAccepted => {
                    JsonCchOrderStatus::IncomingAccepted
                }
                fiber_types::CchOrderStatus::OutgoingInFlight => {
                    JsonCchOrderStatus::OutgoingInFlight
                }
                fiber_types::CchOrderStatus::OutgoingSucceeded => {
                    JsonCchOrderStatus::OutgoingSucceeded
                }
                fiber_types::CchOrderStatus::Success => JsonCchOrderStatus::Success,
                fiber_types::CchOrderStatus::Failed => JsonCchOrderStatus::Failed,
            }
        }
    }

    // ─── CchOrder → CchOrderResponse Conversion ────────────────────────────

    impl From<fiber_types::CchOrder> for crate::cch::CchOrderResponse {
        fn from(order: fiber_types::CchOrder) -> Self {
            crate::cch::CchOrderResponse {
                timestamp: order.created_at,
                expiry_delta_seconds: order.expiry_delta_seconds,
                wrapped_btc_type_script: order.wrapped_btc_type_script,
                incoming_invoice: JsonCchInvoice::from(order.incoming_invoice),
                outgoing_pay_req: order.outgoing_pay_req,
                payment_hash: order.payment_hash.into(),
                amount_sats: order.amount_sats,
                fee_sats: order.fee_sats,
                status: order.status.into(),
            }
        }
    }

    impl From<fiber_types::CchInvoice> for JsonCchInvoice {
        fn from(invoice: fiber_types::CchInvoice) -> Self {
            match invoice {
                fiber_types::CchInvoice::Fiber(inv) => JsonCchInvoice::Fiber(inv.to_string()),
                fiber_types::CchInvoice::Lightning(inv) => {
                    JsonCchInvoice::Lightning(inv.to_string())
                }
            }
        }
    }
}

impl From<fiber_types::UdtCfgInfos> for JsonUdtCfgInfos {
    fn from(infos: fiber_types::UdtCfgInfos) -> Self {
        JsonUdtCfgInfos(infos.0.into_iter().map(JsonUdtArgInfo::from).collect())
    }
}

impl From<fiber_types::UdtArgInfo> for JsonUdtArgInfo {
    fn from(info: fiber_types::UdtArgInfo) -> Self {
        JsonUdtArgInfo {
            name: info.name,
            script: JsonUdtScript::from(info.script),
            auto_accept_amount: info.auto_accept_amount,
            cell_deps: info.cell_deps.into_iter().map(JsonUdtDep::from).collect(),
        }
    }
}

impl From<fiber_types::UdtScript> for JsonUdtScript {
    fn from(script: fiber_types::UdtScript) -> Self {
        JsonUdtScript {
            code_hash: script.code_hash,
            hash_type: script.hash_type.into(),
            args: script.args,
        }
    }
}

impl From<fiber_types::UdtDep> for JsonUdtDep {
    fn from(dep: fiber_types::UdtDep) -> Self {
        JsonUdtDep {
            cell_dep: dep.cell_dep.map(JsonUdtCellDep::from),
            type_id: dep.type_id,
        }
    }
}

impl From<fiber_types::UdtCellDep> for JsonUdtCellDep {
    fn from(cell_dep: fiber_types::UdtCellDep) -> Self {
        JsonUdtCellDep {
            out_point: cell_dep.out_point,
            dep_type: cell_dep.dep_type.into(),
        }
    }
}

// ─── ChannelUpdateInfo Conversions ──────────────────────────────────────────

impl From<fiber_types::ChannelUpdateInfo> for JsonChannelUpdateInfo {
    fn from(info: fiber_types::ChannelUpdateInfo) -> Self {
        JsonChannelUpdateInfo {
            timestamp: info.timestamp,
            enabled: info.enabled,
            outbound_liquidity: info.outbound_liquidity,
            tlc_expiry_delta: info.tlc_expiry_delta,
            tlc_minimum_value: info.tlc_minimum_value,
            fee_rate: info.fee_rate,
        }
    }
}

// ─── SessionRoute Conversions ───────────────────────────────────────────────

impl From<fiber_types::SessionRoute> for JsonSessionRoute {
    fn from(route: fiber_types::SessionRoute) -> Self {
        JsonSessionRoute {
            nodes: route
                .nodes
                .into_iter()
                .map(|node| JsonSessionRouteNode {
                    pubkey: node.pubkey.into(),
                    amount: node.amount,
                    channel_outpoint: node.channel_outpoint,
                })
                .collect(),
        }
    }
}

// ─── RouterHop Conversions ──────────────────────────────────────────────────

impl From<fiber_types::RouterHop> for crate::payment::RouterHop {
    fn from(hop: fiber_types::RouterHop) -> Self {
        crate::payment::RouterHop {
            target: hop.target.into(),
            channel_outpoint: hop.channel_outpoint,
            amount_received: hop.amount_received,
            incoming_tlc_expiry: hop.incoming_tlc_expiry,
        }
    }
}

impl TryFrom<crate::payment::RouterHop> for fiber_types::RouterHop {
    type Error = String;

    fn try_from(hop: crate::payment::RouterHop) -> Result<Self, Self::Error> {
        Ok(fiber_types::RouterHop {
            target: Pubkey::try_from(hop.target)?,
            channel_outpoint: hop.channel_outpoint,
            amount_received: hop.amount_received,
            incoming_tlc_expiry: hop.incoming_tlc_expiry,
        })
    }
}

// ─── HopHint Conversions ────────────────────────────────────────────────────

impl TryFrom<crate::payment::HopHint> for fiber_types::HopHint {
    type Error = String;

    fn try_from(hint: crate::payment::HopHint) -> Result<Self, Self::Error> {
        Ok(fiber_types::HopHint {
            pubkey: Pubkey::try_from(hint.pubkey)?,
            channel_outpoint: hint.channel_outpoint,
            fee_rate: hint.fee_rate,
            tlc_expiry_delta: hint.tlc_expiry_delta,
        })
    }
}

// ─── HopRequire Conversions ─────────────────────────────────────────────────

impl TryFrom<crate::payment::HopRequire> for fiber_types::HopRequire {
    type Error = String;

    fn try_from(hop: crate::payment::HopRequire) -> Result<Self, Self::Error> {
        Ok(fiber_types::HopRequire {
            pubkey: Pubkey::try_from(hop.pubkey)?,
            channel_outpoint: hop.channel_outpoint,
        })
    }
}

// ─── PaymentCustomRecords Conversions ───────────────────────────────────────

impl From<crate::payment::PaymentCustomRecords> for fiber_types::PaymentCustomRecords {
    fn from(records: crate::payment::PaymentCustomRecords) -> Self {
        fiber_types::PaymentCustomRecords { data: records.data }
    }
}

impl From<fiber_types::PaymentCustomRecords> for crate::payment::PaymentCustomRecords {
    fn from(records: fiber_types::PaymentCustomRecords) -> Self {
        crate::payment::PaymentCustomRecords { data: records.data }
    }
}

// ─── CkbInvoice Conversions ─────────────────────────────────────────────────

impl From<fiber_types::CkbInvoice> for crate::invoice::CkbInvoice {
    fn from(invoice: fiber_types::CkbInvoice) -> Self {
        crate::invoice::CkbInvoice {
            currency: invoice.currency.into(),
            amount: invoice.amount,
            signature: invoice.signature.as_ref().map(|sig| {
                // InvoiceSignature's Serialize impl produces a hex string
                serde_json::to_value(sig)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default()
            }),
            data: crate::invoice::InvoiceData::from(invoice.data),
        }
    }
}

impl From<fiber_types::InvoiceData> for crate::invoice::InvoiceData {
    fn from(data: fiber_types::InvoiceData) -> Self {
        crate::invoice::InvoiceData {
            timestamp: data.timestamp,
            payment_hash: data.payment_hash.into(),
            attrs: data
                .attrs
                .into_iter()
                .map(crate::invoice::Attribute::from)
                .collect(),
        }
    }
}

impl From<fiber_types::Attribute> for crate::invoice::Attribute {
    fn from(attr: fiber_types::Attribute) -> Self {
        use crate::invoice::Attribute as JsonAttr;
        use fiber_types::Attribute as InternalAttr;

        match attr {
            InternalAttr::FinalHtlcTimeout(v) => JsonAttr::FinalHtlcTimeout(v),
            InternalAttr::FinalHtlcMinimumExpiryDelta(v) => {
                JsonAttr::FinalHtlcMinimumExpiryDelta(v)
            }
            InternalAttr::ExpiryTime(d) => JsonAttr::ExpiryTime(d),
            InternalAttr::Description(s) => JsonAttr::Description(s),
            InternalAttr::FallbackAddr(s) => JsonAttr::FallbackAddr(s),
            InternalAttr::UdtScript(script) => {
                // CkbScript wraps PackedScript (molecule type); use as_slice() for bytes
                let bytes = script.0.as_slice();
                JsonAttr::UdtScript(format!("0x{}", hex::encode(bytes)))
            }
            InternalAttr::PayeePublicKey(pk) => {
                JsonAttr::PayeePublicKey(JsonPubkey(pk.serialize()))
            }
            InternalAttr::HashAlgorithm(algo) => JsonAttr::HashAlgorithm(algo.into()),
            InternalAttr::Feature(features) => JsonAttr::Feature(features.enabled_features_names()),
            InternalAttr::PaymentSecret(secret) => {
                JsonAttr::PaymentSecret(JsonHash256::from(secret).to_string())
            }
        }
    }
}

// ─── Watchtower Conversions ─────────────────────────────────────────────────

#[cfg(feature = "watchtower")]
mod watchtower_convert {
    use super::*;
    use crate::watchtower::{
        RevocationData as JsonRevocationData, SettlementData as JsonSettlementData,
        SettlementTlc as JsonSettlementTlc, TLCId as JsonTLCId,
    };
    use fiber_types::channel::TLCId as InternalTLCId;
    use fiber_types::watchtower::{
        RevocationData as InternalRevocationData, SettlementData as InternalSettlementData,
        SettlementTlc as InternalSettlementTlc,
    };

    // TLCId conversions

    impl From<InternalTLCId> for JsonTLCId {
        fn from(id: InternalTLCId) -> Self {
            match id {
                InternalTLCId::Offered(v) => JsonTLCId::Offered(v),
                InternalTLCId::Received(v) => JsonTLCId::Received(v),
            }
        }
    }

    impl From<JsonTLCId> for InternalTLCId {
        fn from(id: JsonTLCId) -> Self {
            match id {
                JsonTLCId::Offered(v) => InternalTLCId::Offered(v),
                JsonTLCId::Received(v) => InternalTLCId::Received(v),
            }
        }
    }

    // SettlementTlc conversions

    impl From<InternalSettlementTlc> for JsonSettlementTlc {
        fn from(tlc: InternalSettlementTlc) -> Self {
            JsonSettlementTlc {
                tlc_id: tlc.tlc_id.into(),
                hash_algorithm: tlc.hash_algorithm.into(),
                payment_amount: tlc.payment_amount,
                payment_hash: tlc.payment_hash.into(),
                expiry: tlc.expiry,
                local_key: tlc.local_key.into(),
                remote_key: tlc.remote_key.into(),
            }
        }
    }

    impl TryFrom<JsonSettlementTlc> for InternalSettlementTlc {
        type Error = String;
        fn try_from(tlc: JsonSettlementTlc) -> Result<Self, Self::Error> {
            Ok(InternalSettlementTlc {
                tlc_id: tlc.tlc_id.into(),
                hash_algorithm: tlc.hash_algorithm.into(),
                payment_amount: tlc.payment_amount,
                payment_hash: tlc.payment_hash.into(),
                expiry: tlc.expiry,
                local_key: tlc.local_key.try_into()?,
                remote_key: Pubkey::try_from(tlc.remote_key)
                    .map_err(|e| format!("invalid remote_key: {e}"))?,
            })
        }
    }

    // SettlementData conversions

    impl From<InternalSettlementData> for JsonSettlementData {
        fn from(data: InternalSettlementData) -> Self {
            JsonSettlementData {
                local_amount: data.local_amount,
                remote_amount: data.remote_amount,
                tlcs: data.tlcs.into_iter().map(JsonSettlementTlc::from).collect(),
            }
        }
    }

    impl TryFrom<JsonSettlementData> for InternalSettlementData {
        type Error = String;
        fn try_from(data: JsonSettlementData) -> Result<Self, Self::Error> {
            let tlcs = data
                .tlcs
                .into_iter()
                .map(InternalSettlementTlc::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(InternalSettlementData {
                local_amount: data.local_amount,
                remote_amount: data.remote_amount,
                tlcs,
            })
        }
    }

    // RevocationData conversions

    impl From<InternalRevocationData> for JsonRevocationData {
        fn from(data: InternalRevocationData) -> Self {
            JsonRevocationData {
                commitment_number: data.commitment_number,
                aggregated_signature: data.aggregated_signature.serialize().to_vec(),
                output: data.output,
                output_data: data.output_data,
            }
        }
    }

    impl TryFrom<JsonRevocationData> for InternalRevocationData {
        type Error = String;
        fn try_from(data: JsonRevocationData) -> Result<Self, Self::Error> {
            let sig = musig2::CompactSignature::from_bytes(&data.aggregated_signature)
                .map_err(|e| format!("invalid aggregated_signature: {e}"))?;
            Ok(InternalRevocationData {
                commitment_number: data.commitment_number,
                aggregated_signature: sig,
                output: data.output,
                output_data: data.output_data,
            })
        }
    }
}
