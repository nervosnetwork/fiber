use ractor::ActorRef;

use crate::{
    cch::{
        actions::{
            backend_dispatchers::{dispatch_invoice_handler, InvoiceHandlerType},
            ActionExecutor,
        },
        actor::CchState,
        trackers::LndTrackerMessage,
        CchMessage, CchOrder, CchOrderStore,
    },
    fiber::types::Hash256,
};
use anyhow::Result;

pub struct TrackIncomingInvoiceDispatcher;

pub struct TrackLightningIncomingInvoiceExecutor {
    payment_hash: Hash256,
    lnd_tracker_ref: ActorRef<LndTrackerMessage>,
}

#[async_trait::async_trait]
impl ActionExecutor for TrackLightningIncomingInvoiceExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        self.lnd_tracker_ref
            .send_message(LndTrackerMessage::TrackInvoice(self.payment_hash))?;
        Ok(())
    }
}

impl TrackIncomingInvoiceDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        // Invoice must be tracked in the whole life cycle.
        !order.is_final()
    }

    pub fn dispatch<S: CchOrderStore>(
        state: &CchState<S>,
        _cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        _retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        if !Self::should_dispatch(order) {
            return None;
        }

        match dispatch_invoice_handler(order) {
            // `CchActor` will track all fiber invoices, so there's nothing to do here to track a single invoice.
            InvoiceHandlerType::Fiber => None,
            InvoiceHandlerType::Lightning => {
                Some(Box::new(TrackLightningIncomingInvoiceExecutor {
                    payment_hash: order.payment_hash,
                    lnd_tracker_ref: state.lnd_tracker.clone(),
                }))
            }
        }
    }
}
