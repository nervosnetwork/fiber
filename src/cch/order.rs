use std::fmt;

use anyhow::anyhow;
use lightning_invoice::Bolt11Invoice;
use ractor::{call, Actor, ActorProcessingErr, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use thiserror::Error;

use crate::{
    fiber::{
        serde_utils::{U128Hex, U64Hex},
        types::Hash256,
    },
    invoice::CkbInvoice,
    store::subscription::{InvoiceState, InvoiceUpdate, PaymentState, PaymentUpdate},
};

use super::{CchError, CchMessage, CchOrderStore};

#[derive(Debug)]
pub enum StateTransitionEvent {
    #[allow(dead_code)]
    InvoiceUpdate(InvoiceState),
    #[allow(dead_code)]
    PaymentUpdate(PaymentState),
}

pub struct InvalidStateTransition {
    pub previous_in_state: InvoiceState,
    pub previous_out_state: PaymentState,
    pub state_transition_event: StateTransitionEvent,
    pub error: CchStateError,
}

impl fmt::Debug for InvalidStateTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "An error occurred when trying to make state transition from ({:?}, {:?}) by event {:?}: {}",
            self.previous_in_state, self.previous_out_state, self.state_transition_event, self.error
        )
    }
}

#[derive(Error, Debug)]
pub enum CchOrderError {
    #[error("Invalid state transition: {0:?}")]
    InvalidStateTransition(Box<InvalidStateTransition>),
    #[error("The state is inconsistent: cch order status, {0:?}, invoice state, {1:?}, payment state, {2:?}")]
    InconsistentState(CchOrderStatus, InvoiceState, PaymentState),
    #[error("Failed to pay invoice {0:?}: {1:?}")]
    FailedToPayInvoice(CchInvoice, CchError),
    #[error("Failed to settle invoice {0:?}: {1:?}")]
    FailedToSettleInvoice(CchInvoice, CchError),
}

/// The status of a cross-chain hub order, will update as the order progresses.
#[derive(Default, Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CchOrderStatus {
    /// Order is created and the first half has not received complete payment yet.
    #[default]
    Pending = 0,
    /// HTLC in the first half is accepted (the middleman has received a HTLC).
    FirstHalfAccepted = 1,
    /// There's an outgoing payment in flight for the second half (implies the first half HTLC is accepted).
    SecondHalfInFlight = 2,
    /// The second half payment has succeeded (payment settled).
    SecondHalfSucceeded = 3,
    /// The first half payment has succeeded (implies the second half payment has succeeded).
    Succeeded = 4,
    /// Order is failed.
    Failed = 5,
}

impl TryFrom<(InvoiceState, PaymentState)> for CchOrderStatus {
    type Error = CchStateError;

