use anyhow::Result;
use lnd_grpc_tonic_client::invoicesrpc;
use ractor::{call, ActorRef};

use crate::{
    cch::{
        actions::{
            backend_dispatchers::{dispatch_invoice_handler, InvoiceHandlerType},
            ActionExecutor,
        },
        actor::CchState,
        trackers::LndConnectionInfo,
        CchMessage, CchOrder, CchOrderStatus,
    },
    fiber::{types::Hash256, NetworkActorCommand, NetworkActorMessage, ASSUME_NETWORK_ACTOR_ALIVE},
};

pub struct SettleIncomingInvoiceDispatcher;

pub struct SettleFiberIncomingInvoiceExecutor {
    payment_hash: Hash256,
    payment_preimage: Hash256,
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
        call!(self.network_actor_ref, command).expect(ASSUME_NETWORK_ACTOR_ALIVE)?;
        Ok(())
    }
}

pub struct SettleLightningIncomingInvoiceExecutor {
    payment_preimage: Hash256,
    lnd_connection: LndConnectionInfo,
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
        let resp = client.settle_invoice(req).await?.into_inner();
        tracing::debug!("SettleLightningIncomingInvoiceExecutor resp: {:?}", resp);
        Ok(())
    }
}

impl SettleIncomingInvoiceDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        order.status == CchOrderStatus::OutgoingSucceeded
    }

    pub fn dispatch(
        state: &mut CchState,
        _cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
    ) -> Option<Box<dyn ActionExecutor>> {
        if !Self::should_dispatch(order) {
            return None;
        }
        let payment_preimage = order.payment_preimage?;

        match dispatch_invoice_handler(order) {
            // `CchActor` will track all fiber invoices, so there's nothing to do here to track a single invoice.
            InvoiceHandlerType::Fiber => Some(Box::new(SettleFiberIncomingInvoiceExecutor {
                payment_hash: order.payment_hash,
                payment_preimage,
                network_actor_ref: state.network_actor.clone(),
            })),
            InvoiceHandlerType::Lightning => {
                Some(Box::new(SettleLightningIncomingInvoiceExecutor {
                    payment_preimage,
                    lnd_connection: state.lnd_connection.clone(),
                }))
            }
        }
    }
}
