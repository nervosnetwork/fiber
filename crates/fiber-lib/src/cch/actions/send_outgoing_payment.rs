use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::routerrpc;
use ractor::{forward, ActorRef};
use std::str::FromStr;

use crate::{
    cch::{
        actions::{
            backend_dispatchers::{dispatch_payment_handler, PaymentHandlerType},
            ActionExecutor, CchOrderAction,
        },
        actor::CchState,
        order::CchInvoice,
        trackers::{map_lnd_payment_changed_event, CchTrackingEvent, LndConnectionInfo},
        CchMessage, CchOrder, CchOrderStatus, CchOrderStore,
    },
    fiber::{
        config::MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        payment::{PaymentStatus, SendPaymentCommand},
        types::Hash256,
        NetworkActorCommand, NetworkActorMessage,
    },
    invoice::CkbInvoice,
    time::{SystemTime, UNIX_EPOCH},
};

const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;

pub struct SendOutgoingPaymentDispatcher;

pub struct SendFiberOutgoingPaymentExecutor {
    payment_hash: Hash256,
    cch_actor_ref: ActorRef<CchMessage>,
    network_actor_ref: ActorRef<NetworkActorMessage>,
    outgoing_pay_req: String,
    retry_count: u32,
    /// Maximum TLC expiry for the entire payment route (in milliseconds).
    /// This caps the route to prevent the outgoing payment from exceeding
    /// the incoming payment's remaining expiry time.
    tlc_expiry_limit: u64,
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
            tlc_expiry_limit,
        } = *self;

        let message = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    invoice: Some(outgoing_pay_req),
                    tlc_expiry_limit: Some(tlc_expiry_limit),
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
        // TODO: replace string matching with structured error codes from NetworkActor.
        err.contains("InvalidParameter")
    }
}

pub struct SendLightningOutgoingPaymentExecutor {
    payment_hash: Hash256,
    cch_actor_ref: ActorRef<CchMessage>,
    outgoing_pay_req: String,
    lnd_connection: LndConnectionInfo,
    /// Maximum total CLTV delta for the payment route (in blocks).
    /// This caps the route to prevent the outgoing payment from exceeding
    /// the incoming payment's remaining expiry time.
    cltv_limit: i32,
}

#[async_trait::async_trait]
impl ActionExecutor for SendLightningOutgoingPaymentExecutor {
    async fn execute(self: Box<Self>) -> Result<()> {
        let req = routerrpc::SendPaymentRequest {
            payment_request: self.outgoing_pay_req,
            timeout_seconds: BTC_PAYMENT_TIMEOUT_SECONDS,
            cltv_limit: self.cltv_limit,
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
        matches!(status.code(), tonic::Code::InvalidArgument)
    }
}

impl SendOutgoingPaymentDispatcher {
    pub fn should_dispatch(order: &CchOrder) -> bool {
        order.status == CchOrderStatus::IncomingAccepted
    }

    /// Compute the maximum allowed outgoing payment route expiry (in seconds).
    ///
    /// The incoming TLC/HTLC has a guaranteed minimum remaining time of:
    ///   `incoming_final_expiry_delta - elapsed_since_order_creation`
    ///
    /// We allow at most half of this remaining time for the outgoing payment route.
    /// The other half is reserved for the CCH to settle the incoming payment
    /// after receiving the preimage.
    ///
    /// Returns `None` if there is insufficient time remaining.
    fn compute_max_outgoing_expiry_seconds<S: CchOrderStore>(
        state: &CchState<S>,
        order: &CchOrder,
    ) -> Option<u64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time should always be after UNIX_EPOCH")
            .as_secs();
        let elapsed = now.saturating_sub(order.created_at);

        // The incoming TLC/HTLC was accepted with at least this many seconds of expiry.
        // Using `created_at` is conservative (the TLC was accepted after order creation).
        let incoming_expiry_seconds = match &order.incoming_invoice {
            CchInvoice::Fiber(_) => state.config.ckb_final_tlc_expiry_delta_seconds,
            CchInvoice::Lightning(_) => state.config.btc_final_tlc_expiry_delta_blocks * 600,
        };
        let remaining = incoming_expiry_seconds.checked_sub(elapsed)?;

        // Use half the remaining time for outgoing, half reserved for settling incoming
        Some(remaining / 2)
    }

