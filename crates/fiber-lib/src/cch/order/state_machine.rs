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
        failure_reason: Option<String>,
    },
    OutgoingPaymentChanged {
        status: PaymentStatus,
        payment_preimage: Option<Hash256>,
        failure_reason: Option<String>,
    },
}

pub struct CchOrderStateMachine;

impl CchOrderStateMachine {
    /// Apply an event to the order and return the transition.
    ///
    /// Returns `Ok(Some(new_status))` if the order is transit to a new status.
    /// or `Ok(None)` if the order stays in the same status without changes.
    pub fn apply(
        order: &mut CchOrder,
        event: CchOrderEvent,
    ) -> Result<Option<CchOrderStatus>, CchError> {
        match event {
            CchOrderEvent::IncomingInvoiceChanged {
                status,
                failure_reason,
            } => Self::try_transite_to(order, status.into(), move || {
                failure_reason.unwrap_or_else(|| format!("incoming invoice failed: {}", status))
            }),
            CchOrderEvent::OutgoingPaymentChanged {
                status,
                payment_preimage,
                failure_reason,
            } => {
                if status == PaymentStatus::Success && payment_preimage.is_none() {
                    return Err(CchError::SettledPaymentMissingPreimage);
                }
                // Verify preimage hashes to payment_hash if provided
                if let Some(ref preimage) = payment_preimage {
                    use crate::fiber::hash_algorithm::HashAlgorithm;
                    let hash_algorithm = HashAlgorithm::Sha256;
                    let computed_hash = hash_algorithm.hash(*preimage);
                    if computed_hash.as_slice() != order.payment_hash.as_ref() {
                        return Err(CchError::PreimageHashMismatch);
                    }
                }
                let new_status = Self::try_transite_to(order, status.into(), move || {
                    failure_reason
                        .unwrap_or_else(|| format!("outgoing payment failed: {:?}", status))
                })?;
                if new_status.is_some() && order.payment_preimage.is_none() {
                    order.payment_preimage = payment_preimage;
                }
                Ok(new_status)
            }
        }
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
            (_, CchOrderStatus::Failed) if from != CchOrderStatus::Succeeded => true,
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
    ) -> Result<Option<CchOrderStatus>, CchError>
    where
        F: FnOnce() -> String,
    {
        if !Self::allow_transition(order.status, to) {
            return Err(CchError::InvalidTransition(order.status, to));
        }
        if order.status != to {
            order.status = to;
            if to == CchOrderStatus::Failed {
                order.failure_reason = Some(failure_reason_fn());
            }
            Ok(Some(to))
        } else {
            Ok(None)
        }
    }
}

impl From<CchTrackingEvent> for CchOrderEvent {
    fn from(event: CchTrackingEvent) -> Self {
        match event {
            CchTrackingEvent::InvoiceChanged {
                payment_hash: _,
                status,
                failure_reason,
            } => CchOrderEvent::IncomingInvoiceChanged {
                status,
                failure_reason,
            },
            CchTrackingEvent::PaymentChanged {
                payment_hash: _,
                status,
                payment_preimage,
                failure_reason,
            } => CchOrderEvent::OutgoingPaymentChanged {
                status,
                payment_preimage,
                failure_reason,
            },
        }
    }
}
