use anyhow::{anyhow, Result};
use async_trait::async_trait;
use jsonrpsee::{
    core::client::ClientT as _,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use ractor::{call, forward, Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use std::str::FromStr as _;

use fiber_types::payment::PaymentStatus;
use fiber_types::Hash256;

use crate::cch::actions::CchOrderAction;
use crate::cch::actor::CchMessage;
use crate::cch::trackers::CchTrackingEvent;
use crate::cch::CchError;
use crate::fiber::{
    network::SendPaymentResponse, payment::SendPaymentCommand, NetworkActorCommand,
    NetworkActorMessage, ASSUME_NETWORK_ACTOR_ALIVE,
};
use crate::invoice::CkbInvoice;
use crate::rpc::{
    invoice::{InvoiceResult, NewInvoiceParams, SettleInvoiceParams, SettleInvoiceResult},
    payment::{GetPaymentCommandParams, GetPaymentCommandResult, SendPaymentCommandParams},
};

/// Fee limits for a CCH outgoing payment. `max_fee_amount` is the order fee budget; `max_fee_rate`
/// is derived from it and the payment amount so Fiber's default rate cap does not clamp the fee
/// below the budget.
#[derive(Clone, Copy)]
pub struct OutgoingFeeLimit {
    pub max_fee_amount: u128,
    pub max_fee_rate: u64,
}

/// Messages for the fiber agent actor. Each variant carries an RpcReplyPort for the response.
pub enum CchFiberAgentMessage {
    AddInvoice(CkbInvoice, RpcReplyPort<Result<CkbInvoice>>),
    SendPayment(
        String,
        Option<u64>,
        Option<u128>,
        Option<u64>,
        RpcReplyPort<Result<PaymentStatus>>,
    ),
    GetPayment(Hash256, RpcReplyPort<Result<GetPaymentCommandResult>>),
    PreflightPayment(
        String,
        Option<u64>,
        Option<u128>,
        Option<u64>,
        RpcReplyPort<Result<PaymentStatus>>,
    ),
    SettleInvoice(Hash256, Hash256, RpcReplyPort<Result<()>>),
}

/// Http-only backend state for the fiber agent actor.
/// The actor is only created when CCH connects via fiber_rpc_url (no in-process network actor).
#[derive(Clone)]
pub struct CchFiberAgentHttpBackend {
    pub client: HttpClient,
}

/// Actor that runs fiber agent operations over HTTP (add_invoice, send_payment, settle_invoice).
/// Created only for the Http backend; in-process mode uses the network actor directly via
/// [CchFiberAgentRef::InProcess].
pub struct CchFiberAgentActor;

#[async_trait]
impl Actor for CchFiberAgentActor {
    type Msg = CchFiberAgentMessage;
    type State = CchFiberAgentHttpBackend;
    type Arguments = CchFiberAgentHttpBackend;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CchFiberAgentMessage::AddInvoice(invoice, port) => {
                let result = state.add_invoice(invoice).await;
                let _ = port.send(result);
            }
            CchFiberAgentMessage::SendPayment(
                pay_req,
                tlc_expiry_limit,
                max_fee_amount,
                max_fee_rate,
                port,
            ) => {
                let result = state
                    .send_payment(pay_req, tlc_expiry_limit, max_fee_amount, max_fee_rate)
                    .await;
                let _ = port.send(result);
            }
            CchFiberAgentMessage::GetPayment(payment_hash, port) => {
                let result = state.get_payment(payment_hash).await;
                let _ = port.send(result);
            }
            CchFiberAgentMessage::PreflightPayment(
                pay_req,
                tlc_expiry_limit,
                max_fee_amount,
                max_fee_rate,
                port,
            ) => {
                let result = state
                    .send_payment_with_dry_run(
                        pay_req,
                        tlc_expiry_limit,
                        max_fee_amount,
                        max_fee_rate,
                        true,
                    )
                    .await;
                let _ = port.send(result);
            }
            CchFiberAgentMessage::SettleInvoice(payment_hash, payment_preimage, port) => {
                let result = state.settle_invoice(payment_hash, payment_preimage).await;
                let _ = port.send(result);
            }
        }
        Ok(())
    }
}

