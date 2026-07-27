use anyhow::{anyhow, Result};
use fiber_types::{CchOrder, CchOrderStatus, Hash256};
use ractor::ActorRef;

use crate::cch::{
    actions::{
        backend_dispatchers::{dispatch_payment_handler, PaymentHandlerType},
        ActionExecutor,
    },
    actor::CchState,
    trackers::LndTrackerMessage,
    CchFiberAgentRef, CchMessage, CchOrderStore,
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

struct TrackFiberOutgoingPaymentExecutor {
    payment_hash: Hash256,
    cch_actor_ref: ActorRef<CchMessage>,
    fiber_agent_ref: CchFiberAgentRef,
    retry_count: u32,
}

#[async_trait::async_trait]
impl ActionExecutor for TrackFiberOutgoingPaymentExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        self.fiber_agent_ref
            .forward_get_payment(&self.cch_actor_ref, self.payment_hash, self.retry_count)
            .await
            .map_err(|err| anyhow!(err.to_string()))
    }
}

impl TrackOutgoingPaymentDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        matches!(
            order.status,
            CchOrderStatus::IncomingAccepted | CchOrderStatus::OutgoingInFlight
        )
    }

    pub fn dispatch<S: CchOrderStore>(
        state: &CchState<S>,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        if !Self::should_dispatch(order) {
            return None;
        }

        match dispatch_payment_handler(order) {
            PaymentHandlerType::Fiber => Some(Box::new(TrackFiberOutgoingPaymentExecutor {
                payment_hash: order.payment_hash,
                cch_actor_ref: cch_actor_ref.clone(),
                fiber_agent_ref: state.fiber_agent_ref.clone(),
                retry_count,
            })),
            PaymentHandlerType::Lightning => {
                Some(Box::new(TrackLightningOutgoingPaymentExecutor {
                    payment_hash: order.payment_hash,
                    lnd_tracker_ref: state.lnd_tracker.clone(),
                }))
            }
        }
    }
}
