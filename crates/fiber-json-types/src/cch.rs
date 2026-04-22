//! Cross-chain hub types for the Fiber Network JSON-RPC API.

use crate::invoice::Currency;
use crate::schema_helpers::*;
use crate::serde_utils::{Hash256, U128Hex, U64Hex};
use ckb_jsonrpc_types::Script;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// The status of a cross-chain hub order, will update as the order progresses.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub enum CchOrderStatus {
    /// Order is created and waiting for the incoming invoice to collect enough TLCs.
    Pending,
    /// The incoming invoice collected the required TLCs and is ready to send outgoing payment to obtain the preimage.
    IncomingAccepted,
    /// The outgoing payment is in flight.
    OutgoingInFlight,
    /// The outgoing payment is settled and preimage has been obtained.
    OutgoingSuccess,
    /// Both payments are settled and the order succeeds.
    Success,
    /// Order is failed.
    Failed,
}

/// The generated proxy invoice for the incoming payment.
///
/// The JSON representation:
///
/// ```text
/// { "Fiber": String } | { "Lightning": String }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum CchInvoice {
    /// Fiber invoice string
    Fiber(String),
    /// Lightning invoice string
    Lightning(String),
}

/// Parameters for sending BTC via cross-chain hub.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SendBTCParams {
    /// Payment request string for the BTC Lightning payee.
    pub btc_pay_req: String,
    /// Request currency
    pub currency: Currency,
}

/// Cross-chain hub order response.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CchOrderResponse {
    /// Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub timestamp: u64,
    /// Relative expiry time in seconds from `created_at` that the order expires
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expiry_delta_seconds: u64,

    /// Wrapped BTC type script
    pub wrapped_btc_type_script: Script,

    /// Generated invoice for the incoming payment
    pub incoming_invoice: CchInvoice,
    /// The final payee to accept the payment. It has the different network with incoming invoice.
    pub outgoing_pay_req: String,
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
    /// Amount required to pay in Satoshis, including fee
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount_sats: u128,
    /// Fee in Satoshis
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub fee_sats: u128,
    /// Order status
    pub status: CchOrderStatus,
}

/// Parameters for receiving BTC via cross-chain hub.
#[serde_as]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReceiveBTCParams {
    /// Payment request string for the CKB Fiber payee.
    pub fiber_pay_req: String,
}

/// Parameters for getting a CCH order.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetCchOrderParams {
    /// Payment hash for the HTLC for both CKB and BTC.
    pub payment_hash: Hash256,
}
