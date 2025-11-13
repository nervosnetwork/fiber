use lightning_invoice::Bolt11Invoice;
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
    /// Order is created and has not received the incoming payment
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
    pub fn is_incoming_invoice_fiber(&self) -> bool {
        matches!(self.incoming_invoice, CchInvoice::Fiber(_))
    }
    pub fn is_incoming_invoice_lnd(&self) -> bool {
        matches!(self.incoming_invoice, CchInvoice::Lightning(_))
    }
    pub fn is_outgoing_payment_fiber(&self) -> bool {
        self.is_incoming_invoice_lnd()
    }
    pub fn is_outgoing_payment_lnd(&self) -> bool {
        self.is_incoming_invoice_fiber()
    }

    pub fn is_awaiting_invoice_event(&self) -> bool {
        self.status.is_awaiting_invoice_event()
    }
    pub fn is_awaiting_fiber_invoice_event(&self) -> bool {
        self.is_awaiting_invoice_event() && self.is_incoming_invoice_fiber()
    }
    pub fn is_awaiting_lnd_invoice_event(&self) -> bool {
        self.is_awaiting_invoice_event() && self.is_incoming_invoice_lnd()
    }
    pub fn is_awaiting_payment_event(&self) -> bool {
        self.status.is_awaiting_payment_event()
    }
    pub fn is_awaiting_fiber_payment_event(&self) -> bool {
        self.is_awaiting_payment_event() && self.is_outgoing_payment_fiber()
    }
    pub fn is_awaiting_lnd_payment_event(&self) -> bool {
        self.is_awaiting_payment_event() && self.is_outgoing_payment_lnd()
    }
}

impl CchOrderStatus {
    pub fn is_awaiting_invoice_event(&self) -> bool {
        matches!(self, CchOrderStatus::Pending)
    }
    pub fn is_awaiting_payment_event(&self) -> bool {
        matches!(
            self,
            CchOrderStatus::IncomingAccepted | CchOrderStatus::OutgoingInFlight
        )
    }
}
