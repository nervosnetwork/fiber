use anyhow::{anyhow, Result};
use lnd_grpc_tonic_client::invoicesrpc;
use ractor::{call, ActorRef};

use crate::{
    cch::{
        actions::{
            backend_dispatchers::{dispatch_invoice_handler, InvoiceHandlerType},
            ActionExecutor,
        },
        actor::CchState,
        trackers::{CchTrackingEvent, LndConnectionInfo},
        CchMessage, CchOrderStore,
    },
    fiber::{NetworkActorCommand, NetworkActorMessage, ASSUME_NETWORK_ACTOR_ALIVE},
    invoice::{CkbInvoiceStatus, SettleInvoiceError},
};
use fiber_types::{CchOrder, CchOrderStatus, Hash256};

pub struct SettleIncomingInvoiceDispatcher;

pub struct SettleFiberIncomingInvoiceExecutor {
    payment_hash: Hash256,
    payment_preimage: Hash256,
    cch_actor_ref: ActorRef<CchMessage>,
    network_actor_ref: ActorRef<NetworkActorMessage>,
}

#[async_trait::async_trait]
impl ActionExecutor for SettleFiberIncomingInvoiceExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        let payment_hash = self.payment_hash;
        let payment_preimage = self.payment_preimage;
        let command = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                payment_hash,
                payment_preimage,
                rpc_reply,
            ))
        };
        if let Err(err) = call!(self.network_actor_ref, command).expect(ASSUME_NETWORK_ACTOR_ALIVE)
        {
            let failure_reason = format!("SettleFiberIncomingInvoiceExecutor failure: {:?}", err);
            if Self::is_permanent_error(err) {
                self.cch_actor_ref.send_message(CchMessage::TrackingEvent(
                    CchTrackingEvent::InvoiceChanged {
                        payment_hash,
                        status: CkbInvoiceStatus::Cancelled,
                        failure_reason: Some(failure_reason),
                    },
                ))?;
            } else {
                return Err(anyhow!(failure_reason));
            }
        }
        Ok(())
    }
}

impl SettleFiberIncomingInvoiceExecutor {
    fn is_permanent_error(err: SettleInvoiceError) -> bool {
        matches!(
            err,
            SettleInvoiceError::InvoiceNotFound
                | SettleInvoiceError::HashMismatch
                | SettleInvoiceError::InvoiceStillOpen
                | SettleInvoiceError::InvoiceAlreadyCancelled
                | SettleInvoiceError::InvoiceAlreadyExpired
                | SettleInvoiceError::InvoiceAlreadyPaid
        )
    }
}

pub struct SettleLightningIncomingInvoiceExecutor {
    payment_hash: Hash256,
    payment_preimage: Hash256,
    lnd_connection: LndConnectionInfo,
    cch_actor_ref: ActorRef<CchMessage>,
}

#[async_trait::async_trait]
impl ActionExecutor for SettleLightningIncomingInvoiceExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        // settle the lnd invoice
        let req = invoicesrpc::SettleInvoiceMsg {
            preimage: self.payment_preimage.into(),
        };
        tracing::debug!("SettleLightningIncomingInvoiceExecutor req: {:?}", req);

        let mut client = self.lnd_connection.create_invoices_client().await?;
        // TODO: set a fee
        match client.settle_invoice(req).await {
            Ok(resp) => {
                let resp = resp.into_inner();
                tracing::debug!("SettleLightningIncomingInvoiceExecutor resp: {:?}", resp);
                Ok(())
            }
            Err(err) => {
                let failure_reason =
                    format!("SettleLightningIncomingInvoiceExecutor error: {:?}", err);
                if Self::is_permanent_error(err) {
                    self.cch_actor_ref.send_message(CchMessage::TrackingEvent(
                        CchTrackingEvent::InvoiceChanged {
                            payment_hash: self.payment_hash,
                            status: CkbInvoiceStatus::Cancelled,
                            failure_reason: Some(failure_reason),
                        },
                    ))?;
                    Ok(())
                } else {
                    Err(anyhow!(failure_reason))
                }
            }
        }
    }
}

impl SettleLightningIncomingInvoiceExecutor {
    fn is_permanent_error(status: tonic::Status) -> bool {
        status.code() == tonic::Code::InvalidArgument
    }
}

impl SettleIncomingInvoiceDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        order.status == CchOrderStatus::OutgoingSucceeded
    }

    pub fn dispatch<S: CchOrderStore>(
        state: &CchState<S>,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        _retry_count: u32,
    ) -> Option<Box<dyn ActionExecutor>> {
        if !Self::should_dispatch(order) {
            return None;
        }
        let payment_preimage = order.payment_preimage?;

        match dispatch_invoice_handler(order) {
            // `CchActor` will track all fiber invoices, so there's nothing to do here to track a single invoice.
            InvoiceHandlerType::Fiber => Some(Box::new(SettleFiberIncomingInvoiceExecutor {
                payment_preimage,
                payment_hash: order.payment_hash,
                network_actor_ref: state.network_actor.clone(),
                cch_actor_ref: cch_actor_ref.clone(),
            })),
            InvoiceHandlerType::Lightning => {
                Some(Box::new(SettleLightningIncomingInvoiceExecutor {
                    payment_preimage,
                    payment_hash: order.payment_hash,
                    lnd_connection: state.lnd_connection.clone(),
                    cch_actor_ref: cch_actor_ref.clone(),
                }))
            }
        }
    }
}
