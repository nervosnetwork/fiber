pub(crate) mod backend_dispatchers;
pub(crate) mod send_outgoing_payment;
pub(crate) mod settle_incoming_invoice;
pub(crate) mod track_incoming_invoice;
mod track_outgoing_payment;
use send_outgoing_payment::SendOutgoingPaymentDispatcher;
use settle_incoming_invoice::SettleIncomingInvoiceDispatcher;
use track_incoming_invoice::TrackIncomingInvoiceDispatcher;
use track_outgoing_payment::TrackOutgoingPaymentDispatcher;

use anyhow::Result;
use ractor::ActorRef;

use crate::cch::{actor::CchState, order::CchOrderAction, CchMessage, CchOrder};

#[async_trait::async_trait]
pub trait ActionExecutor: Send + Sync {
    async fn execute(self: Box<Self>) -> Result<()>;
}

pub struct ActionDispatcher;

impl ActionDispatcher {
    fn dispatch(
        state: &mut CchState,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        action: CchOrderAction,
    ) -> Option<Box<dyn ActionExecutor>> {
        match action {
            CchOrderAction::TrackIncomingInvoice => {
                TrackIncomingInvoiceDispatcher::dispatch(state, cch_actor_ref, order)
            }
            CchOrderAction::SendOutgoingPayment => {
                SendOutgoingPaymentDispatcher::dispatch(state, cch_actor_ref, order)
            }
            CchOrderAction::TrackOutgoingPayment => {
                TrackOutgoingPaymentDispatcher::dispatch(state, cch_actor_ref, order)
            }
            CchOrderAction::SettleIncomingInvoice => {
                SettleIncomingInvoiceDispatcher::dispatch(state, cch_actor_ref, order)
            }
        }
    }

    /// Execute an action.
    ///
    /// Executor cannot modify the order directly, but can send events to the actor.
    pub async fn execute(
        state: &mut CchState,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        action: CchOrderAction,
    ) -> Result<()> {
        if let Some(executor) = Self::dispatch(state, cch_actor_ref, order, action) {
            return executor.execute().await;
        }
        Ok(())
    }
}
