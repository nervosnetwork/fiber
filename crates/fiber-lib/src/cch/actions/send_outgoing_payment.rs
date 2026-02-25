use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use lnd_grpc_tonic_client::routerrpc;
use ractor::{forward, ActorRef};

use crate::{
    cch::{
        actions::{
            backend_dispatchers::{dispatch_payment_handler, PaymentHandlerType},
            ActionExecutor, CchOrderAction,
        },
        actor::CchState,
        trackers::{map_lnd_payment_changed_event, CchTrackingEvent, LndConnectionInfo},
        CchMessage, CchOrder, CchOrderStatus, CchOrderStore,
    },
    fiber::{
        payment::{PaymentStatus, SendPaymentCommand},
        types::Hash256,
        NetworkActorCommand, NetworkActorMessage,
    },
};

const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;

pub struct SendOutgoingPaymentDispatcher;

pub struct SendFiberOutgoingPaymentExecutor {
    payment_hash: Hash256,
    cch_actor_ref: ActorRef<CchMessage>,
    network_actor_ref: ActorRef<NetworkActorMessage>,
    outgoing_pay_req: String,
    retry_count: u32,
}

#[async_trait::async_trait]
impl ActionExecutor for SendFiberOutgoingPaymentExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        let Self {
            payment_hash,
            cch_actor_ref,
            network_actor_ref,
            outgoing_pay_req,
            retry_count,
        } = *self;

        let message = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    invoice: Some(outgoing_pay_req),
                    ..Default::default()
                },
                rpc_reply,
            ))
        };

        forward!(network_actor_ref, message, cch_actor_ref, move |result| {
            match result {
                Ok(payment) => CchMessage::TrackingEvent(CchTrackingEvent::PaymentChanged {
                    payment_hash: payment.payment_hash,
                    payment_preimage: None,
                    status: payment.status,
                    failure_reason: None,
                }),
                // TODO: replace string match with structured error type from NetworkActor.
                Err(err) if err.contains("Payment session already exists") => {
                    CchMessage::TrackingEvent(CchTrackingEvent::PaymentChanged {
                        payment_hash,
                        payment_preimage: None,
                        status: PaymentStatus::Inflight,
                        failure_reason: None,
                    })
                }
                Err(err) => {
                    let failure_reason =
                        format!("SendFiberOutgoingPaymentExecutor failure: {:?}", err);
                    if Self::is_permanent_error(&err) {
                        CchMessage::TrackingEvent(CchTrackingEvent::PaymentChanged {
                            payment_hash,
                            payment_preimage: None,
                            status: PaymentStatus::Failed,
                            failure_reason: Some(failure_reason),
                        })
                    } else {
                        CchMessage::ActionRetry {
                            payment_hash,
                            action: CchOrderAction::SendOutgoingPayment,
                            retry_count,
                            reason: failure_reason,
                        }
                    }
                }
            }
        })
        .map_err(|err| anyhow!(err.to_string()))?;

        Ok(())
    }
}

impl SendFiberOutgoingPaymentExecutor {
    fn is_permanent_error(err: &str) -> bool {
        // InvalidParameter errors are permanent validation errors
        if err.contains("InvalidParameter") {
            return true;
        }

        // Additional permanent errors that won't be fixed by retrying
        let err_lower = err.to_lowercase();
        err_lower.contains("invalid payment request")
            || err_lower.contains("invoice expired")
            || err_lower.contains("payment hash mismatch")
    }
}

pub struct SendLightningOutgoingPaymentExecutor {
    payment_hash: Hash256,
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
        let event = match payment_result_opt {
            Some(Ok(payment)) => map_lnd_payment_changed_event(payment)?,
            Some(Err(err)) if err.code() == tonic::Code::AlreadyExists => {
                CchTrackingEvent::PaymentChanged {
                    payment_hash: self.payment_hash,
                    payment_preimage: None,
                    status: PaymentStatus::Inflight,
                    failure_reason: None,
                }
            }
            Some(Err(err)) => {
                let failure_reason =
                    format!("SendLightningOutgoingPaymentExecutor failure: {:?}", err);
                if Self::is_permanent_error(err) {
                    CchTrackingEvent::PaymentChanged {
                        payment_hash: self.payment_hash,
                        payment_preimage: None,
                        status: PaymentStatus::Failed,
                        failure_reason: Some(failure_reason),
                    }
                } else {
                    return Err(anyhow!(failure_reason));
                }
            }
            None => {
                return Err(anyhow!(
                    "SendLightningOutgoingPaymentExecutor failed to get payment result because stream is closed"
                ));
            }
        };
        self.cch_actor_ref
            .send_message(CchMessage::TrackingEvent(event))?;
        Ok(())
    }
}

impl SendLightningOutgoingPaymentExecutor {
    fn is_permanent_error(status: tonic::Status) -> bool {
        // Check for explicit invalid argument errors
        if matches!(status.code(), tonic::Code::InvalidArgument) {
            return true;
        }

        // LND often returns Unknown status for validation errors that are permanent.
        // Check the error message to identify these cases.
        if matches!(status.code(), tonic::Code::Unknown) {
            let msg = status.message().to_lowercase();
            // These are validation/policy errors that won't be fixed by retrying
            return msg.contains("self-payments not allowed")
                || msg.contains("invoice is already paid")
                || msg.contains("invoice expired")
                || msg.contains("incorrect payment amount")
                || msg.contains("payment hash mismatch");
        }

        false
    }
}

impl SendOutgoingPaymentDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        order.status == CchOrderStatus::IncomingAccepted
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
            PaymentHandlerType::Fiber => Some(Box::new(SendFiberOutgoingPaymentExecutor {
                payment_hash: order.payment_hash,
                cch_actor_ref: cch_actor_ref.clone(),
                network_actor_ref: state.network_actor.clone(),
                outgoing_pay_req: order.outgoing_pay_req.clone(),
                retry_count,
            })),
            PaymentHandlerType::Lightning => Some(Box::new(SendLightningOutgoingPaymentExecutor {
                payment_hash: order.payment_hash,
                cch_actor_ref: cch_actor_ref.clone(),
                outgoing_pay_req: order.outgoing_pay_req.clone(),
                lnd_connection: state.lnd_connection.clone(),
            })),
        }
    }
}
