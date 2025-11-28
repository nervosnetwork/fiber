use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use lnd_grpc_tonic_client::routerrpc;
use ractor::{call, ActorRef};

use crate::{
    cch::{
        actions::{
            backend_dispatchers::{dispatch_payment_handler, PaymentHandlerType},
            ActionExecutor,
        },
        actor::CchState,
        trackers::{map_lnd_payment_changed_event, CchTrackingEvent, LndConnectionInfo},
        CchMessage, CchOrder, CchOrderStatus,
    },
    fiber::{
        payment::SendPaymentCommand, NetworkActorCommand, NetworkActorMessage,
        ASSUME_NETWORK_ACTOR_ALIVE,
    },
};

const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;

pub struct SendOutgoingPaymentDispatcher;

pub struct SendFiberOutgoingPaymentExecutor {
    cch_actor_ref: ActorRef<CchMessage>,
    network_actor_ref: ActorRef<NetworkActorMessage>,
    outgoing_pay_req: String,
}

#[async_trait::async_trait]
impl ActionExecutor for SendFiberOutgoingPaymentExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        let outgoing_pay_req = self.outgoing_pay_req;
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    invoice: Some(outgoing_pay_req),
                    ..Default::default()
                },
                rpc_reply,
            ))
        };

        let payment = call!(self.network_actor_ref, message)
            .expect(ASSUME_NETWORK_ACTOR_ALIVE)
            .map_err(|err| anyhow!("{}", err))?;

        self.cch_actor_ref.send_message(CchMessage::TrackingEvent(
            CchTrackingEvent::PaymentChanged {
                payment_hash: payment.payment_hash,
                payment_preimage: None,
                status: payment.status,
            },
        ))?;

        Ok(())
    }
}

pub struct SendLightningOutgoingPaymentExecutor {
    cch_actor_ref: ActorRef<CchMessage>,
    outgoing_pay_req: String,
    lnd_connection: LndConnectionInfo,
}

#[async_trait::async_trait]
impl ActionExecutor for SendLightningOutgoingPaymentExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        let req = routerrpc::SendPaymentRequest {
            payment_request: self.outgoing_pay_req,
            timeout_seconds: BTC_PAYMENT_TIMEOUT_SECONDS,
            ..Default::default()
        };
        tracing::debug!("SendLightningOutgoingPaymentExecutor req: {:?}", req);

        let mut client = self.lnd_connection.create_router_client().await?;
        // TODO: set a fee
        let mut stream = client.send_payment_v2(req).await?.into_inner();
        // Wait for the first message then quit
        let payment_result_opt = stream.next().await;
        tracing::debug!(
            "SendLightningOutgoingPaymentExecutor resp: {:?}",
            payment_result_opt
        );
        if let Some(Ok(payment)) = payment_result_opt {
            self.cch_actor_ref.send_message(CchMessage::TrackingEvent(
                map_lnd_payment_changed_event(payment)?,
            ))?;
        }
        Ok(())
    }
}

impl SendOutgoingPaymentDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        order.status == CchOrderStatus::IncomingAccepted
    }

    pub fn dispatch(
        state: &mut CchState,
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
    ) -> Option<Box<dyn ActionExecutor>> {
        if !Self::should_dispatch(order) {
            return None;
        }

        match dispatch_payment_handler(order) {
            PaymentHandlerType::Fiber => Some(Box::new(SendFiberOutgoingPaymentExecutor {
                cch_actor_ref: cch_actor_ref.clone(),
                network_actor_ref: state.network_actor.clone(),
                outgoing_pay_req: order.outgoing_pay_req.clone(),
            })),
            PaymentHandlerType::Lightning => Some(Box::new(SendLightningOutgoingPaymentExecutor {
                cch_actor_ref: cch_actor_ref.clone(),
                outgoing_pay_req: order.outgoing_pay_req.clone(),
                lnd_connection: state.lnd_connection.clone(),
            })),
        }
    }
}
