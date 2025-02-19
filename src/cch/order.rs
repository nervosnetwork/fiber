use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::{
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
    invoice::CkbInvoice,
    store::subscription::{InvoiceState, InvoiceUpdate, PaymentState, PaymentUpdate},
};

/// The status of a cross-chain hub order, will update as the order progresses.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    /// Order is created and the first half has not received complete payment yet.
    Pending = 0,
    /// HTLC in the first half is accepted.
    FirstHalfAccepted = 1,
    /// There's an outgoing payment in flight for the second half.
    SecondHalfInFlight = 2,
    /// The second half payment is succeeded.
    SecondHalfSucceeded = 3,
    /// The first half payment is succeeded.
    FirstHalfSucceeded = 4,
    /// Order is failed.
    Failed = 5,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrder {
    // The payment hash of the order
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    // Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,

    #[serde_as(as = "U128Hex")]
    /// Amount required to pay in Satoshis via wrapped BTC, including the fee for the cross-chain hub
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub in_invoice: CchInvoice,
    pub out_invoice: CchInvoice,
    pub in_state: InvoiceState,
    pub out_state: PaymentState,
}

impl CchOrder {
    pub fn new(
        payment_hash: Hash256,
        created_at: u64,
        expires_after: u64,
        amount_sats: u128,
        fee_sats: u128,
        in_invoice: CchInvoice,
        out_invoice: CchInvoice,
    ) -> Self {
        Self {
            payment_hash,
            payment_preimage: None,
            created_at,
            expires_after,
            amount_sats,
            fee_sats,
            in_invoice,
            out_invoice,
            in_state: InvoiceState::Open,
            out_state: PaymentState::Created,
        }
    }

    pub fn status(&self) -> Result<CchOrderStatus, CchStateError> {
        let status = match (self.in_state, self.out_state) {
            (InvoiceState::Cancelled | InvoiceState::Expired, _) => CchOrderStatus::Failed,
            (_, PaymentState::Failed) => CchOrderStatus::Failed,
            (InvoiceState::Open, PaymentState::Created) => CchOrderStatus::Pending,
            (InvoiceState::Open, _) => {
                return Err(format!(
                    "The second payment has a state too new for a just open first payment: {:?}",
                    self.out_state
                ))
            }
            (
                InvoiceState::Received {
                    amount: _amount,
                    is_finished,
                },
                PaymentState::Created,
            ) => {
                if is_finished {
                    CchOrderStatus::FirstHalfAccepted
                } else {
                    CchOrderStatus::Pending
                }
            }
            (
                InvoiceState::Received {
                    amount: _amount,
                    is_finished,
                },
                PaymentState::Inflight,
            ) => {
                if !is_finished {
                    return Err("The second payment should be inflight when the first one is unfinished".to_string());
                }
                CchOrderStatus::SecondHalfInFlight
            }
            (InvoiceState::Received { .. }, PaymentState::Success { .. }) => {
                CchOrderStatus::SecondHalfSucceeded
            }
            (InvoiceState::Paid, PaymentState::Success { .. }) => {
                CchOrderStatus::SecondHalfSucceeded
            }
            (InvoiceState::Paid, _) => {
                return Err(format!(
                    "The first payment succeeded while the second payment has state (should have been succeeded or failed): {:?}",
                    self.out_state
                ))
            }
        };
        Ok(status)
    }
}

pub type CchStateError = String;

pub type FiberInvoiceUpdate = InvoiceUpdate;
pub type FiberPaymentUpdate = PaymentUpdate;
pub type LightningInvoiceUpdate = InvoiceUpdate;
pub type LightningPaymentUpdate = PaymentUpdate;

pub struct CchInvoiceUpdate {
    pub is_fiber: bool,
    pub update: InvoiceUpdate,
}

pub struct CchPaymentUpdate {
    pub is_fiber: bool,
    pub update: PaymentUpdate,
}

/// A cross-chain hub invoice, which can be either a lightning network invoice or a fiber network invoice.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CchInvoice {
    /// A lightning network invoice
    Lightning(#[serde_as(as = "DisplayFromStr")] Bolt11Invoice),
    /// A fiber network invoice
    Fiber(#[serde_as(as = "DisplayFromStr")] CkbInvoice),
}

impl CchInvoice {
    pub fn is_fiber(&self) -> bool {
        matches!(self, CchInvoice::Fiber(_))
    }

    pub fn payment_hash(&self) -> Hash256 {
        match self {
            CchInvoice::Lightning(invoice) => invoice.payment_hash().into(),
            CchInvoice::Fiber(invoice) => *invoice.payment_hash(),
        }
    }
}
