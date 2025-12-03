use super::{CchOrder, CchOrderStatus};
use crate::{
    cch::{trackers::CchTrackingEvent, CchError},
    fiber::{payment::PaymentStatus, types::Hash256},
    invoice::CkbInvoiceStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CchOrderEvent {
    IncomingInvoiceChanged {
        status: CkbInvoiceStatus,
    },
    OutgoingPaymentChanged {
        status: PaymentStatus,
        payment_preimage: Option<Hash256>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CchOrderAction {
    TrackIncomingInvoice,
    SendOutgoingPayment,
    TrackOutgoingPayment,
    SettleIncomingInvoice,
}

/// The state machine transition result contains the new order and the actions to be taken.
#[derive(Debug, Clone)]
pub struct CchOrderTransition {
    pub order: CchOrder,
    /// Whether the order has been modified.
    pub dirty: bool,
    pub actions: Vec<CchOrderAction>,
}

pub struct CchOrderStateMachine;

impl CchOrderStateMachine {
    /// The actions to be taken when the order enters a new status.
    pub fn on_entering(order: &CchOrder) -> Vec<CchOrderAction> {
        match order.status {
            CchOrderStatus::Pending => vec![CchOrderAction::TrackIncomingInvoice],
            CchOrderStatus::IncomingAccepted => vec![
                CchOrderAction::SendOutgoingPayment,
                CchOrderAction::TrackOutgoingPayment,
            ],
            CchOrderStatus::OutgoingInFlight => vec![CchOrderAction::TrackOutgoingPayment],
            CchOrderStatus::OutgoingSucceeded => vec![CchOrderAction::SettleIncomingInvoice],
            CchOrderStatus::Succeeded => vec![],
            CchOrderStatus::Failed => vec![],
        }
    }

    /// Apply an event to the order and return the transition.
    pub fn apply(
        mut order: CchOrder,
        event: CchOrderEvent,
    ) -> Result<CchOrderTransition, CchError> {
        let prev_status = order.status;

        match event {
            CchOrderEvent::IncomingInvoiceChanged { status } => {
                Self::try_transite_to(&mut order, status.into(), || {
                    format!("incoming invoice failed: {}", status)
                })?;
            }
            CchOrderEvent::OutgoingPaymentChanged {
                status,
                payment_preimage,
            } => {
                if status == PaymentStatus::Success && payment_preimage.is_none() {
                    return Err(CchError::SettledPaymentMissingPreimage);
                }
                Self::try_transite_to(&mut order, status.into(), || {
                    format!("outgoing payment failed: {:?}", status)
                })?;
                if order.payment_preimage.is_none() {
                    order.payment_preimage = payment_preimage;
                }
            }
        }

        // Currently we won't modify the order unless after a transition to a new status.
        let dirty = order.status != prev_status;
        let actions = if dirty {
            Self::on_entering(&order)
        } else {
            vec![]
        };
        Ok(CchOrderTransition {
            order,
            dirty,
            actions,
        })
    }

    fn allow_transition(from: CchOrderStatus, to: CchOrderStatus) -> bool {
        match (from, to) {
            (CchOrderStatus::Pending, CchOrderStatus::IncomingAccepted) => true,
            // When the payment succeeds immediately, we can transit to the `OutgoingSucceeded` directly.
            (
                CchOrderStatus::IncomingAccepted,
                CchOrderStatus::OutgoingInFlight | CchOrderStatus::OutgoingSucceeded,
            ) => true,
            (CchOrderStatus::OutgoingInFlight, CchOrderStatus::OutgoingSucceeded) => true,
            (CchOrderStatus::OutgoingSucceeded, CchOrderStatus::Succeeded) => true,
            (_, CchOrderStatus::Failed) => true,
            _ => {
                // Allow staying in the same status
                from == to
            }
        }
    }

    fn try_transite_to<F>(
        order: &mut CchOrder,
        to: CchOrderStatus,
        failure_reason_fn: F,
    ) -> Result<(), CchError>
    where
        F: FnOnce() -> String,
    {
        if !Self::allow_transition(order.status, to) {
            return Err(CchError::InvalidTransition(order.status, to));
        }
        order.status = to;
        if to == CchOrderStatus::Failed {
            order.failure_reason = Some(failure_reason_fn());
        }
        Ok(())
    }
}

impl From<CchTrackingEvent> for CchOrderEvent {
    fn from(event: CchTrackingEvent) -> Self {
        match event {
            CchTrackingEvent::InvoiceChanged {
                payment_hash: _,
                status,
            } => CchOrderEvent::IncomingInvoiceChanged { status },
            CchTrackingEvent::PaymentChanged {
                payment_hash: _,
                status,
                payment_preimage,
            } => CchOrderEvent::OutgoingPaymentChanged {
                status,
                payment_preimage,
            },
        }
    }
}