impl CchFiberAgentHttpBackend {
    /// Build the Http backend from a Fiber RPC URL. Used when spawning [CchFiberAgentActor].
    pub fn try_new(url: &str) -> Result<Self> {
        let client = HttpClientBuilder::default().build(url)?;
        Ok(Self { client })
    }

    pub async fn add_invoice(&self, invoice: CkbInvoice) -> Result<CkbInvoice> {
        let invoice_params = NewInvoiceParams {
            amount: invoice
                .amount
                .ok_or_else(|| anyhow!("cch invoice requires amount"))?,
            payment_hash: Some((*invoice.payment_hash()).into()),
            hash_algorithm: invoice.hash_algorithm().copied().map(Into::into),
            expiry: invoice.expiry_time().map(|duration| duration.as_secs()),
            final_expiry_delta: invoice.final_tlc_minimum_expiry_delta().copied(),
            udt_type_script: invoice
                .udt_type_script()
                .map(|script| script.clone().into()),
            description: invoice.description().cloned(),
            currency: invoice.currency.into(),
            payment_preimage: None,
            fallback_address: invoice.fallback_address().cloned(),
            allow_mpp: Some(invoice.allow_mpp()),
            ..Default::default()
        };
        let response = self
            .client
            .request::<InvoiceResult, _>("new_invoice", rpc_params![invoice_params])
            .await?;
        CkbInvoice::from_str(&response.invoice_address).map_err(Into::into)
    }

    pub async fn send_payment(
        &self,
        pay_req: String,
        tlc_expiry_limit: Option<u64>,
        max_fee_amount: Option<u128>,
        max_fee_rate: Option<u64>,
    ) -> Result<PaymentStatus> {
        self.send_payment_with_dry_run(
            pay_req,
            tlc_expiry_limit,
            max_fee_amount,
            max_fee_rate,
            false,
        )
        .await
    }

    async fn send_payment_with_dry_run(
        &self,
        pay_req: String,
        tlc_expiry_limit: Option<u64>,
        max_fee_amount: Option<u128>,
        max_fee_rate: Option<u64>,
        dry_run: bool,
    ) -> Result<PaymentStatus> {
        let payment_params = SendPaymentCommandParams {
            target_pubkey: None,
            amount: None,
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit,
            invoice: Some(pay_req),
            timeout: None,
            max_fee_amount,
            max_fee_rate,
            max_parts: None,
            trampoline_hops: None,
            keysend: None,
            udt_type_script: None,
            allow_self_payment: None,
            custom_records: None,
            hop_hints: None,
            dry_run: Some(dry_run),
        };
        let response = self
            .client
            .request::<GetPaymentCommandResult, _>("send_payment", rpc_params![payment_params])
            .await?;
        Ok(response.status.into())
    }

    pub async fn settle_invoice(
        &self,
        payment_hash: Hash256,
        payment_preimage: Hash256,
    ) -> Result<()> {
        let settle_invoice_params = SettleInvoiceParams {
            payment_hash: payment_hash.into(),
            payment_preimage: payment_preimage.into(),
        };
        self.client
            .request::<SettleInvoiceResult, _>("settle_invoice", rpc_params![settle_invoice_params])
            .await?;
        Ok(())
    }

    pub async fn get_payment(&self, payment_hash: Hash256) -> Result<GetPaymentCommandResult> {
        let params = GetPaymentCommandParams {
            payment_hash: payment_hash.into(),
        };
        Ok(self
            .client
            .request("get_payment", rpc_params![params])
            .await?)
    }
}

/// In-process fiber agent: talks to the local [NetworkActor]. Used when CCH runs alongside Fiber.
/// For the Http backend (separate service), use [CchFiberAgentActor] with [CchFiberAgentHttpBackend].
#[derive(Clone)]
pub enum CchFiberAgent {
    InProcess {
        network_actor: ActorRef<NetworkActorMessage>,
    },
}

