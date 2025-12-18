use std::sync::Arc;

use ractor::OutputPort;

use crate::cch::{CchIncomingEvent, CchIncomingPaymentStatus, CchOutgoingPaymentStatus};
use crate::fiber::payment::PaymentStatus;
use crate::invoice::{CkbInvoiceStatus, PreimageStore as _};
use crate::store::store_impl::{StoreChange, StoreChangeWatcher};
use crate::store::Store;

/// Maps StoreChange events into CchIncomingEvent and emits them via an OutputPort.
#[derive(Debug)]
pub struct CchFiberStoreWatcher {
    // Requires a reference to the store to potentially read additional data
    store: Store,
    out: Arc<OutputPort<CchIncomingEvent>>,
}

impl CchFiberStoreWatcher {
    pub fn new(store: Store, out: Arc<OutputPort<CchIncomingEvent>>) -> Self {
        Self { store, out }
    }

    pub fn port(&self) -> Arc<OutputPort<CchIncomingEvent>> {
        self.out.clone()
    }

    fn map_store_change_to_event(&self, change: &StoreChange) -> Option<CchIncomingEvent> {
        match change {
            StoreChange::PutCkbInvoiceStatus {
                payment_hash,
                invoice_status,
            } => Some(CchIncomingEvent::InvoiceChanged {
                payment_hash: *payment_hash,
                status: map_invoice_status(*invoice_status),
            }),

            StoreChange::PutPaymentSession {
                payment_hash,
                payment_session,
            } => {
                let status = map_payment_status(payment_session.status);
                let payment_preimage = self.store.get_preimage(payment_hash);
                // If preimage is not available for a settled payment, defer the notification on deleting preimage.
                (status != CchOutgoingPaymentStatus::Settled || payment_preimage.is_some())
                    .then_some(CchIncomingEvent::PaymentChanged {
                        payment_hash: *payment_hash,
                        payment_preimage,
                        status,
                    })
            }
            StoreChange::PutPreimage {
                payment_hash,
                payment_preimage,
            } => {
                // When preimage is inserted, assume the payment is settled.
                Some(CchIncomingEvent::PaymentChanged {
                    payment_hash: *payment_hash,
                    payment_preimage: Some(*payment_preimage),
                    status: CchOutgoingPaymentStatus::Settled,
                })
            }
        }
    }
}

impl StoreChangeWatcher for CchFiberStoreWatcher {
    fn on_store_change(&self, change: StoreChange) {
        tracing::debug!("CchStoreWatcher received store change: {:?}", change);
        if let Some(event) = self.map_store_change_to_event(&change) {
            tracing::debug!("CchStoreWatcher emitting event: {:?}", event);
            self.out.send(event);
        } else {
            tracing::trace!("CchStoreWatcher ignoring non-relevant change: {:?}", change);
        }
    }
}

fn map_invoice_status(status: CkbInvoiceStatus) -> CchIncomingPaymentStatus {
    match status {
        CkbInvoiceStatus::Open => CchIncomingPaymentStatus::InFlight,
        CkbInvoiceStatus::Received => CchIncomingPaymentStatus::Accepted,
        CkbInvoiceStatus::Paid => CchIncomingPaymentStatus::Settled,
        CkbInvoiceStatus::Cancelled => CchIncomingPaymentStatus::Failed,
        CkbInvoiceStatus::Expired => CchIncomingPaymentStatus::Failed,
    }
}

fn map_payment_status(status: PaymentStatus) -> CchOutgoingPaymentStatus {
    match status {
        PaymentStatus::Created | PaymentStatus::Inflight => CchOutgoingPaymentStatus::InFlight,
        PaymentStatus::Success => CchOutgoingPaymentStatus::Settled,
        PaymentStatus::Failed => CchOutgoingPaymentStatus::Failed,
    }
}
