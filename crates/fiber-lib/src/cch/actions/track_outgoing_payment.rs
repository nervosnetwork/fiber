use anyhow::{anyhow, Result};
use fiber_types::CchOrder;
use ractor::ActorRef;

use crate::cch::{
    actions::{
        backend_dispatchers::{dispatch_payment_handler, PaymentHandlerType},
        ActionExecutor,
    },
    actor::CchState,
    CchFiberAgentRef, CchMessage, CchOrderStore,
};

pub struct TrackOutgoingPaymentDispatcher;

struct TrackFiberOutgoingPaymentExecutor {
    payment_hash: fiber_types::Hash256,
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
    pub fn dispatch<S: CchOrderStore>(
        state: &CchState<S>,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        match dispatch_payment_handler(order) {
            PaymentHandlerType::Fiber => Some(Box::new(TrackFiberOutgoingPaymentExecutor {
                payment_hash: order.payment_hash,
                cch_actor_ref: cch_actor_ref.clone(),
                fiber_agent_ref: state.fiber_agent_ref.clone(),
                retry_count,
            })),
            // LND's tracker subscribes to all Lightning payment updates.
            PaymentHandlerType::Lightning => None,
        }
    }
}
