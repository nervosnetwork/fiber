use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{
    create_invoices_client, create_router_client, invoicesrpc, lnrpc, routerrpc, InvoicesClient,
    RouterClient, Uri,
};
use ractor::{call, DerivedActorRef, RpcReplyPort};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{select, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::cch::order::CchInvoice;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::types::{Hash256, Pubkey};
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::store::subscription::{
    InvoiceState, InvoiceSubscription, PaymentState, PaymentSubscription, PaymentUpdate,
};
use crate::store::subscription_impl::SubscriptionImpl;
use crate::store::{SubscriptionError, SubscriptionId};

use super::order::{
    CchInvoiceUpdate, CchPaymentUpdate, FiberInvoiceUpdate, FiberPaymentUpdate,
    LightningInvoiceUpdate, LightningPaymentUpdate,
};
use super::{CchConfig, CchError, CchOrder, CchOrdersDb};

pub const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;
pub const DEFAULT_ORDER_EXPIRY_SECONDS: u64 = 86400; // 24 hours

pub async fn start_cch(
    config: CchConfig,
    tracker: TaskTracker,
    token: CancellationToken,
    root_actor: ActorCell,
    network_actor: ActorRef<NetworkActorMessage>,
    pubkey: Pubkey,
    subscription: SubscriptionImpl,
) -> Result<ActorRef<CchMessage>> {
    let lnd_connection = config.get_lnd_connection_info().await?;
    let (actor, _handle) = Actor::spawn_linked(
        Some("cch actor".to_string()),
        CchActor::new(
            config,
            tracker,
            token,
            network_actor,
            pubkey,
            subscription,
            lnd_connection,
        ),
        (),
        root_actor,
    )
    .await?;
    Ok(actor)
}

#[derive(Clone, Debug, Deserialize)]
pub struct SendBTC {
    pub btc_pay_req: String,
    pub currency: Currency,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReceiveBTC {
    pub fiber_pay_req: String,
}

pub enum CchMessage {
    SendBTC(SendBTC, RpcReplyPort<Result<CchOrder, CchError>>),
    ReceiveBTC(ReceiveBTC, RpcReplyPort<Result<CchOrder, CchError>>),

    GetCchOrder(Hash256, RpcReplyPort<Result<CchOrder, CchError>>),

    LightningPaymentUpdate(LightningPaymentUpdate),
    LightningInvoiceUpdate(LightningInvoiceUpdate),

    FiberPaymentUpdate(FiberPaymentUpdate),
    FiberInvoiceUpdate(FiberInvoiceUpdate),

    SubscribeFiberPayment(
        Hash256,
        DerivedActorRef<FiberPaymentUpdate>,
        RpcReplyPort<Result<SubscriptionId, SubscriptionError>>,
    ),

    UnsubscribeFiberPayment(SubscriptionId, RpcReplyPort<Result<(), SubscriptionError>>),

    SubscribeFiberInvoice(
        Hash256,
        DerivedActorRef<FiberInvoiceUpdate>,
        RpcReplyPort<Result<SubscriptionId, SubscriptionError>>,
    ),

    UnsubscribeFiberInvoice(SubscriptionId, RpcReplyPort<Result<(), SubscriptionError>>),
}

impl From<FiberPaymentUpdate> for CchMessage {
    fn from(update: FiberPaymentUpdate) -> Self {
        CchMessage::FiberPaymentUpdate(update)
    }
}

impl TryFrom<CchMessage> for FiberPaymentUpdate {
    type Error = anyhow::Error;

    fn try_from(msg: CchMessage) -> Result<Self, Self::Error> {
        match msg {
            CchMessage::FiberPaymentUpdate(update) => Ok(update),
            _ => Err(anyhow!("CchMessage is not PaymentUpdate")),
        }
    }
}

impl From<FiberInvoiceUpdate> for CchMessage {
    fn from(update: FiberInvoiceUpdate) -> Self {
        CchMessage::FiberInvoiceUpdate(update)
    }
}

impl TryFrom<CchMessage> for FiberInvoiceUpdate {
    type Error = anyhow::Error;

    fn try_from(msg: CchMessage) -> Result<Self, Self::Error> {
        match msg {
            CchMessage::FiberInvoiceUpdate(update) => Ok(update),
            _ => Err(anyhow!("CchMessage is not InvoiceUpdate")),
        }
    }
}

#[derive(Clone)]
pub struct LndConnectionInfo {
    pub uri: Uri,
    pub cert: Option<Vec<u8>>,
    pub macaroon: Option<Vec<u8>>,
}

impl LndConnectionInfo {
    pub(crate) async fn create_router_client(
        &self,
    ) -> Result<RouterClient, lnd_grpc_tonic_client::channel::Error> {
        create_router_client(
            self.uri.clone(),
            self.cert.as_deref(),
            self.macaroon.as_deref(),
        )
        .await
    }

    pub(crate) async fn create_invoices_client(
        &self,
    ) -> Result<InvoicesClient, lnd_grpc_tonic_client::channel::Error> {
        create_invoices_client(
            self.uri.clone(),
            self.cert.as_deref(),
            self.macaroon.as_deref(),
        )
        .await
    }
}

pub struct CchActor {
    config: CchConfig,
    tracker: TaskTracker,
    token: CancellationToken,
    network_actor: ActorRef<NetworkActorMessage>,
    pubkey: Pubkey,
    subscription: SubscriptionImpl,
    lnd_connection: LndConnectionInfo,
}

pub struct CchState {
    orders_db: CchOrdersDb,
}

#[ractor::async_trait]
impl Actor for CchActor {
    type Msg = CchMessage;
    type State = CchState;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _config: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let lnd_connection = self.config.get_lnd_connection_info().await?;

        let payments_tracker =
            LndPaymentsTracker::new(myself.clone(), lnd_connection.clone(), self.token.clone());
        self.tracker
            .spawn(async move { payments_tracker.run().await });

        Ok(CchState {
            orders_db: Default::default(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CchMessage::SendBTC(send_btc, port) => {
                let result = self.send_btc(state, send_btc, myself.get_derived()).await;
                let _ = port.send(result);
                Ok(())
            }
            CchMessage::ReceiveBTC(receive_btc, port) => {
                let result = self
                    .receive_btc(myself.clone(), state, receive_btc, myself.get_derived())
                    .await;
                let _ = port.send(result);
                Ok(())
            }
            CchMessage::GetCchOrder(payment_hash, port) => {
                let result = state
                    .orders_db
                    .get_cch_order(&payment_hash)
                    .await
                    .map_err(Into::into);
                let _ = port.send(result);
                Ok(())
            }
            CchMessage::LightningPaymentUpdate(payment_update) => {
                if let Err(err) = self
                    .handle_payment_update(
                        state,
                        CchPaymentUpdate {
                            is_fiber: false,
                            update: payment_update,
                        },
                    )
                    .await
                {
                    tracing::error!(
                        payment_update = ?payment_update,
                        error = ?err,
                        "Failed to handle lightning payment update",
                    );
                }
                Ok(())
            }
            CchMessage::LightningInvoiceUpdate(event) => {
                if let Err(err) = self
                    .handle_invoice_update(
                        state,
                        CchInvoiceUpdate {
                            is_fiber: false,
                            update: event,
                        },
                    )
                    .await
                {
                    tracing::error!(
                        invoice_update = ?event,
                        error = ?err,
                        "Failed to handle lightning invoice update",
                    );
                }
                Ok(())
            }
            CchMessage::FiberPaymentUpdate(payment_update) => {
                if let Err(err) = self
                    .handle_payment_update(
                        state,
                        CchPaymentUpdate {
                            is_fiber: true,
                            update: payment_update,
                        },
                    )
                    .await
                {
                    tracing::error!(payment_update = ?payment_update, error = ?err, "Failed to handle fiber payment update");
                }
                Ok(())
            }

            CchMessage::FiberInvoiceUpdate(invoice_update) => {
                if let Err(err) = self
                    .handle_invoice_update(
                        state,
                        CchInvoiceUpdate {
                            is_fiber: true,
                            update: invoice_update,
                        },
                    )
                    .await
                {
                    tracing::error!(invoice_update = ?invoice_update, error = ?err, "Failed to handle fiber invoice update");
                }
                Ok(())
            }

            CchMessage::SubscribeFiberPayment(hash256, actor_ref, rpc_reply_port) => {
                let result = self
                    .subscription
                    .subscribe_payment(hash256, actor_ref.clone())
                    .await
                    .map_err(Into::into);

                let _ = rpc_reply_port.send(result);
                Ok(())
            }
            CchMessage::UnsubscribeFiberPayment(subscription_id, rpc_reply_port) => {
                let result = self
                    .subscription
                    .unsubscribe_payment(subscription_id)
                    .await
                    .map_err(Into::into);

                let _ = rpc_reply_port.send(result);
                Ok(())
            }
            CchMessage::SubscribeFiberInvoice(hash256, actor_ref, rpc_reply_port) => {
                let result = self
                    .subscription
                    .subscribe_invoice(hash256, actor_ref.clone())
                    .await
                    .map_err(Into::into);

                let _ = rpc_reply_port.send(result);
                Ok(())
            }
            CchMessage::UnsubscribeFiberInvoice(subscription_id, rpc_reply_port) => {
                let result = self
                    .subscription
                    .unsubscribe_invoice(subscription_id)
                    .await
                    .map_err(Into::into);

                let _ = rpc_reply_port.send(result);
                Ok(())
            }
        }
    }
}

impl CchActor {
    pub fn new(
        config: CchConfig,
        tracker: TaskTracker,
        token: CancellationToken,
        network_actor: ActorRef<NetworkActorMessage>,
        pubkey: Pubkey,
        subscription: SubscriptionImpl,
        lnd_connection: LndConnectionInfo,
    ) -> Self {
        Self {
            config,
            tracker,
            token,
            network_actor,
            pubkey,
            subscription,
            lnd_connection,
        }
    }

    async fn send_btc(
        &self,
        state: &mut CchState,
        send_btc: SendBTC,
        fiber_invoice_tracker: DerivedActorRef<FiberInvoiceUpdate>,
    ) -> Result<CchOrder, CchError> {
        let out_invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        tracing::debug!(btc_invoice = ?out_invoice, "Received SentBTC order");

        let our_pubkey = self.pubkey;
        let out_final_expiry_delta = self.config.ckb_final_tlc_expiry_delta;
        let out_currency = Currency::Fibb;
        let out_script = self.config.get_wrapped_btc_script();
        let fee_rate_per_million_sats = self.config.fee_rate_per_million_sats as u128;
        let base_fee_sats = self.config.base_fee_sats as u128;

        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        let expiry = out_invoice
            .expires_at()
            .and_then(|expired_at| expired_at.checked_sub(duration_since_epoch))
            .map(|duration| duration.as_secs())
            .ok_or(CchError::BTCInvoiceExpired)?;

        let amount_msat = out_invoice
            .amount_milli_satoshis()
            .ok_or(CchError::BTCInvoiceMissingAmount)? as u128;

        let fee_sats = amount_msat * fee_rate_per_million_sats / 1_000_000_000u128 + base_fee_sats;

        let amount = amount_msat.div_ceil(1_000u128) + fee_sats;

        let fiber_invoice = InvoiceBuilder::new(out_currency)
            .payee_pub_key(our_pubkey.into())
            .amount(Some(amount))
            .payment_hash(out_invoice.payment_hash().into())
            .hash_algorithm(HashAlgorithm::Sha256)
            .expiry_time(Duration::from_secs(expiry))
            .final_expiry_delta(out_final_expiry_delta)
            .udt_type_script(out_script.clone())
            .build()?;

        let order = CchOrder::new(
            *fiber_invoice.payment_hash(),
            duration_since_epoch.as_secs(),
            expiry,
            amount_msat.div_ceil(1_000u128) + fee_sats,
            fee_sats,
            CchInvoice::Fiber(fiber_invoice.clone()),
            CchInvoice::Lightning(out_invoice),
        );

        let message = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::AddInvoice(
                fiber_invoice.clone(),
                None,
                rpc_reply,
            ))
        };

        call!(&self.network_actor, message).expect("call actor")?;

        self.subscription
            .subscribe_invoice(order.payment_hash, fiber_invoice_tracker)
            .await?;

        state.orders_db.insert_cch_order(order.clone()).await?;

        Ok(order)
    }

    async fn receive_btc(
        &self,
        myself: ActorRef<CchMessage>,
        state: &mut CchState,
        receive_btc: ReceiveBTC,
        fiber_payment_tracker: DerivedActorRef<FiberPaymentUpdate>,
    ) -> Result<CchOrder, CchError> {
        let invoice = CkbInvoice::from_str(&receive_btc.fiber_pay_req)?;
        tracing::debug!(ckb_invoice = ?invoice, "Received ReceiveBTC order");
        let wbtc_script = self.config.get_wrapped_btc_script();
        let udt_type_script = invoice.udt_type_script();
        if udt_type_script != Some(&wbtc_script) {
            return Err(CchError::ReceiveBTCInvalidUdtScript(
                wbtc_script.clone(),
                udt_type_script.cloned(),
            ));
        }
        let payment_hash = *invoice.payment_hash();
        let payment_hash_str = format!("0x{}", hex::encode(payment_hash));
        let amount_sats = invoice.amount().ok_or(CchError::CKBInvoiceMissingAmount)?;
        let final_tlc_minimum_expiry_delta =
            *invoice.final_tlc_minimum_expiry_delta().unwrap_or(&0);
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let fee_sats = amount_sats * (self.config.fee_rate_per_million_sats as u128)
            / 1_000_000u128
            + (self.config.base_fee_sats as u128);
        if amount_sats <= fee_sats {
            return Err(CchError::CchOrderAmountTooSmall);
        }
        if amount_sats > (i64::MAX / 1_000i64) as u128 {
            return Err(CchError::CchOrderAmountTooLarge);
        }

        let mut client = self.lnd_connection.create_invoices_client().await?;
        let req = invoicesrpc::AddHoldInvoiceRequest {
            hash: payment_hash.into(),
            value_msat: (amount_sats * 1_000u128) as i64,
            expiry: DEFAULT_ORDER_EXPIRY_SECONDS as i64,
            // TODO: We are using different units for lock time here.
            // Lightning uses blocks in cltv_expiry, while fiber uses seconds.
            cltv_expiry: self.config.btc_final_tlc_expiry + final_tlc_minimum_expiry_delta,
            ..Default::default()
        };
        let add_invoice_resp = client
            .add_hold_invoice(req)
            .await
            .map_err(|err| CchError::LndRpcError(err.to_string()))?
            .into_inner();
        let btc_pay_req = add_invoice_resp.payment_request;
        let in_invoice = Bolt11Invoice::from_str(&btc_pay_req)?;

        let order = CchOrder::new(
            payment_hash,
            duration_since_epoch.as_secs(),
            DEFAULT_ORDER_EXPIRY_SECONDS,
            amount_sats,
            fee_sats,
            CchInvoice::Lightning(in_invoice),
            CchInvoice::Fiber(invoice.clone()),
        );

        self.subscription
            .subscribe_payment(payment_hash, fiber_payment_tracker)
            .await?;

        let invoice_tracker = LndInvoiceTracker::new(
            myself,
            payment_hash_str,
            self.lnd_connection.clone(),
            self.token.clone(),
        );
        self.tracker
            .spawn(async move { invoice_tracker.run().await });

        state.orders_db.insert_cch_order(order.clone()).await?;

        Ok(order)
    }

    async fn pay_invoice(&self, invoice: CchInvoice) -> Result<Option<CchPaymentUpdate>, CchError> {
        let payment_hash = invoice.payment_hash();
        tracing::debug!(
            payment_hash = ?payment_hash,
            invoice = ?invoice,
            "Paying invoice",
        );

        match &invoice {
            CchInvoice::Lightning(invoice) => {
                let out_invoice = invoice.to_string();
                let req = routerrpc::SendPaymentRequest {
                    payment_request: out_invoice,
                    timeout_seconds: BTC_PAYMENT_TIMEOUT_SECONDS,
                    ..Default::default()
                };
                tracing::debug!("[inbounding tlc] SendPaymentRequest: {:?}", req);

                let mut client = self.lnd_connection.create_router_client().await?;
                // TODO: set a fee
                let mut stream = client
                    .send_payment_v2(req)
                    .await
                    .map_err(|err| CchError::LndGrpcRequestError(err.to_string()))?
                    .into_inner();
                // Wait for the first message then quit
                select! {
                    payment_result_opt = stream.next() => {
                        tracing::debug!("[inbounding tlc] payment result: {:?}", payment_result_opt);
                        if let Some(Ok(payment)) = payment_result_opt {
                            Ok(Some(
                                CchPaymentUpdate {
                                is_fiber: false,
                                update: PaymentUpdate::try_from(payment).map_err(|err| {
                                    CchError::UnexpectedLndData(err.to_string())
                                })?
                            }
                            ))
                        } else {
                            Ok(None)
                        }
                    }
                    _ = self.token.cancelled() => {
                        tracing::debug!("Cancellation received, shutting down cch service");
                        Err(CchError::TaskCanceled)
                    }
                }
            }
            CchInvoice::Fiber(fiber_invoice) => {
                let message = |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                        SendPaymentCommand {
                            invoice: Some(fiber_invoice.to_string()),
                            ..Default::default()
                        },
                        rpc_reply,
                    ))
                };

                // TODO: handle payment failure here.
                let tlc_response = call!(self.network_actor, message)
                    .expect("call actor")
                    .map_err(CchError::SendFiberPaymentError)?;
                // TODO: handle payment failure here.
                let state = if tlc_response.status == PaymentSessionStatus::Failed {
                    PaymentState::Failed
                } else {
                    PaymentState::Inflight
                };
                Ok(Some(CchPaymentUpdate {
                    is_fiber: true,
                    update: PaymentUpdate {
                        hash: payment_hash,
                        state,
                    },
                }))
            }
        }
    }

    async fn handle_invoice_update(
        &self,
        state: &mut CchState,
        invoice_update: CchInvoiceUpdate,
    ) -> Result<(), CchError> {
        let CchInvoiceUpdate {
            is_fiber,
            update: invoice_update,
        } = invoice_update;
        tracing::trace!(is_fiber = is_fiber, invoice_update = ?invoice_update, "Cch received invoice update");

        let mut order = state.orders_db.get_cch_order(&invoice_update.hash).await?;

        // TODO: check that we can update the in_state.
        order.in_state = invoice_update.state;
        state.orders_db.update_cch_order(order.clone()).await?;

        match (order.in_state, order.out_state) {
            (
                InvoiceState::Received {
                    is_finished: true, ..
                },
                PaymentState::Created,
            ) => {
                order.out_state = PaymentState::Inflight;
            }
            _ => {
                // TODO: handle other states
                return Ok(());
            }
        }

        let result = self.pay_invoice(order.out_invoice.clone()).await?;
        state.orders_db.update_cch_order(order.clone()).await?;
        if let Some(payment_update) = result {
            return self.handle_payment_update(state, payment_update).await;
        }

        Ok(())
    }

    async fn settle_invoice(
        &self,
        invoice: &CchInvoice,
        preimage: Hash256,
    ) -> Result<(), CchError> {
        let payment_hash = invoice.payment_hash();
        tracing::debug!(
            hash = ?payment_hash,
            preimage = ?preimage,
            invoice = ?invoice,
            "Settling invoice",
        );
        match &invoice {
            CchInvoice::Lightning(_) => {
                let req = invoicesrpc::SettleInvoiceMsg {
                    preimage: preimage.as_ref().to_vec(),
                };
                let mut client = self.lnd_connection.create_invoices_client().await?;
                let _ = client
                    .settle_invoice(req)
                    .await
                    .map_err(|error| CchError::LndGrpcRequestError(error.to_string()))?
                    .into_inner();
                // TODO: settle_invoice response actually contains no useful information.
                // We need to check the invoice state to see if it's settled.
            }
            CchInvoice::Fiber(_) => {
                let message = move |rpc_reply| -> NetworkActorMessage {
                    NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                        payment_hash,
                        preimage,
                        rpc_reply,
                    ))
                };

                call!(&self.network_actor, message).expect("call actor")?;
            }
        }
        Ok(())
    }

    async fn handle_payment_update(
        &self,
        state: &mut CchState,
        payment_update: CchPaymentUpdate,
    ) -> Result<(), CchError> {
        let CchPaymentUpdate {
            is_fiber,
            update: payment_update,
        } = payment_update;
        tracing::trace!(is_fiber = is_fiber, payment_update = ?payment_update, "Cch received payment update");
        let payment_hash = payment_update.hash;

        let mut order = state.orders_db.get_cch_order(&payment_hash).await?;

        order.out_state = payment_update.state;
        match (&order.in_state, &order.out_state) {
            (
                InvoiceState::Received {
                    is_finished: true, ..
                },
                PaymentState::Success { preimage },
            ) => {
                let preimage = *preimage;
                order.payment_preimage = Some(preimage);
                self.settle_invoice(&order.in_invoice, preimage).await?;
            }
            (_, PaymentState::Failed) => {
                // TODO: handle payment failure
            }
            _ => {
                // TODO: handle other states
            }
        }

        state.orders_db.update_cch_order(order).await?;

        Ok(())
    }
}