    /// Check whether there is sufficient time remaining on the incoming payment
    /// to safely send the outgoing payment. If not, fail the order.
    ///
    /// Returns `Some(max_outgoing_seconds)` if safe, `None` if the order was failed.
    fn check_expiry_or_fail(
        cch_actor_ref: &ActorRef<CchMessage>,
        order: &CchOrder,
        max_outgoing_seconds: Option<u64>,
    ) -> Option<u64> {
        let max_outgoing_seconds = match max_outgoing_seconds {
            Some(s) if s > 0 => s,
            _ => {
                let _ = cch_actor_ref.send_message(CchMessage::TrackingEvent(
                    CchTrackingEvent::PaymentChanged {
                        payment_hash: order.payment_hash,
                        payment_preimage: None,
                        status: PaymentStatus::Failed,
                        failure_reason: Some(
                            "Insufficient HTLC expiry delta: incoming payment has expired or \
                             has no remaining time for outgoing payment"
                                .into(),
                        ),
                    },
                ));
                return None;
            }
        };

        // Verify the max outgoing expiry can accommodate the outgoing invoice's
        // minimum final expiry delta (otherwise routing is impossible).
        let outgoing_min_seconds = match &order.incoming_invoice {
            CchInvoice::Fiber(_) => {
                // Outgoing is BTC Lightning: parse the BTC invoice's min_final_cltv_expiry_delta
                Bolt11Invoice::from_str(&order.outgoing_pay_req)
                    .ok()
                    .map(|inv| inv.min_final_cltv_expiry_delta() * 600)
                    .unwrap_or(0)
            }
            CchInvoice::Lightning(_) => {
                // Outgoing is CKB Fiber: parse the CKB invoice's final_tlc_minimum_expiry_delta
                CkbInvoice::from_str(&order.outgoing_pay_req)
                    .ok()
                    .and_then(|inv| inv.final_tlc_minimum_expiry_delta().copied())
                    .map(|millis| millis / 1000)
                    .unwrap_or(0)
            }
        };

        if max_outgoing_seconds < outgoing_min_seconds {
            let _ = cch_actor_ref.send_message(CchMessage::TrackingEvent(
                CchTrackingEvent::PaymentChanged {
                    payment_hash: order.payment_hash,
                    payment_preimage: None,
                    status: PaymentStatus::Failed,
                    failure_reason: Some(format!(
                        "Insufficient HTLC expiry delta: max outgoing route expiry ({} seconds) \
                         is less than the outgoing invoice's minimum final expiry ({} seconds). \
                         Not enough time remaining on the incoming payment to safely \
                         handle outgoing payment settlement.",
                        max_outgoing_seconds, outgoing_min_seconds
                    )),
                },
            ));
            return None;
        }

        Some(max_outgoing_seconds)
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

        // Check the remaining incoming time and compute the max outgoing route expiry.
        // This ensures the CCH has enough time to settle the incoming payment
        // even in the worst case where the outgoing payment settles at the last moment.
        let max_outgoing_seconds = Self::compute_max_outgoing_expiry_seconds(state, order);
        let max_outgoing_seconds =
            Self::check_expiry_or_fail(cch_actor_ref, order, max_outgoing_seconds)?;

        match dispatch_payment_handler(order) {
            PaymentHandlerType::Fiber => {
                let tlc_expiry_limit = max_outgoing_seconds
                    .saturating_mul(1000)
                    .min(MAX_PAYMENT_TLC_EXPIRY_LIMIT);
                Some(Box::new(SendFiberOutgoingPaymentExecutor {
                    payment_hash: order.payment_hash,
                    cch_actor_ref: cch_actor_ref.clone(),
                    network_actor_ref: state.network_actor.clone(),
                    outgoing_pay_req: order.outgoing_pay_req.clone(),
                    retry_count,
                    tlc_expiry_limit,
                }))
            }
            PaymentHandlerType::Lightning => {
                let cltv_limit = (max_outgoing_seconds / 600) as i32;
                Some(Box::new(SendLightningOutgoingPaymentExecutor {
                    payment_hash: order.payment_hash,
                    cch_actor_ref: cch_actor_ref.clone(),
                    outgoing_pay_req: order.outgoing_pay_req.clone(),
                    lnd_connection: state.lnd_connection.clone(),
                    cltv_limit,
                }))
            }
        }
    }
}
