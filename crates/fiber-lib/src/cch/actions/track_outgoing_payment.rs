use anyhow::Result;
use fiber_types::{CchOrder, CchOrderStatus, Hash256};
use ractor::ActorRef;

use crate::cch::{
    actions::{
        backend_dispatchers::{dispatch_payment_handler, PaymentHandlerType},
        ActionExecutor,
    },
    actor::CchState,
    trackers::LndTrackerMessage,
    CchMessage, CchOrderStore,
};

pub struct TrackOutgoingPaymentDispatcher;

struct TrackLightningOutgoingPaymentExecutor {
    payment_hash: Hash256,
    lnd_tracker_ref: ActorRef<LndTrackerMessage>,
}

#[async_trait::async_trait]
impl ActionExecutor for TrackLightningOutgoingPaymentExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        self.lnd_tracker_ref
            .send_message(LndTrackerMessage::TrackPayment(self.payment_hash))?;
        Ok(())
    }
}

impl TrackOutgoingPaymentDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        matches!(
            order.status,
            CchOrderStatus::IncomingAccepted | CchOrderStatus::OutgoingInFlight
        ) && dispatch_payment_handler(order) == PaymentHandlerType::Lightning
    }

    pub fn dispatch<S: CchOrderStore>(
        state: &CchState<S>,
        _cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        _retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        if !Self::should_dispatch(order) {
            // `CchActor` already tracks all Fiber payments through store changes.
            return None;
        }

        Some(Box::new(TrackLightningOutgoingPaymentExecutor {
            payment_hash: order.payment_hash,
            lnd_tracker_ref: state.lnd_tracker.clone(),
        }))
    }
}