struct LndPaymentsTracker {
    cch_actor: ActorRef<CchMessage>,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
}

impl LndPaymentsTracker {
    fn new(
        cch_actor: ActorRef<CchMessage>,
        lnd_connection: LndConnectionInfo,
        token: CancellationToken,
    ) -> Self {
        Self {
            cch_actor,
            lnd_connection,
            token,
        }
    }

    async fn run(self) {
        tracing::debug!(
            target: "fnn::cch::actor::tracker::lnd_payments",
            "will connect {}",
            self.lnd_connection.uri
        );

        // TODO: clean up expired orders
        loop {
            select! {
                result = self.run_inner() => {
                    match result {
                        Ok(_) => {
                            break;
                        }
                        Err(err) => {
                            tracing::error!(
                                target: "fnn::cch::actor::tracker::lnd_payments",
                                "Error tracking LND payments, retry 15 seconds later: {:?}",
                                err
                            );
                            select! {
                                _ = sleep(Duration::from_secs(15)) => {
                                    // continue
                                }
                                _ = self.token.cancelled() => {
                                    tracing::debug!("Cancellation received, shutting down cch service");
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down cch service");
                    return;
                }
            }
        }
    }

    async fn run_inner(&self) -> Result<()> {
        let mut client = self.lnd_connection.create_router_client().await?;
        let mut stream = client
            .track_payments(routerrpc::TrackPaymentsRequest {
                no_inflight_updates: true,
            })
            .await?
            .into_inner();

        tracing::debug!("Subscribed to lnd payments");
        loop {
            select! {
                payment_opt = stream.next() => {
                    match payment_opt {
                        Some(Ok(payment)) => self.on_payment(payment).await?,
                        Some(Err(err)) => return Err(err.into()),
                        None => return Err(anyhow!("unexpected closed stream")),
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down cch service");
                    return Ok(());
                }
            }
        }
    }

    async fn on_payment(&self, payment: lnrpc::Payment) -> Result<()> {
        tracing::debug!(target: "fnn::cch::actor::tracker::lnd_payments", "payment: {:?}", payment);
        match LightningPaymentUpdate::try_from(payment) {
            Ok(update) => {
                let message = CchMessage::LightningPaymentUpdate(update);
                self.cch_actor.cast(message)?;
            }
            Err(err) => {
                tracing::error!(
                    target: "fnn::cch::actor::tracker::lnd_payments",
                    "Failed to parse lnd payment: {}",
                    err
                );
            }
        }
        Ok(())
    }
}

/// Subscribe single invoice.
///
/// Lnd does not notify Accepted event in SubscribeInvoices rpc.
///
/// <https://github.com/lightningnetwork/lnd/blob/07b6af41dbe2a5a1c85e5c46cc41019b64640d90/invoices/invoiceregistry.go#L292-L293>
struct LndInvoiceTracker {
    cch_actor: ActorRef<CchMessage>,
    payment_hash: String,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
}

impl LndInvoiceTracker {
    fn new(
        cch_actor: ActorRef<CchMessage>,
        payment_hash: String,
        lnd_connection: LndConnectionInfo,
        token: CancellationToken,
    ) -> Self {
        Self {
            cch_actor,
            payment_hash,
            lnd_connection,
            token,
        }
    }

    async fn run(self) {
        tracing::debug!(
            target: "fnn::cch::actor::tracker::lnd_invoice",
            "will connect {}",
            self.lnd_connection.uri
        );
        loop {
            select! {
                result = self.run_inner() => {
                    match result {
                        Ok(_) => {
                            break;
                        }
                        Err(err) => {
                            tracing::error!(
                                target: "fnn::cch::actor::tracker::lnd_invoice",
                                "Error tracking LND invoices, retry 15 seconds later: {:?}",
                                err
                            );
                            select! {
                                _ = sleep(Duration::from_secs(15)) => {
                                    // continue
                                }
                                _ = self.token.cancelled() => {
                                    tracing::debug!("Cancellation received, shutting down cch service");
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down cch service");
                    return;
                }
            }
        }
    }

    async fn run_inner(&self) -> Result<()> {
        let mut client = self.lnd_connection.create_invoices_client().await?;
        // TODO: clean up expired orders
        let mut stream = client
            .subscribe_single_invoice(invoicesrpc::SubscribeSingleInvoiceRequest {
                r_hash: hex::decode(self.payment_hash.trim_start_matches("0x"))?,
            })
            .await?
            .into_inner();
        tracing::debug!("Subscribed to lnd invoice: {}", self.payment_hash);
        loop {
            select! {
                invoice_opt = stream.next() => {
                    match invoice_opt {
                        Some(Ok(invoice)) => if self.on_invoice(invoice).await? {
                            return Ok(());
                        },
                        Some(Err(err)) => return Err(err.into()),
                        None => return Err(anyhow!("unexpected closed stream")),
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down cch service");
                    return Ok(());
                }
            }
        }
    }

    // Return true to quit the tracker
    async fn on_invoice(&self, invoice: lnrpc::Invoice) -> Result<bool> {
        tracing::debug!("[LndInvoiceTracker] invoice: {:?}", invoice);
        let should_quit = match LightningInvoiceUpdate::try_from(invoice) {
            Ok(update) => {
                let is_final = update.state.is_final();
                let message = CchMessage::LightningInvoiceUpdate(update);
                self.cch_actor.cast(message)?;
                is_final
            }
            Err(err) => {
                tracing::error!("Failed to parse lnd invoice: {}", err);
                return Ok(true);
            }
        };

        Ok(should_quit)
    }
}
