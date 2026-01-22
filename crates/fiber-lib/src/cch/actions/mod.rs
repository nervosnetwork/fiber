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

use crate::cch::{actor::CchState, CchMessage, CchOrder, CchOrderStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CchOrderAction {
    TrackIncomingInvoice,
    SendOutgoingPayment,
    TrackOutgoingPayment,
    SettleIncomingInvoice,
}

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
