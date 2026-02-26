//! Cross-chain hub (CCH) types for the Fiber Network Node RPC API.

use crate::invoice::Currency;
use crate::serde_utils::{U128Hex, U64Hex};
use crate::Hash256;

use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

pub use fiber_types::cch::CchOrderStatus;

// ============================================================
// CCH invoice
// ============================================================

/// The generated proxy invoice for the incoming payment.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CchInvoice {
    /// Fiber invoice as an encoded string
    Fiber(String),
    /// Lightning invoice
    Lightning(#[serde_as(as = "DisplayFromStr")] Bolt11Invoice),
}

// ============================================================
// CCH order
// ============================================================

/// A cross-chain hub order.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrder {
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    #[serde_as(as = "U64Hex")]
    pub expiry_delta_seconds: u64,
    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,
    pub outgoing_pay_req: String,
    pub incoming_invoice: CchInvoice,
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    pub status: CchOrderStatus,
    pub failure_reason: Option<String>,
}

// ============================================================
// RPC param/result types
// ============================================================

#[derive(Serialize, Deserialize)]
pub struct SendBTCParams {
    /// Payment request string for the BTC Lightning payee.
    pub btc_pay_req: String,
    /// Request currency
    pub currency: Currency,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrderResponse {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    /// Relative expiry time in seconds
    #[serde_as(as = "U64Hex")]
    pub expiry_delta_seconds: u64,
    /// Wrapped BTC type script
    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,
    /// Generated invoice for the incoming payment
    pub incoming_invoice: CchInvoice,
    /// The final payee to accept the payment.
    pub outgoing_pay_req: String,
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
    /// Amount required to pay in Satoshis, including fee
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,
    /// Order status
    pub status: CchOrderStatus,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ReceiveBTCParams {
    /// Payment request string for the CKB Fiber payee.
    pub fiber_pay_req: String,
}

#[derive(Serialize, Deserialize)]
pub struct GetCchOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
}
