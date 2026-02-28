use std::sync::Arc;

use ractor::OutputPort;

use crate::cch::trackers::CchTrackingEvent;
use crate::invoice::PreimageStore as _;
use crate::store::store_impl::{StoreChange, StoreChangeWatcher};
use crate::store::Store;
use fiber_types::payment::PaymentStatus;

/// Maps StoreChange events into CchTrackingEvent and emits them via an OutputPort.
#[derive(Debug)]
pub struct CchFiberStoreWatcher {
    // Requires a reference to the store to potentially read additional data
    store: Store,
    out: Arc<OutputPort<CchTrackingEvent>>,
}

impl CchFiberStoreWatcher {
    pub fn new(store: Store, out: Arc<OutputPort<CchTrackingEvent>>) -> Self {
        Self { store, out }
    }

    pub fn port(&self) -> Arc<OutputPort<CchTrackingEvent>> {
        self.out.clone()
    }

    fn map_store_change_to_event(&self, change: &StoreChange) -> Option<CchTrackingEvent> {
        match change {
            StoreChange::PutCkbInvoiceStatus {
                payment_hash,
                invoice_status,
            } => Some(CchTrackingEvent::InvoiceChanged {
                payment_hash: *payment_hash,
                status: *invoice_status,
                failure_reason: None,
            }),
            StoreChange::PutPaymentSession {
                payment_hash,
                payment_session,
            } => {
                let status = payment_session.status;
                let payment_preimage = self.store.get_preimage(payment_hash);
                // If preimage is not available for a settled payment, defer the notification on putting preimage.
                (status != PaymentStatus::Success || payment_preimage.is_some()).then_some(
                    CchTrackingEvent::PaymentChanged {
                        payment_hash: *payment_hash,
                        payment_preimage,
                        status,
                        failure_reason: None,
                    },
                )
            }
            StoreChange::PutPreimage {
                payment_hash,
                payment_preimage,
            } => {
                // When preimage is inserted, assume the payment is settled.
                Some(CchTrackingEvent::PaymentChanged {
                    payment_hash: *payment_hash,
                    payment_preimage: Some(*payment_preimage),
                    status: PaymentStatus::Success,
                    failure_reason: None,
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
