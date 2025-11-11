use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::lnrpc;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::{
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
    invoice::CkbInvoice,
};

/// The status of a cross-chain hub order, will update as the order progresses.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    /// Order is created and has not send out payments yet.
    Pending = 0,
    /// HTLC in the incoming payment is accepted.
    IncomingAccepted = 1,
    /// There's an outgoing payment in flight.
    OutgoingInFlight = 2,
    /// The outgoing payment is settled.
    OutgoingSettled = 3,
    /// Both payments are settled and the order succeeds.
    Succeeded = 4,
    /// Order is failed.
    Failed = 5,
}

/// Lnd payment is the outgoing part of a CCHOrder to send BTC from Fiber to Lightning
impl From<lnrpc::payment::PaymentStatus> for CchOrderStatus {
    fn from(status: lnrpc::payment::PaymentStatus) -> Self {
        use lnrpc::payment::PaymentStatus;
        match status {
            PaymentStatus::Succeeded => CchOrderStatus::OutgoingSettled,
            PaymentStatus::Failed => CchOrderStatus::Failed,
            _ => CchOrderStatus::OutgoingInFlight,
        }
    }
}

/// Lnd invoice is the incoming part of a CCHOrder to receive BTC from Lightning to Fiber
impl From<lnrpc::invoice::InvoiceState> for CchOrderStatus {
    fn from(state: lnrpc::invoice::InvoiceState) -> Self {
        use lnrpc::invoice::InvoiceState;
        match state {
            InvoiceState::Accepted => CchOrderStatus::IncomingAccepted,
            InvoiceState::Canceled => CchOrderStatus::Failed,
            InvoiceState::Settled => CchOrderStatus::Succeeded,
            _ => CchOrderStatus::Pending,
        }
    }
}

/// The generated proxy invoice for the incoming payment.
///
/// The JSON representation:
///
/// ```text
/// { "Fiber": String } | { "Lightning": String }
/// ```
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CchInvoice {
    /// Fiber invoice that once paid, the hub will send the outgoing payment to Lightning
    Fiber(#[serde_as(as = "DisplayFromStr")] CkbInvoice),
    /// Lightning invoice that once paid, the hub will send the outgoing payment to Fiber
    Lightning(#[serde_as(as = "DisplayFromStr")] Bolt11Invoice),
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrder {
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    // Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,
    // The minimal expiry delta in milliseconds of the final TLC hop in the CKB network
    #[serde_as(as = "U64Hex")]
    pub ckb_final_tlc_expiry_delta: u64,

    pub wrapped_btc_type_script: ckb_jsonrpc_types::Script,

    pub outgoing_pay_req: String,
    pub incoming_invoice: CchInvoice,
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,

    /// Amount required to pay in Satoshis via BTC or wrapped BTC, including the fee for the cross-chain hub
    #[serde_as(as = "U128Hex")]
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub status: CchOrderStatus,
}

impl CchOrder {
    pub fn is_from_fiber_to_lightning(&self) -> bool {
        matches!(self.incoming_invoice, CchInvoice::Fiber(_))
    }
}
