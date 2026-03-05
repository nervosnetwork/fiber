//! Conversion helpers between fiber-json-types (String-based) and fiber-types (domain types).
//!
//! All type conversions at the RPC boundary live here, keeping the JSON types crate
//! free of internal domain dependencies.

use ckb_types::packed::Byte32;
use ckb_types::prelude::{Entity, Unpack};
use fiber_types::{
    AwaitingChannelReadyFlags, AwaitingTxSignaturesFlags, ShuttingDownFlags, SigningCommitmentFlags,
};
use fiber_types::{
    ChannelState as InternalChannelState, CloseFlags, CollaboratingFundingTxFlags, Hash256,
    InboundTlcStatus as InternalInboundTlcStatus, Multiaddr, NegotiatingFundingFlags, NodeId,
    OutboundTlcStatus as InternalOutboundTlcStatus, Pubkey, TlcStatus as InternalTlcStatus,
};
use std::str::FromStr;

use fiber_json_types::cch::{CchInvoice as JsonCchInvoice, CchOrderStatus as JsonCchOrderStatus};
use fiber_json_types::channel::{
    ChannelState as JsonChannelState, InboundTlcStatus as JsonInboundTlcStatus,
    OutboundTlcStatus as JsonOutboundTlcStatus, TlcStatus as JsonTlcStatus,
};
use fiber_json_types::graph::{
    ChannelUpdateInfo as JsonChannelUpdateInfo, UdtArgInfo as JsonUdtArgInfo,
    UdtCellDep as JsonUdtCellDep, UdtCfgInfos as JsonUdtCfgInfos, UdtDep as JsonUdtDep,
    UdtScript as JsonUdtScript,
};
use fiber_json_types::invoice::{
    CkbInvoiceStatus as JsonCkbInvoiceStatus, Currency as JsonCurrency,
    HashAlgorithm as JsonHashAlgorithm,
};
use fiber_json_types::payment::{
    PaymentStatus as JsonPaymentStatus, SessionRoute as JsonSessionRoute,
    SessionRouteNode as JsonSessionRouteNode,
};
use fiber_json_types::serde_utils::{Hash256 as JsonHash256, Pubkey as JsonPubkey};

// ─── Primitive Converters ───────────────────────────────────────────────────

/// Convert a Hash256 to a JsonHash256.
pub fn hash256_to_json_hash256(hash: &Hash256) -> JsonHash256 {
    JsonHash256(
        hash.as_ref()
            .try_into()
            .expect("Hash256 is always 32 bytes"),
    )
}

/// Convert a JsonHash256 to an internal Hash256.
pub fn json_hash256_to_hash256(jh: &JsonHash256) -> Hash256 {
    Hash256::from(jh.0)
}

/// Convert a Pubkey to a JsonPubkey.
pub fn pubkey_to_json_pubkey(pubkey: &Pubkey) -> JsonPubkey {
    JsonPubkey(pubkey.serialize())
}

/// Convert a JsonPubkey to an internal Pubkey (validates as secp256k1 point).
pub fn json_pubkey_to_pubkey(jp: &JsonPubkey) -> Result<Pubkey, String> {
    Pubkey::from_slice(&jp.0).map_err(|e| format!("invalid pubkey '{}': {}", jp, e))
}

/// Convert a Multiaddr to its string representation.
pub fn multiaddr_to_string(addr: &Multiaddr) -> String {
    addr.to_string()
}

/// Parse a string into a Multiaddr.
pub fn string_to_multiaddr(s: &str) -> Result<Multiaddr, String> {
    s.parse::<Multiaddr>()
        .map_err(|e| format!("invalid multiaddr '{}': {}", s, e))
}

/// Convert a NodeId to its base58 string representation.
pub fn node_id_to_string(node_id: &NodeId) -> String {
    bs58::encode(node_id.as_ref()).into_string()
}

/// Parse a base58 string into a NodeId.
pub fn string_to_node_id(s: &str) -> Result<NodeId, String> {
    NodeId::from_str(s).map_err(|e| format!("invalid node_id '{}': {}", s, e))
}

