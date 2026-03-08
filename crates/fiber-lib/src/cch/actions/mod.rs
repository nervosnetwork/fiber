pub(crate) mod backend_dispatchers;
pub(crate) mod send_outgoing_payment;
pub(crate) mod settle_incoming_invoice;
pub(crate) mod track_incoming_invoice;
mod track_outgoing_payment;
use fiber_types::{CchOrder, CchOrderStatus};
use send_outgoing_payment::SendOutgoingPaymentDispatcher;
use settle_incoming_invoice::SettleIncomingInvoiceDispatcher;
use track_incoming_invoice::TrackIncomingInvoiceDispatcher;
use track_outgoing_payment::TrackOutgoingPaymentDispatcher;

use anyhow::Result;
use ractor::ActorRef;

use crate::cch::{actor::CchState, CchMessage, CchOrderStore};

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
    fn dispatch<S: CchOrderStore>(
        state: &CchState<S>,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        action: CchOrderAction,
        retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        match action {
            CchOrderAction::TrackIncomingInvoice => {
                TrackIncomingInvoiceDispatcher::dispatch(state, cch_actor_ref, order, retry_count)
            }
            CchOrderAction::SendOutgoingPayment => {
                SendOutgoingPaymentDispatcher::dispatch(state, cch_actor_ref, order, retry_count)
            }
            CchOrderAction::TrackOutgoingPayment => {
                TrackOutgoingPaymentDispatcher::dispatch(state, cch_actor_ref, order, retry_count)
            }
            CchOrderAction::SettleIncomingInvoice => {
                SettleIncomingInvoiceDispatcher::dispatch(state, cch_actor_ref, order, retry_count)
            }
        }
    }

    /// The actions to be taken when the state machine is started for the order.
    pub fn on_starting(order: &CchOrder) -> Vec<CchOrderAction> {
        let mut actions = Self::on_entering(order);
        match order.status {
            CchOrderStatus::IncomingAccepted
            | CchOrderStatus::OutgoingInFlight
            | CchOrderStatus::OutgoingSucceeded => {
                // Ensure start incoming invoice tracking.
                actions.push(CchOrderAction::TrackIncomingInvoice);
            }
            _ => {}
        }
        actions
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
    pub async fn execute<S: CchOrderStore>(
        state: &CchState<S>,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        action: CchOrderAction,
        retry_count: u32,
    ) -> Result<()> {
        if let Some(executor) = Self::dispatch(state, cch_actor_ref, order, action, retry_count) {
            return executor.execute().await;
        }
        Ok(())
    }
}
