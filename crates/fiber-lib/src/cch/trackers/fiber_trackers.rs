use std::sync::Arc;

use ractor::OutputPort;

use crate::cch::trackers::CchTrackingEvent;
use crate::invoice::PreimageStore as _;
use crate::store::store_impl::StoreChange;
use crate::store::Store;
use fiber_types::payment::PaymentStatus;

/// Maps StoreChange events into CchTrackingEvent and emits them via an OutputPort.
pub struct CchFiberStoreWatcher;

impl CchFiberStoreWatcher {
    /// Build a watcher closure that maps `StoreChange` events into `CchTrackingEvent`
    /// and sends them through the given `OutputPort`.
    ///
    /// The `store` reference is needed because some mappings require reading
    /// additional data (e.g. looking up a preimage by payment hash).
    pub fn build_watcher(
        store: Store,
        out: Arc<OutputPort<CchTrackingEvent>>,
    ) -> Arc<dyn Fn(StoreChange) + Send + Sync> {
        Arc::new(move |change: StoreChange| {
            tracing::debug!("CchStoreWatcher received store change: {:?}", change);
            if let Some(event) = map_store_change_to_event(&store, &change) {
                tracing::debug!("CchStoreWatcher emitting event: {:?}", event);
                out.send(event);
            } else {
                tracing::trace!("CchStoreWatcher ignoring non-relevant change: {:?}", change);
            }
        })
    }
}

fn map_store_change_to_event(store: &Store, change: &StoreChange) -> Option<CchTrackingEvent> {
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
            let payment_preimage = store.get_preimage(payment_hash);
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