    fn try_from((in_state, out_state): (InvoiceState, PaymentState)) -> Result<Self, Self::Error> {
        match (in_state, out_state) {
            (InvoiceState::Cancelled | InvoiceState::Expired, _) => Ok(CchOrderStatus::Failed),
            (_, PaymentState::Failed) => Ok(CchOrderStatus::Failed),
            (InvoiceState::Open, PaymentState::Created) => Ok(CchOrderStatus::Pending),
            (InvoiceState::Open, _) => Err(format!(
                "The second payment has a state too new for a just open first payment: {:?}",
                out_state
            )),
            (
                InvoiceState::Received {
                    amount: _amount,
                    is_finished,
                },
                PaymentState::Created,
            ) => {
                if is_finished {
                    Ok(CchOrderStatus::FirstHalfAccepted)
                } else {
                    Ok(CchOrderStatus::Pending)
                }
            }
            (
                InvoiceState::Received {
                    amount: _amount,
                    is_finished,
                },
                PaymentState::Inflight,
            ) => {
                if !is_finished {
                    return Err("The second payment should be inflight when the first one is unfinished".to_string());
                }
                Ok(CchOrderStatus::SecondHalfInFlight)
            }
            (InvoiceState::Received { .. }, PaymentState::Success { .. }) => {
                Ok(CchOrderStatus::SecondHalfSucceeded)
            }
            (InvoiceState::Paid, PaymentState::Success { .. }) => Ok(CchOrderStatus::Succeeded),
            (InvoiceState::Paid, _) => Err(format!(
                "The first payment succeeded while the second payment has state (should have been succeeded or failed): {:?}",
                out_state
            )),
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CchOrder {
    // The payment hash of the order
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,
    // Seconds since epoch when the order is created
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    // Seconds after timestamp that the order expires
    #[serde_as(as = "U64Hex")]
    pub expires_after: u64,

    #[serde_as(as = "U128Hex")]
    /// Amount required to pay in Satoshis via wrapped BTC, including the fee for the cross-chain hub
    pub amount_sats: u128,
    #[serde_as(as = "U128Hex")]
    pub fee_sats: u128,

    pub in_invoice: CchInvoice,
    pub out_invoice: CchInvoice,
    pub in_state: InvoiceState,
    pub out_state: PaymentState,

    pub status: CchOrderStatus,
}

impl CchOrder {
    pub fn new(
        payment_hash: Hash256,
        created_at: u64,
        expires_after: u64,
        amount_sats: u128,
        fee_sats: u128,
        in_invoice: CchInvoice,
        out_invoice: CchInvoice,
    ) -> Self {
        Self {
            payment_hash,
            payment_preimage: Default::default(),
            created_at,
            expires_after,
            amount_sats,
            fee_sats,
            in_invoice,
            out_invoice,
            in_state: Default::default(),
            out_state: Default::default(),
            status: Default::default(),
        }
    }

    pub fn is_first_half_fiber(&self) -> bool {
        self.in_invoice.is_fiber()
    }

    pub fn is_second_half_fiber(&self) -> bool {
        self.out_invoice.is_fiber()
    }

    pub fn is_finalized(&self) -> bool {
        matches!(
            self.get_status(),
            CchOrderStatus::Failed | CchOrderStatus::Succeeded
        )
    }

    pub fn get_status(&self) -> CchOrderStatus {
        self.status
    }

    pub fn check_state_consistency(&self) -> Result<(), CchOrderError> {
        if !CchOrderStatus::try_from((self.in_state, self.out_state))
            .map_or(false, |status| status == self.get_status())
        {
            return Err(CchOrderError::InconsistentState(
                self.get_status(),
                self.in_state,
                self.out_state,
            ));
        }
        Ok(())
    }

    // Try to save the invoice update and update the state of the order.
    // If the state transition is invalid, return an error, else if the state
    // is updated, return true, else return false.
    fn try_save_invoice_update(
        &mut self,
        invoice_update: CchInvoiceUpdate,
    ) -> Result<bool, CchOrderError> {
        if self.in_invoice.is_fiber() != invoice_update.is_fiber {
            return Err(CchOrderError::InvalidStateTransition(Box::new(
                InvalidStateTransition {
                    previous_in_state: self.in_state,
                    previous_out_state: self.out_state,
                    state_transition_event: StateTransitionEvent::InvoiceUpdate(
                        invoice_update.update.state,
                    ),
                    error: "The invoice update is for the wrong network".to_string(),
                },
            )));
        }
        let new_state = invoice_update.update.state;
        if new_state == self.in_state {
            return Ok(false);
        }
        let new_status = CchOrderStatus::try_from((invoice_update.update.state, self.out_state))
            .map_err(|error| {
                CchOrderError::InvalidStateTransition(Box::new(InvalidStateTransition {
                    previous_in_state: self.in_state,
                    previous_out_state: self.out_state,
                    state_transition_event: StateTransitionEvent::InvoiceUpdate(new_state),
                    error,
                }))
            })?;
        self.status = new_status;
        self.in_state = new_state;
        Ok(true)
    }

    // Update the order state based on the invoice update.
    async fn handle_invoice_update(
        &mut self,
        myself: &ActorRef<CchOrderActorMessage>,
        cch_actor: &ActorRef<CchMessage>,
        invoice_update: CchInvoiceUpdate,
    ) -> Result<(), CchOrderError> {
        tracing::trace!(invoice_update = ?invoice_update, "Cch received invoice update");
        let is_updated = self.try_save_invoice_update(invoice_update)?;
        if !is_updated {
            return Ok(());
        }

        self.on_status_updated(myself, cch_actor).await?;

        Ok(())
    }

    // Try to save the payment update and update the state of the order.
    // If the state transition is invalid, return an error, else if the state
    // is updated, return true, else return false.
    fn try_save_payment_update(
        &mut self,
        payment_update: CchPaymentUpdate,
    ) -> Result<bool, CchOrderError> {
        tracing::trace!(payment_update = ?payment_update, "Cch received payment update");
        if self.out_invoice.is_fiber() != payment_update.is_fiber {
            return Err(CchOrderError::InvalidStateTransition(Box::new(
                InvalidStateTransition {
                    previous_in_state: self.in_state,
                    previous_out_state: self.out_state,
                    state_transition_event: StateTransitionEvent::PaymentUpdate(
                        payment_update.update.state,
                    ),
                    error: "The payment update is for the wrong network".to_string(),
                },
            )));
        }

        let new_status = CchOrderStatus::try_from((self.in_state, payment_update.update.state))
            .map_err(|error| {
                CchOrderError::InvalidStateTransition(Box::new(InvalidStateTransition {
                    previous_in_state: self.in_state,
                    previous_out_state: self.out_state,
                    state_transition_event: StateTransitionEvent::PaymentUpdate(
                        payment_update.update.state,
                    ),
                    error,
                }))
            })?;
        self.status = new_status;
        self.out_state = payment_update.update.state;
        if let PaymentState::Success { preimage } = payment_update.update.state {
            self.payment_preimage = Some(preimage);
        }
        Ok(true)
    }

    // Update the order state based on the payment update.
    async fn handle_payment_update(
        &mut self,
        myself: &ActorRef<CchOrderActorMessage>,
        cch_actor: &ActorRef<CchMessage>,
        payment_update: CchPaymentUpdate,
    ) -> Result<(), CchOrderError> {
        let is_updated = self.try_save_payment_update(payment_update)?;
        if !is_updated {
            return Ok(());
        }
        self.on_status_updated(myself, cch_actor).await?;

        Ok(())
    }

    // This function will be called when the status of the order is updated or when the order actor is started.
    // We need to make sure that this function is idempotent. E.g., if the order is in the FirstHalfAccepted state,
    // and we may call this function twice to send a payment over the second half, we need to make sure that
    // even though the function is called twice, but the payment is sent only once.
    // Making this function idempotent simplifies the state of the order. In the above example,
    // We don't have to track if the payment attempt succeeded or failed, we just need to resend the payment again.
    async fn on_status_updated(
        &mut self,
        myself: &ActorRef<CchOrderActorMessage>,
        cch_actor: &ActorRef<CchMessage>,
    ) -> Result<(), CchOrderError> {
        match self.get_status() {
            CchOrderStatus::FirstHalfAccepted => {
                if let Some(payment_update) =
                    call!(&cch_actor, CchMessage::PayInvoice, self.out_invoice.clone())
                        .expect("call cch actor")
                        .map_err(|e| {
                            CchOrderError::FailedToPayInvoice(self.out_invoice.clone(), e)
                        })?
                {
                    myself
                        .send_message(CchOrderActorMessage::PaymentUpdate(payment_update))
                        .expect("send payment update to myself");
                }
            }
            CchOrderStatus::SecondHalfSucceeded => {
                let preimage = self.payment_preimage.expect("preimage is set");
                call!(
                    cch_actor,
                    CchMessage::SettleInvoice,
                    self.in_invoice.clone(),
                    preimage
                )
                .expect("call cch actor")
                .map_err(|e| CchOrderError::FailedToSettleInvoice(self.in_invoice.clone(), e))?;
            }
            CchOrderStatus::Failed | CchOrderStatus::Succeeded => {
                cch_actor
                    .send_message(CchMessage::NotifyOrderOutCome(
                        self.payment_hash,
                        self.get_status(),
                    ))
                    .expect("send order finalized message to cch actor");
            }
            CchOrderStatus::Pending | CchOrderStatus::SecondHalfInFlight => {
                // Waiting for next invoice/payment update to arrive
            }
        }
        Ok(())
    }
}

pub type CchStateError = String;

pub type FiberInvoiceUpdate = InvoiceUpdate;
pub type FiberPaymentUpdate = PaymentUpdate;
pub type LightningInvoiceUpdate = InvoiceUpdate;
pub type LightningPaymentUpdate = PaymentUpdate;

#[derive(Debug)]
pub struct CchInvoiceUpdate {
    pub is_fiber: bool,
    pub update: InvoiceUpdate,
}

#[derive(Debug)]
pub struct CchPaymentUpdate {
    pub is_fiber: bool,
    pub update: PaymentUpdate,
}

/// A cross-chain hub invoice, which can be either a lightning network invoice or a fiber network invoice.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CchInvoice {
    /// A lightning network invoice
    Lightning(#[serde_as(as = "DisplayFromStr")] Bolt11Invoice),
    /// A fiber network invoice
    Fiber(#[serde_as(as = "DisplayFromStr")] CkbInvoice),
}

impl CchInvoice {
    pub fn is_fiber(&self) -> bool {
        matches!(self, CchInvoice::Fiber(_))
    }

    pub fn payment_hash(&self) -> Hash256 {
        match self {
            CchInvoice::Lightning(invoice) => invoice.payment_hash().into(),
            CchInvoice::Fiber(invoice) => *invoice.payment_hash(),
        }
    }
}

pub struct CchOrderActor<S> {
    pub cch_actor: ActorRef<CchMessage>,
    pub store: S,
}

impl<S> CchOrderActor<S>
where
    S: CchOrderStore + Clone + Send + Sync + 'static,
{
    pub async fn start(
        cch_actor: &ActorRef<CchMessage>,
        store: S,
        order: CchOrder,
    ) -> ActorRef<CchOrderActorMessage> {
        let actor = CchOrderActor {
            cch_actor: cch_actor.clone(),
            store,
        };
        Actor::spawn_linked(
            Some(format!("cch order actor {}", order.payment_hash)),
            actor,
            order,
            cch_actor.get_cell(),
        )
        .await
        .expect("start cch order actor")
        .0
    }
}

pub enum CchOrderActorMessage {
    InvoiceUpdate(CchInvoiceUpdate),
    PaymentUpdate(CchPaymentUpdate),
}

#[ractor::async_trait]
impl<S> Actor for CchOrderActor<S>
where
    S: CchOrderStore + Clone + Send + Sync + 'static,
{
    type Msg = CchOrderActorMessage;
    type State = CchOrder;
    type Arguments = CchOrder;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        order: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        order.check_state_consistency()?;
        tracing::debug!(payment_hash = ?order.payment_hash, status = ?order.get_status(), "Cch order started");
        let mut state = order;
        state.on_status_updated(&myself, &self.cch_actor).await?;
        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CchOrderActorMessage::InvoiceUpdate(invoice_update) => {
                if let Err(error) = state
                    .handle_invoice_update(&myself, &self.cch_actor, invoice_update)
                    .await
                {
                    tracing::error!(error = ?error, "Failed to handle invoice update");
                }
            }
            CchOrderActorMessage::PaymentUpdate(payment_update) => {
                if let Err(error) = state
                    .handle_payment_update(&myself, &self.cch_actor, payment_update)
                    .await
                {
                    tracing::error!(error = ?error, "Failed to handle payment update");
                }
            }
        }
        self.store
            .update_cch_order(state.clone())
            .expect("update cch order");
        Ok(())
    }
}

// The From and TryFrom implementations are used to convert a ActorRef<CchOrderActorMessage>
// to a DerivedActorRef<FiberPaymentUpdate>.
// https://docs.rs/ractor/latest/ractor/actor/derived_actor/struct.DerivedActorRef.html
impl From<FiberPaymentUpdate> for CchOrderActorMessage {
    fn from(update: FiberPaymentUpdate) -> Self {
        CchOrderActorMessage::PaymentUpdate(CchPaymentUpdate {
            is_fiber: true,
            update,
        })
    }
}

impl TryFrom<CchOrderActorMessage> for FiberPaymentUpdate {
    type Error = anyhow::Error;

    fn try_from(msg: CchOrderActorMessage) -> Result<Self, Self::Error> {
        match msg {
            CchOrderActorMessage::PaymentUpdate(update) if update.is_fiber => Ok(update.update),
            _ => Err(anyhow!("CchOrderActorMessage is not a fiber PaymentUpdate")),
        }
    }
}

// The From and TryFrom implementations are used to convert a ActorRef<CchOrderActorMessage>
// to a DerivedActorRef<FiberInvoiceUpdate>.
// https://docs.rs/ractor/latest/ractor/actor/derived_actor/struct.DerivedActorRef.html
impl From<FiberInvoiceUpdate> for CchOrderActorMessage {
    fn from(update: FiberInvoiceUpdate) -> Self {
        CchOrderActorMessage::InvoiceUpdate(CchInvoiceUpdate {
            is_fiber: true,
            update,
        })
    }
}

impl TryFrom<CchOrderActorMessage> for FiberInvoiceUpdate {
    type Error = anyhow::Error;

    fn try_from(msg: CchOrderActorMessage) -> Result<Self, Self::Error> {
        match msg {
            CchOrderActorMessage::InvoiceUpdate(update) if update.is_fiber => Ok(update.update),
            _ => Err(anyhow!("CchOrderActorMessage is not a fiber InvoiceUpdate")),
        }
    }
}