impl CchFiberAgent {
    pub fn try_new(
        network_actor: Option<ActorRef<NetworkActorMessage>>,
        _fiber_rpc_url: Option<&str>,
    ) -> Result<Self> {
        match network_actor {
            Some(network_actor) => Ok(CchFiberAgent::InProcess { network_actor }),
            None => Err(anyhow!("in-process agent requires network actor")),
        }
    }

    /// Add an invoice (in-process only).
    pub async fn add_invoice(&self, invoice: CkbInvoice) -> Result<CkbInvoice> {
        let Self::InProcess { network_actor } = self;
        let invoice_cloned = invoice.clone();
        let message = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::AddInvoice(
                invoice.clone(),
                None,
                rpc_reply,
            ))
        };
        call!(network_actor, message).expect(ASSUME_NETWORK_ACTOR_ALIVE)?;
        Ok(invoice_cloned)
    }

    pub async fn send_payment(
        &self,
        pay_req: String,
        tlc_expiry_limit: Option<u64>,
        max_fee_amount: Option<u128>,
        max_fee_rate: Option<u64>,
    ) -> Result<PaymentStatus> {
        let Self::InProcess { network_actor } = self;
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    invoice: Some(pay_req),
                    tlc_expiry_limit,
                    max_fee_amount,
                    max_fee_rate,
                    ..Default::default()
                },
                rpc_reply,
            ))
        };
        Ok(call!(network_actor, message)
            .expect(ASSUME_NETWORK_ACTOR_ALIVE)
            .map_err(|err| anyhow!("{}", err))?
            .status)
    }

    pub async fn settle_invoice(
        &self,
        payment_hash: Hash256,
        payment_preimage: Hash256,
    ) -> Result<()> {
        let Self::InProcess { network_actor } = self;
        let command = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                payment_hash,
                payment_preimage,
                rpc_reply,
            ))
        };
        call!(network_actor, command).expect(ASSUME_NETWORK_ACTOR_ALIVE)?;
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Uniform derived-actor-ref interface for both backends
// -----------------------------------------------------------------------------

/// Uniform interface to send fiber operations: either to the in-process network actor
/// or to the RPC proxy actor (only created for the Http backend via [CchFiberAgentActor]).
#[derive(Clone)]
pub enum CchFiberAgentRef {
    InProcess(ActorRef<NetworkActorMessage>),
    Rpc(ActorRef<CchFiberAgentMessage>),
}

fn to_fiber_err(e: impl std::fmt::Display) -> CchError {
    CchError::FiberNodeError(anyhow::anyhow!("{}", e))
}

impl CchFiberAgentRef {
    /// Add invoice; returns the (possibly updated) invoice.
    pub async fn call_add_invoice(&self, invoice: CkbInvoice) -> Result<CkbInvoice, CchError> {
        match self {
            Self::InProcess(network_actor) => {
                let invoice_cloned = invoice.clone();
                let msg = move |tx: RpcReplyPort<Result<(), crate::invoice::InvoiceError>>| {
                    NetworkActorMessage::Command(NetworkActorCommand::AddInvoice(
                        invoice.clone(),
                        None,
                        tx,
                    ))
                };
                call!(network_actor, msg)
                    .map_err(to_fiber_err)?
                    .map_err(to_fiber_err)?;
                Ok(invoice_cloned)
            }
            Self::Rpc(rpc_actor) => call!(rpc_actor, |port| {
                CchFiberAgentMessage::AddInvoice(invoice.clone(), port)
            })
            .map_err(to_fiber_err)?
            .map_err(CchError::FiberNodeError),
        }
    }