/// Convert a Byte32 to its 0x-prefixed hex string.
pub fn byte32_to_string(b: Byte32) -> String {
    let hash: ckb_types::H256 = b.unpack();
    format!("{:#x}", hash)
}

// ─── ChannelState Conversions ───────────────────────────────────────────────

/// Convert internal ChannelState to JSON ChannelState.
pub fn channel_state_to_json(state: InternalChannelState) -> JsonChannelState {
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

/// Convert JSON ChannelState to internal ChannelState.
#[allow(dead_code)]
pub fn json_to_channel_state(state: JsonChannelState) -> InternalChannelState {
    match state {
        JsonChannelState::NegotiatingFunding(bits) => InternalChannelState::NegotiatingFunding(
            NegotiatingFundingFlags::from_bits_truncate(bits.0),
        ),
        JsonChannelState::CollaboratingFundingTx(bits) => {
            InternalChannelState::CollaboratingFundingTx(
                CollaboratingFundingTxFlags::from_bits_truncate(bits.0),
            )
        }
        JsonChannelState::SigningCommitment(bits) => InternalChannelState::SigningCommitment(
            SigningCommitmentFlags::from_bits_truncate(bits.0),
        ),
        JsonChannelState::AwaitingTxSignatures(bits) => InternalChannelState::AwaitingTxSignatures(
            AwaitingTxSignaturesFlags::from_bits_truncate(bits.0),
        ),
        JsonChannelState::AwaitingChannelReady(bits) => InternalChannelState::AwaitingChannelReady(
            AwaitingChannelReadyFlags::from_bits_truncate(bits.0),
        ),
        JsonChannelState::ChannelReady => InternalChannelState::ChannelReady,
        JsonChannelState::ShuttingDown(bits) => {
            InternalChannelState::ShuttingDown(ShuttingDownFlags::from_bits_truncate(bits.0))
        }
        JsonChannelState::Closed(bits) => {
            InternalChannelState::Closed(CloseFlags::from_bits_truncate(bits.0))
        }
    }
}

/// Check if a JSON ChannelState represents a "pending" channel (still being opened or failed).
/// Replaces the old `ChannelState::is_pending()` method that was on the JSON type.
pub fn is_channel_state_pending(state: &JsonChannelState) -> bool {
    match state {
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

// ─── TlcStatus Conversions ──────────────────────────────────────────────────

/// Convert internal TlcStatus to JSON TlcStatus.
pub fn tlc_status_to_json(status: &InternalTlcStatus) -> JsonTlcStatus {
    match status {
        InternalTlcStatus::Outbound(s) => JsonTlcStatus::Outbound(outbound_tlc_status_to_json(s)),
        InternalTlcStatus::Inbound(s) => JsonTlcStatus::Inbound(inbound_tlc_status_to_json(s)),
    }
}

fn outbound_tlc_status_to_json(status: &InternalOutboundTlcStatus) -> JsonOutboundTlcStatus {
    match status {
        InternalOutboundTlcStatus::LocalAnnounced => JsonOutboundTlcStatus::LocalAnnounced,
        InternalOutboundTlcStatus::Committed => JsonOutboundTlcStatus::Committed,
        InternalOutboundTlcStatus::RemoteRemoved => JsonOutboundTlcStatus::RemoteRemoved,
        InternalOutboundTlcStatus::RemoveWaitPrevAck => JsonOutboundTlcStatus::RemoveWaitPrevAck,
        InternalOutboundTlcStatus::RemoveWaitAck => JsonOutboundTlcStatus::RemoveWaitAck,
        InternalOutboundTlcStatus::RemoveAckConfirmed => JsonOutboundTlcStatus::RemoveAckConfirmed,
    }
}

fn inbound_tlc_status_to_json(status: &InternalInboundTlcStatus) -> JsonInboundTlcStatus {
    match status {
        InternalInboundTlcStatus::RemoteAnnounced => JsonInboundTlcStatus::RemoteAnnounced,
        InternalInboundTlcStatus::AnnounceWaitPrevAck => JsonInboundTlcStatus::AnnounceWaitPrevAck,
        InternalInboundTlcStatus::AnnounceWaitAck => JsonInboundTlcStatus::AnnounceWaitAck,
        InternalInboundTlcStatus::Committed => JsonInboundTlcStatus::Committed,
        InternalInboundTlcStatus::LocalRemoved => JsonInboundTlcStatus::LocalRemoved,
        InternalInboundTlcStatus::RemoveAckConfirmed => JsonInboundTlcStatus::RemoveAckConfirmed,
    }
}

// ─── PaymentStatus Conversions ──────────────────────────────────────────────

/// Convert internal PaymentStatus to JSON PaymentStatus.
pub fn payment_status_to_json(status: &fiber_types::PaymentStatus) -> JsonPaymentStatus {
    match status {
        fiber_types::PaymentStatus::Created => JsonPaymentStatus::Created,
        fiber_types::PaymentStatus::Inflight => JsonPaymentStatus::Inflight,
        fiber_types::PaymentStatus::Success => JsonPaymentStatus::Success,
        fiber_types::PaymentStatus::Failed => JsonPaymentStatus::Failed,
    }
}

/// Convert JSON PaymentStatus to internal PaymentStatus.
pub fn json_to_payment_status(status: &JsonPaymentStatus) -> fiber_types::PaymentStatus {
    match status {
        JsonPaymentStatus::Created => fiber_types::PaymentStatus::Created,
        JsonPaymentStatus::Inflight => fiber_types::PaymentStatus::Inflight,
        JsonPaymentStatus::Success => fiber_types::PaymentStatus::Success,
        JsonPaymentStatus::Failed => fiber_types::PaymentStatus::Failed,
    }
}

// ─── Currency Conversions ───────────────────────────────────────────────────

/// Convert internal Currency to JSON Currency.
pub fn currency_to_json(currency: &fiber_types::Currency) -> JsonCurrency {
    match currency {
        fiber_types::Currency::Fibb => JsonCurrency::Fibb,
        fiber_types::Currency::Fibt => JsonCurrency::Fibt,
        fiber_types::Currency::Fibd => JsonCurrency::Fibd,
    }
}

/// Convert JSON Currency to internal Currency.
pub fn json_to_currency(currency: &JsonCurrency) -> fiber_types::Currency {
    match currency {
        JsonCurrency::Fibb => fiber_types::Currency::Fibb,
        JsonCurrency::Fibt => fiber_types::Currency::Fibt,
        JsonCurrency::Fibd => fiber_types::Currency::Fibd,
    }
}

// ─── HashAlgorithm Conversions ──────────────────────────────────────────────

/// Convert internal HashAlgorithm to JSON HashAlgorithm.
#[allow(dead_code)]
pub fn hash_algorithm_to_json(algo: &fiber_types::HashAlgorithm) -> JsonHashAlgorithm {
    match algo {
        fiber_types::HashAlgorithm::CkbHash => JsonHashAlgorithm::CkbHash,
        fiber_types::HashAlgorithm::Sha256 => JsonHashAlgorithm::Sha256,
    }
}

/// Convert JSON HashAlgorithm to internal HashAlgorithm.
pub fn json_to_hash_algorithm(algo: &JsonHashAlgorithm) -> fiber_types::HashAlgorithm {
    match algo {
        JsonHashAlgorithm::CkbHash => fiber_types::HashAlgorithm::CkbHash,
        JsonHashAlgorithm::Sha256 => fiber_types::HashAlgorithm::Sha256,
    }
}

// ─── CkbInvoiceStatus Conversions ───────────────────────────────────────────

/// Convert internal CkbInvoiceStatus to JSON CkbInvoiceStatus.
pub fn invoice_status_to_json(status: &fiber_types::CkbInvoiceStatus) -> JsonCkbInvoiceStatus {
    match status {
        fiber_types::CkbInvoiceStatus::Open => JsonCkbInvoiceStatus::Open,
        fiber_types::CkbInvoiceStatus::Cancelled => JsonCkbInvoiceStatus::Cancelled,
        fiber_types::CkbInvoiceStatus::Expired => JsonCkbInvoiceStatus::Expired,
        fiber_types::CkbInvoiceStatus::Received => JsonCkbInvoiceStatus::Received,
        fiber_types::CkbInvoiceStatus::Paid => JsonCkbInvoiceStatus::Paid,
    }
}

/// Convert JSON CkbInvoiceStatus to internal CkbInvoiceStatus.
#[allow(dead_code)]
pub fn json_to_invoice_status(status: &JsonCkbInvoiceStatus) -> fiber_types::CkbInvoiceStatus {
    match status {
        JsonCkbInvoiceStatus::Open => fiber_types::CkbInvoiceStatus::Open,
        JsonCkbInvoiceStatus::Cancelled => fiber_types::CkbInvoiceStatus::Cancelled,
        JsonCkbInvoiceStatus::Expired => fiber_types::CkbInvoiceStatus::Expired,
        JsonCkbInvoiceStatus::Received => fiber_types::CkbInvoiceStatus::Received,
        JsonCkbInvoiceStatus::Paid => fiber_types::CkbInvoiceStatus::Paid,
    }
}

// ─── CchOrderStatus Conversions ─────────────────────────────────────────────

/// Convert internal CchOrderStatus to JSON CchOrderStatus.
pub fn cch_order_status_to_json(status: &fiber_types::CchOrderStatus) -> JsonCchOrderStatus {
    match status {
        fiber_types::CchOrderStatus::Pending => JsonCchOrderStatus::Pending,
        fiber_types::CchOrderStatus::IncomingAccepted => JsonCchOrderStatus::IncomingAccepted,
        fiber_types::CchOrderStatus::OutgoingInFlight => JsonCchOrderStatus::OutgoingInFlight,
        fiber_types::CchOrderStatus::OutgoingSucceeded => JsonCchOrderStatus::OutgoingSucceeded,
        fiber_types::CchOrderStatus::Succeeded => JsonCchOrderStatus::Succeeded,
        fiber_types::CchOrderStatus::Failed => JsonCchOrderStatus::Failed,
    }
}

// ─── UdtCfgInfos Conversions ────────────────────────────────────────────────

/// Convert internal UdtCfgInfos to JSON UdtCfgInfos.
pub fn udt_cfg_infos_to_json(infos: &fiber_types::UdtCfgInfos) -> JsonUdtCfgInfos {
    JsonUdtCfgInfos(infos.0.iter().map(udt_arg_info_to_json).collect())
}

fn udt_arg_info_to_json(info: &fiber_types::UdtArgInfo) -> JsonUdtArgInfo {
    JsonUdtArgInfo {
        name: info.name.clone(),
        script: udt_script_to_json(&info.script),
        auto_accept_amount: info.auto_accept_amount,
        cell_deps: info.cell_deps.iter().map(udt_dep_to_json).collect(),
    }
}

fn udt_script_to_json(script: &fiber_types::UdtScript) -> JsonUdtScript {
    JsonUdtScript {
        code_hash: script.code_hash.clone(),
        hash_type: script.hash_type.into(),
        args: script.args.clone(),
    }
}

fn udt_dep_to_json(dep: &fiber_types::UdtDep) -> JsonUdtDep {
    JsonUdtDep {
        cell_dep: dep.cell_dep.as_ref().map(udt_cell_dep_to_json),
        type_id: dep.type_id.clone(),
    }
}

fn udt_cell_dep_to_json(cell_dep: &fiber_types::UdtCellDep) -> JsonUdtCellDep {
    JsonUdtCellDep {
        out_point: cell_dep.out_point.clone(),
        dep_type: cell_dep.dep_type.into(),
    }
}

// ─── ChannelUpdateInfo Conversions ──────────────────────────────────────────

/// Convert internal ChannelUpdateInfo to JSON ChannelUpdateInfo.
pub fn channel_update_info_to_json(info: &fiber_types::ChannelUpdateInfo) -> JsonChannelUpdateInfo {
    JsonChannelUpdateInfo {
        timestamp: info.timestamp,
        enabled: info.enabled,
        outbound_liquidity: info.outbound_liquidity,
        tlc_expiry_delta: info.tlc_expiry_delta,
        tlc_minimum_value: info.tlc_minimum_value,
        fee_rate: info.fee_rate,
    }
}

// ─── SessionRoute Conversions ───────────────────────────────────────────────

/// Convert internal SessionRoute to JSON SessionRoute.
pub fn session_route_to_json(route: &fiber_types::SessionRoute) -> JsonSessionRoute {
    JsonSessionRoute {
        nodes: route
            .nodes
            .iter()
            .map(|node| JsonSessionRouteNode {
                pubkey: pubkey_to_json_pubkey(&node.pubkey),
                amount: node.amount,
                channel_outpoint: node.channel_outpoint.clone(),
            })
            .collect(),
    }
}

// ─── RouterHop Conversions ──────────────────────────────────────────────────

/// Convert internal RouterHop to JSON RouterHop.
pub fn router_hop_to_json(hop: &fiber_types::RouterHop) -> fiber_json_types::payment::RouterHop {
    fiber_json_types::payment::RouterHop {
        target: pubkey_to_json_pubkey(&hop.target),
        channel_outpoint: hop.channel_outpoint.clone(),
        amount_received: hop.amount_received,
        incoming_tlc_expiry: hop.incoming_tlc_expiry,
    }
}

/// Convert JSON RouterHop to internal RouterHop.
pub fn json_to_router_hop(
    hop: &fiber_json_types::payment::RouterHop,
) -> Result<fiber_types::RouterHop, String> {
    Ok(fiber_types::RouterHop {
        target: json_pubkey_to_pubkey(&hop.target)?,
        channel_outpoint: hop.channel_outpoint.clone(),
        amount_received: hop.amount_received,
        incoming_tlc_expiry: hop.incoming_tlc_expiry,
    })
}

// ─── HopHint Conversions ────────────────────────────────────────────────────

/// Convert JSON HopHint to internal HopHint.
pub fn json_to_hop_hint(
    hint: &fiber_json_types::payment::HopHint,
) -> Result<fiber_types::HopHint, String> {
    Ok(fiber_types::HopHint {
        pubkey: json_pubkey_to_pubkey(&hint.pubkey)?,
        channel_outpoint: hint.channel_outpoint.clone(),
        fee_rate: hint.fee_rate,
        tlc_expiry_delta: hint.tlc_expiry_delta,
    })
}

// ─── HopRequire Conversions ─────────────────────────────────────────────────

/// Convert JSON HopRequire to internal HopRequire.
pub fn json_to_hop_require(
    hop: &fiber_json_types::payment::HopRequire,
) -> Result<fiber_types::HopRequire, String> {
    Ok(fiber_types::HopRequire {
        pubkey: json_pubkey_to_pubkey(&hop.pubkey)?,
        channel_outpoint: hop.channel_outpoint.clone(),
    })
}

// ─── PaymentCustomRecords Conversions ───────────────────────────────────────

/// Convert JSON PaymentCustomRecords to internal PaymentCustomRecords.
pub fn json_to_payment_custom_records(
    records: &fiber_json_types::payment::PaymentCustomRecords,
) -> fiber_types::PaymentCustomRecords {
    fiber_types::PaymentCustomRecords {
        data: records.data.clone(),
    }
}

/// Convert internal PaymentCustomRecords to JSON PaymentCustomRecords.
pub fn payment_custom_records_to_json(
    records: &fiber_types::PaymentCustomRecords,
) -> fiber_json_types::payment::PaymentCustomRecords {
    fiber_json_types::payment::PaymentCustomRecords {
        data: records.data.clone(),
    }
}

// ─── CkbInvoice Conversions ─────────────────────────────────────────────────

/// Convert an internal CkbInvoice to a JSON CkbInvoice.
pub fn ckb_invoice_to_json(
    invoice: &fiber_types::CkbInvoice,
) -> fiber_json_types::invoice::CkbInvoice {
    fiber_json_types::invoice::CkbInvoice {
        currency: currency_to_json(&invoice.currency),
        amount: invoice.amount,
        signature: invoice.signature.as_ref().map(|sig| {
            // InvoiceSignature's Serialize impl produces a hex string (base32-encoded compact sig)
            serde_json::to_value(sig)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default()
        }),
        data: invoice_data_to_json(&invoice.data),
    }
}

fn invoice_data_to_json(data: &fiber_types::InvoiceData) -> fiber_json_types::invoice::InvoiceData {
    fiber_json_types::invoice::InvoiceData {
        timestamp: data.timestamp,
        payment_hash: hash256_to_json_hash256(&data.payment_hash),
        attrs: data.attrs.iter().map(attribute_to_json).collect(),
    }
}

fn attribute_to_json(attr: &fiber_types::Attribute) -> fiber_json_types::invoice::Attribute {
    use fiber_json_types::invoice::Attribute as JsonAttr;
    use fiber_types::Attribute as InternalAttr;

    match attr {
        InternalAttr::FinalHtlcTimeout(v) => JsonAttr::FinalHtlcTimeout(*v),
        InternalAttr::FinalHtlcMinimumExpiryDelta(v) => JsonAttr::FinalHtlcMinimumExpiryDelta(*v),
        InternalAttr::ExpiryTime(d) => JsonAttr::ExpiryTime(*d),
        InternalAttr::Description(s) => JsonAttr::Description(s.clone()),
        InternalAttr::FallbackAddr(s) => JsonAttr::FallbackAddr(s.clone()),
        InternalAttr::UdtScript(script) => {
            // CkbScript wraps PackedScript (molecule type); use as_slice() for bytes
            let bytes = script.0.as_slice();
            JsonAttr::UdtScript(format!("0x{}", hex::encode(bytes)))
        }
        InternalAttr::PayeePublicKey(pk) => JsonAttr::PayeePublicKey(JsonPubkey(pk.serialize())),
        InternalAttr::HashAlgorithm(algo) => JsonAttr::HashAlgorithm(hash_algorithm_to_json(algo)),
        InternalAttr::Feature(features) => JsonAttr::Feature(features.enabled_features_names()),
        InternalAttr::PaymentSecret(secret) => {
            JsonAttr::PaymentSecret(hash256_to_json_hash256(secret).to_string())
        }
    }
}

// ─── CchOrder → CchOrderResponse Conversion ────────────────────────────────

/// Convert an internal CchOrder to a JSON CchOrderResponse.
pub fn cch_order_to_json(order: &fiber_types::CchOrder) -> fiber_json_types::cch::CchOrderResponse {
    fiber_json_types::cch::CchOrderResponse {
        timestamp: order.created_at,
        expiry_delta_seconds: order.expiry_delta_seconds,
        wrapped_btc_type_script: order.wrapped_btc_type_script.clone(),
        incoming_invoice: cch_invoice_to_json(&order.incoming_invoice),
        outgoing_pay_req: order.outgoing_pay_req.clone(),
        payment_hash: hash256_to_json_hash256(&order.payment_hash),
        amount_sats: order.amount_sats,
        fee_sats: order.fee_sats,
        status: cch_order_status_to_json(&order.status),
    }
}

fn cch_invoice_to_json(invoice: &fiber_types::CchInvoice) -> JsonCchInvoice {
    match invoice {
        fiber_types::CchInvoice::Fiber(inv) => JsonCchInvoice::Fiber(inv.to_string()),
        fiber_types::CchInvoice::Lightning(inv) => JsonCchInvoice::Lightning(inv.to_string()),
    }
}