    /// Verify that Fiber can currently build a route for an outgoing payment without creating a
    /// payment session or sending a TLC.
    pub async fn call_payment_preflight(
        &self,
        outgoing_pay_req: String,
        tlc_expiry_limit: u64,
        fee_limit: OutgoingFeeLimit,
    ) -> Result<(), CchError> {
        let tlc_limit = Some(tlc_expiry_limit);
        let max_fee = Some(fee_limit.max_fee_amount);
        let max_fee_rate = Some(fee_limit.max_fee_rate);
        match self {
            Self::InProcess(network_actor) => {
                let msg = move |tx| {
                    NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                        SendPaymentCommand {
                            invoice: Some(outgoing_pay_req.clone()),
                            tlc_expiry_limit: tlc_limit,
                            max_fee_amount: max_fee,
                            max_fee_rate,
                            dry_run: true,
                            ..Default::default()
                        },
                        tx,
                    ))
                };
                call!(network_actor, msg)
                    .map_err(to_fiber_err)?
                    .map_err(to_fiber_err)?;
                Ok(())
            }
            Self::Rpc(rpc_actor) => call!(rpc_actor, |port| {
                CchFiberAgentMessage::PreflightPayment(
                    outgoing_pay_req,
                    tlc_limit,
                    max_fee,
                    max_fee_rate,
                    port,
                )
            })
            .map_err(to_fiber_err)?
            .map(|_| ())
            .map_err(CchError::FiberNodeError),
        }
    }

    /// Send payment and forward the result to the CCH actor. The mapping from the
    /// backend-specific reply type to [CchMessage] is handled internally.
    ///
    /// `max_fee_amount` caps the outgoing routing fee at the CCH order's fee budget so the
    /// operator never spends more on the outgoing leg than it collected on the incoming leg.
    /// `max_fee_rate` is derived from `max_fee_amount` and the payment amount so Fiber's default
    /// rate cap does not clamp the fee below the budget.
    pub async fn forward_send_payment(
        &self,
        outgoing_pay_req: String,
        tlc_expiry_limit: u64,
        fee_limit: OutgoingFeeLimit,
        target: &ActorRef<CchMessage>,
        payment_hash: Hash256,
        retry_count: u32,
    ) -> Result<(), ractor::RactorErr<CchMessage>> {
        let tlc_limit = Some(tlc_expiry_limit);
        let max_fee = Some(fee_limit.max_fee_amount);
        let max_fee_rate = Some(fee_limit.max_fee_rate);
        let target_ref = target.clone();
        match self {
            Self::InProcess(network_actor) => {
                let pay_req = outgoing_pay_req.clone();
                forward!(
                    network_actor,
                    |tx| NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                        SendPaymentCommand {
                            invoice: Some(pay_req),
                            tlc_expiry_limit: tlc_limit,
                            max_fee_amount: max_fee,
                            max_fee_rate,
                            ..Default::default()
                        },
                        tx,
                    )),
                    target_ref,
                    move |result| map_send_payment_result(
                        result.map(|r| r.status),
                        payment_hash,
                        retry_count,
                    )
                )
            }
            Self::Rpc(rpc_actor) => {
                let target_ref = target.clone();
                forward!(
                    rpc_actor,
                    |port| CchFiberAgentMessage::SendPayment(
                        outgoing_pay_req,
                        tlc_limit,
                        max_fee,
                        max_fee_rate,
                        port
                    ),
                    target_ref,
                    move |result| map_send_payment_result(
                        result.map_err(|e| e.to_string()),
                        payment_hash,
                        retry_count,
                    )
                )
            }
        }
    }

    /// Query a durable Fiber payment and forward the result to the CCH actor.
    pub async fn forward_get_payment(
        &self,
        target: &ActorRef<CchMessage>,
        payment_hash: Hash256,
        retry_count: u32,
    ) -> Result<(), ractor::RactorErr<CchMessage>> {
        let target_ref = target.clone();
        match self {
            Self::InProcess(network_actor) => forward!(
                network_actor,
                |port| NetworkActorMessage::Command(NetworkActorCommand::GetPayment(
                    payment_hash,
                    port,
                )),
                target_ref,
                move |result: Result<SendPaymentResponse, String>| {
                    map_get_payment_result(
                        result.map(|response| {
                            (
                                response.status,
                                response.payment_preimage,
                                response.failed_error,
                            )
                        }),
                        payment_hash,
                        retry_count,
                    )
                }
            ),
            Self::Rpc(rpc_actor) => forward!(
                rpc_actor,
                |port| CchFiberAgentMessage::GetPayment(payment_hash, port),
                target_ref,
                move |result| map_get_payment_result(
                    result
                        .map(|response| {
                            (
                                response.status.into(),
                                response.payment_preimage.map(Into::into),
                                response.failed_error,
                            )
                        })
                        .map_err(|err| err.to_string()),
                    payment_hash,
                    retry_count,
                )
            ),
        }
    }

    /// Settle an invoice by payment hash and preimage.
    pub async fn call_settle_invoice(
        &self,
        payment_hash: Hash256,
        payment_preimage: Hash256,
    ) -> Result<(), anyhow::Error> {
        match self {
            Self::InProcess(network_actor) => {
                let cmd =
                    move |tx: RpcReplyPort<Result<(), crate::invoice::SettleInvoiceError>>| {
                        NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                            payment_hash,
                            payment_preimage,
                            tx,
                        ))
                    };
                call!(network_actor, cmd)
                    .map_err(|e| anyhow::anyhow!("network actor: {}", e))?
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                Ok(())
            }
            Self::Rpc(rpc_actor) => call!(rpc_actor, |port| {
                CchFiberAgentMessage::SettleInvoice(payment_hash, payment_preimage, port)
            })
            .map_err(|e| anyhow::anyhow!("fiber agent actor: {}", e))?
            .map_err(|e| anyhow::anyhow!("{}", e)),
        }
    }
}

fn is_permanent_send_payment_error(err: &str) -> bool {
    if err.contains("InvalidParameter") {
        return true;
    }
    let err_lower = err.to_lowercase();
    err_lower.contains("invalid payment request")
        || err_lower.contains("invoice expired")
        || err_lower.contains("payment hash mismatch")
        || err_lower.contains("no path found")
}

fn map_send_payment_result(
    result: Result<PaymentStatus, String>,
    payment_hash: Hash256,
    retry_count: u32,
) -> CchMessage {
    match result {
        Ok(status) => CchMessage::TrackingEvent(CchTrackingEvent::PaymentChanged {
            payment_hash,
            payment_preimage: None,
            status,
            failure_reason: None,
        }),
        Err(err) if err.contains("Payment session already exists") => {
            CchMessage::TrackingEvent(CchTrackingEvent::PaymentChanged {
                payment_hash,
                payment_preimage: None,
                status: PaymentStatus::Inflight,
                failure_reason: None,
            })
        }
        Err(err) => {
            let failure_reason = format!("SendFiberOutgoingPaymentExecutor failure: {:?}", err);
            if is_permanent_send_payment_error(&err) {
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
}

fn map_get_payment_result(
    result: Result<(PaymentStatus, Option<Hash256>, Option<String>), String>,
    payment_hash: Hash256,
    retry_count: u32,
) -> CchMessage {
    match result {
        Ok((PaymentStatus::Success, None, _)) => CchMessage::ActionRetry {
            payment_hash,
            action: CchOrderAction::TrackOutgoingPayment,
            retry_count,
            reason: "successful Fiber payment has no persisted preimage".to_string(),
        },
        Ok((status, payment_preimage, failure_reason)) => {
            CchMessage::TrackingEvent(CchTrackingEvent::PaymentChanged {
                payment_hash,
                payment_preimage,
                status,
                failure_reason,
            })
        }
        Err(reason) => CchMessage::ActionRetry {
            payment_hash,
            action: CchOrderAction::TrackOutgoingPayment,
            retry_count,
            reason: format!("failed to query Fiber payment: {reason}"),
        },
    }
}
