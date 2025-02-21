use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{
    create_invoices_client, create_router_client, invoicesrpc, lnrpc, routerrpc, InvoicesClient,
    RouterClient, Uri,
};
use ractor::{call, RpcReplyPort};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{select, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::cch::order::{CchInvoice, CchOrderActor};
use crate::errors::ALREADY_EXISTS_DESCRIPTION;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::types::{Hash256, Pubkey};
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::store::subscription::{
    InvoiceSubscription, PaymentState, PaymentSubscription, PaymentUpdate,
};
use crate::store::subscription_impl::SubscriptionImpl;
use crate::store::SubscriptionId;

use super::order::{
    CchInvoiceUpdate, CchOrderActorMessage, CchPaymentUpdate, LightningInvoiceUpdate,
    LightningPaymentUpdate,
};
use super::{CchConfig, CchError, CchOrder, CchOrderStatus, CchOrderStore};

pub const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;
pub const DEFAULT_ORDER_EXPIRY_SECONDS: u64 = 86400; // 24 hours

#[allow(clippy::too_many_arguments)]
pub async fn start_cch<S: CchOrderStore + Clone + Send + Sync + 'static>(
    config: CchConfig,
    tracker: TaskTracker,
    token: CancellationToken,
    root_actor: ActorCell,
    network_actor: ActorRef<NetworkActorMessage>,
    pubkey: Pubkey,
    subscription: SubscriptionImpl,
    store: S,
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
            store,
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

    PayInvoice(
        CchInvoice,
        RpcReplyPort<Result<Option<CchPaymentUpdate>, CchError>>,
    ),
    SettleInvoice(
        CchInvoice,
        // Preimage
        Hash256,
        RpcReplyPort<Result<(), CchError>>,
    ),

    LightningPaymentUpdate(LightningPaymentUpdate),
    LightningInvoiceUpdate(LightningInvoiceUpdate),

    NotifyOrderOutCome(Hash256, CchOrderStatus),
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

pub struct CchActor<S> {
    config: CchConfig,
    tracker: TaskTracker,
    token: CancellationToken,
    network_actor: ActorRef<NetworkActorMessage>,
    pubkey: Pubkey,
    subscription: SubscriptionImpl,
    lnd_connection: LndConnectionInfo,
    store: S,
}

#[derive(Default)]
pub struct CchState {
    orders: HashMap<Hash256, CchOrderActorWithTrackingHandle>,
}

// This is the cch order actor with the tracking handles.
// When the actor is stopped, the tracking handles will be dropped.
pub struct CchOrderActorWithTrackingHandle {
    actor: ActorRef<CchOrderActorMessage>,
    invoice_tracking_handle: TrackingHandle,
    payment_tracking_handle: TrackingHandle,
}

impl CchOrderActorWithTrackingHandle {
    pub fn new(
        actor: ActorRef<CchOrderActorMessage>,
        invoice_tracking_handle: TrackingHandle,
        payment_tracking_handle: TrackingHandle,
    ) -> Self {
        Self {
            actor,
            invoice_tracking_handle,
            payment_tracking_handle,
        }
    }
}

impl CchState {
    fn get_order(
        &self,
        payment_hash: &Hash256,
    ) -> Result<&CchOrderActorWithTrackingHandle, CchError> {
        self.orders
            .get(payment_hash)
            .ok_or(CchError::OrderNotFound(*payment_hash))
    }

    fn send_message_to_order_actor(&self, payment_hash: &Hash256, message: CchOrderActorMessage) {
        match self.get_order(payment_hash) {
            Ok(order_actor) => {
                if let Err(err) = order_actor.actor.send_message(message) {
                    tracing::error!(error = ?err, "Failed to send message to order actor");
                }
            }
            Err(err) => {
                tracing::error!(error = ?err, "Failed to send message to order actor");
            }
        }
    }

    // We use https://lightning.engineering/api-docs/api/lnd/router/track-payments/
    // to track all the payments sent to the lnd node. But not all payments are related to the CCH.
    // We use this function to filter out the payments that are not related to the CCH.
    fn is_lightning_payment_subscribed(&self, payment_hash: &Hash256) -> bool {
        self.orders
            .get(payment_hash)
            .map_or(false, |order| !order.payment_tracking_handle.is_fiber())
    }
}

#[ractor::async_trait]
impl<S> Actor for CchActor<S>
where
    S: CchOrderStore + Clone + Send + Sync + 'static,
{
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

        let mut state = CchState::default();

        let orders = match self.store.get_active_cch_orders() {
            Ok(orders) => orders.into_iter().collect::<Vec<_>>(),
            Err(err) => {
                tracing::error!(error = ?err, "Failed to get active cch orders");
                return Ok(state);
            }
        };
        for order in orders {
            let payment_hash = order.payment_hash;
            let order_actor =
                CchOrderActor::start(&myself, self.store.clone(), order.clone()).await;
            if let Err(err) = self
                .subscribe_updates_for_order(&myself, &mut state, &order, &order_actor)
                .await
            {
                tracing::error!(error = ?err, payment_hash = ?payment_hash,  "Failed to subscribe updates for order");
                continue;
            }
        }

        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CchMessage::SendBTC(send_btc, port) => {
                let result = self.send_btc(&myself, state, send_btc).await;
                let _ = port.send(result);
                Ok(())
            }
            CchMessage::ReceiveBTC(receive_btc, port) => {
                let result = self.receive_btc(&myself, state, receive_btc).await;
                let _ = port.send(result);
                Ok(())
            }
            CchMessage::PayInvoice(cch_invoice, rpc_reply_port) => {
                let result = self.pay_invoice(cch_invoice).await.map_err(Into::into);
                let _ = rpc_reply_port.send(result);
                Ok(())
            }
            CchMessage::SettleInvoice(cch_invoice, hash256, rpc_reply_port) => {
                let result = self
                    .settle_invoice(&cch_invoice, hash256)
                    .await
                    .map_err(Into::into);
                let _ = rpc_reply_port.send(result);
                Ok(())
            }
            CchMessage::LightningPaymentUpdate(payment_update) => {
                if state.is_lightning_payment_subscribed(&payment_update.hash) {
                    state.send_message_to_order_actor(
                        &payment_update.hash,
                        CchOrderActorMessage::PaymentUpdate(CchPaymentUpdate {
                            is_fiber: false,
                            update: payment_update,
                        }),
                    );
                }
                Ok(())
            }
            CchMessage::LightningInvoiceUpdate(event) => {
                state.send_message_to_order_actor(
                    &event.hash,
                    CchOrderActorMessage::InvoiceUpdate(CchInvoiceUpdate {
                        is_fiber: false,
                        update: event,
                    }),
                );
                Ok(())
            }
            CchMessage::NotifyOrderOutCome(hash, cch_order_status) => {
                if matches!(
                    cch_order_status,
                    CchOrderStatus::Succeeded | CchOrderStatus::Failed
                ) {
                    tracing::debug!(hash = ?hash, status = ?cch_order_status, "Cch order finished");
                    if let Some(order) = state.orders.remove(&hash) {
                        self.stop_cch_order_actor(hash, order).await;
                    }
                }
                Ok(())
            }
        }
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        for (payment_hash, order) in state.orders.drain() {
            self.stop_cch_order_actor(payment_hash, order).await;
        }
        Ok(())
    }
}

impl<S> CchActor<S>
where
    S: CchOrderStore + Clone + Send + Sync + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: CchConfig,
        tracker: TaskTracker,
        token: CancellationToken,
        network_actor: ActorRef<NetworkActorMessage>,
        pubkey: Pubkey,
        subscription: SubscriptionImpl,
        lnd_connection: LndConnectionInfo,
        store: S,
    ) -> Self {
        Self {
            config,
            tracker,
            token,
            network_actor,
            pubkey,
            subscription,
            lnd_connection,
            store,
        }
    }

    async fn send_btc(
        &self,
        myself: &ActorRef<CchMessage>,
        state: &mut CchState,
        send_btc: SendBTC,
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

        self.store.create_cch_order(order.clone())?;
        let order_actor = CchOrderActor::start(myself, self.store.clone(), order.clone()).await;
        self.subscribe_updates_for_order(myself, state, &order, &order_actor)
            .await?;

        Ok(order)
    }

    async fn receive_btc(
        &self,
        myself: &ActorRef<CchMessage>,
        state: &mut CchState,
        receive_btc: ReceiveBTC,
    ) -> Result<CchOrder, CchError> {
        let invoice = CkbInvoice::from_str(&receive_btc.fiber_pay_req)?;
        if invoice.hash_algorithm() != Some(&HashAlgorithm::Sha256) {
            return Err(CchError::CKBInvoiceInvalidHashAlgorithm(
                invoice.hash_algorithm().copied(),
            ));
        }
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

        let order_actor = CchOrderActor::start(myself, self.store.clone(), order.clone()).await;
        self.subscribe_updates_for_order(myself, state, &order, &order_actor)
            .await?;
        self.store.create_cch_order(order.clone())?;

        Ok(order)
    }

    async fn stop_cch_order_actor(
        &self,
        payment_hash: Hash256,
        order: CchOrderActorWithTrackingHandle,
    ) {
        tracing::debug!(hash = ?payment_hash, "Order finished, stopping invoice/payment tracking");
        match order.invoice_tracking_handle {
            TrackingHandle::Fiber(subscription_id) => {
                if let Err(error) = self.subscription.unsubscribe_invoice(subscription_id).await {
                    tracing::error!(error = ?error, hash = ?payment_hash, "Failed to unsubscribe invoice");
                }
            }
            TrackingHandle::Lightning(handle) => {
                handle.cancel();
            }
        }
        match order.payment_tracking_handle {
            TrackingHandle::Fiber(subscription_id) => {
                if let Err(error) = self.subscription.unsubscribe_payment(subscription_id).await {
                    tracing::error!(error = ?error, hash = ?payment_hash, "Failed to unsubscribe payment");
                }
            }
            TrackingHandle::Lightning(handle) => {
                handle.cancel();
            }
        }
    }

    // Pay an invoice and return the optional payment update. Since we are sending a payment,
    // we may or may not an immediate payment status update (for example, when the same payment
    // is sent twice, lnd/fiber APIs currently will tell us the payment exists, but won't give
    // out detailed information), so we return an optional payment update. But the caller needs
    // to track the payment for progress anyway. So the payment update should really be treated
    // a nice-to-have information.
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
                let mut stream = match client.send_payment_v2(req).await {
                    Ok(stream) => stream.into_inner(),
                    // 6 is the standard code for grpc error ALREADY_EXISTS.
                    // https://grpc.io/docs/guides/status-codes/
                    // tonic is not reexported by lnd_grpc_tonic_client, so we have to use the raw code.
                    Err(status) if i32::from(status.code()) == 6 => {
                        return Ok(None);
                    }
                    Err(err) => {
                        return Err(CchError::LndGrpcRequestError(err.to_string()));
                    }
                };
                // Wait for the first message then quit
                select! {
                    payment_result_opt = stream.next() => {
                        match payment_result_opt {
                            Some(Ok(payment)) => {
                                Ok(Some(
                                    CchPaymentUpdate {
                                        is_fiber: false,
                                        update: PaymentUpdate::try_from(payment).map_err(|err| {
                                            CchError::UnexpectedLndData(err.to_string())
                                        })?
                                    }
                                ))
                            }
                            Some(Err(status)) => {
                                tracing::error!(payment_hash = ?payment_hash, status = ?status, status_code = i32::from(status.code()), "Sending lnd payment failed");
                                Err(CchError::LndGrpcRequestError(status.to_string()))
                            }
                            // We don't really know if the payment is successful or not.
                            // But in this case we can always retry the payment.
                            None => Ok(None)
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
                let tlc_response = match call!(self.network_actor, message).expect("call actor") {
                    Ok(tlc_response) => tlc_response,
                    Err(err) if err.contains(ALREADY_EXISTS_DESCRIPTION) => return Ok(None),
                    Err(err) => return Err(CchError::SendFiberPaymentError(err.to_string())),
                };
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

    fn start_lightning_invoice_tracker(
        &self,
        payment_hash: Hash256,
        myself: &ActorRef<CchMessage>,
    ) -> Result<LightningTrackingHandle, CchError> {
        let token = self.token.child_token();
        let payment_hash_str = format!("0x{}", hex::encode(payment_hash));
        let invoice_tracker = LndInvoiceTracker::new(
            myself.clone(),
            payment_hash_str,
            self.lnd_connection.clone(),
            token.clone(),
        );
        self.tracker
            .spawn(async move { invoice_tracker.run().await });
        Ok(token)
    }

    async fn start_fiber_invoice_tracker(
        &self,
        payment_hash: Hash256,
        order_actor: &ActorRef<CchOrderActorMessage>,
    ) -> Result<FiberTrackingHandle, CchError> {
        self.subscription
            .subscribe_invoice(payment_hash, order_actor.get_derived())
            .await
            .map_err(Into::into)
    }

    async fn start_lightning_payment_tracker(
        &self,
        payment_hash: Hash256,
        myself: &ActorRef<CchMessage>,
    ) -> Result<LightningTrackingHandle, CchError> {
        let token = self.token.child_token();
        let myself = myself.clone();
        let lnd_connection = self.lnd_connection.clone();

        // Start a payment tracking. Return an error when non-retriable error occurs.
        // Return Ok(None) when retriable error occurs. Return Ok(Some(payment_update)) when payment is successful.
        async fn get_payment(
            payment_hash: Hash256,
            lnd_connection: &LndConnectionInfo,
        ) -> Result<Option<PaymentUpdate>, CchError> {
            let mut stream = match lnd_connection.create_router_client().await {
                Ok(mut client) => match client
                    .track_payment_v2(routerrpc::TrackPaymentRequest {
                        no_inflight_updates: true,
                        payment_hash: payment_hash.as_ref().to_vec(),
                    })
                    .await
                {
                    Ok(stream) => stream.into_inner(),
                    Err(error) => {
                        tracing::warn!(error = ?error, "Failed to track payment");
                        return Ok(None);
                    }
                },
                Err(error) => {
                    tracing::warn!(error = ?error, "Failed to create router client");
                    return Ok(None);
                }
            };
            let payment_opt = stream.next().await;
            match payment_opt {
                Some(Ok(payment)) => {
                    let payment_update = PaymentUpdate::try_from(payment)
                        .map_err(|err| CchError::UnexpectedLndData(err.to_string()))?;
                    Ok(Some(payment_update))
                }
                Some(Err(err)) => {
                    tracing::error!(error = ?err, "Failed to track payment");
                    Ok(None)
                }
                None => {
                    tracing::error!("Unexpected closed stream while tracking lightning payment");
                    Ok(None)
                }
            }
        }

        self.tracker.spawn(async move {
            loop {
                select! {
                    payment_opt = get_payment(payment_hash, &lnd_connection) => {
                        match payment_opt {
                            Ok(Some(payment_update)) => {
                                let message = CchMessage::LightningPaymentUpdate(payment_update);
                                myself.cast(message).expect("send message to cch actor");
                                return;
                            }
                            Ok(None) => {
                                // Sleep for a while before retrying
                                sleep(Duration::from_secs(15)).await;
                            }
                            Err(err) => {
                                tracing::error!(error = ?err, "Failed to track lightning payment");
                                return;
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        tracing::debug!("Cancellation received while tracking lightning payment");
                        return;
                    }
                }
            }
        });

        tracing::debug!("Subscribed to lnd payments");

        let token = self.token.child_token();
        Ok(token)
    }

    async fn start_fiber_payment_tracker(
        &self,
        payment_hash: Hash256,
        order_actor: &ActorRef<CchOrderActorMessage>,
    ) -> Result<FiberTrackingHandle, CchError> {
        self.subscription
            .subscribe_payment(payment_hash, order_actor.get_derived())
            .await
            .map_err(Into::into)
    }

    async fn subscribe_updates_for_order(
        &self,
        myself: &ActorRef<CchMessage>,
        state: &mut CchState,
        order: &CchOrder,
        order_actor: &ActorRef<CchOrderActorMessage>,
    ) -> Result<(), CchError> {
        let payment_hash = order.payment_hash;
        let invoice_tracking_handle = if order.is_first_half_fiber() {
            TrackingHandle::Fiber(
                self.start_fiber_invoice_tracker(payment_hash, order_actor)
                    .await?,
            )
        } else {
            TrackingHandle::Lightning(self.start_lightning_invoice_tracker(payment_hash, myself)?)
        };
        let payment_tracking_handle = if order.is_second_half_fiber() {
            TrackingHandle::Fiber(
                self.start_fiber_payment_tracker(payment_hash, order_actor)
                    .await?,
            )
        } else {
            TrackingHandle::Lightning(
                self.start_lightning_payment_tracker(payment_hash, myself)
                    .await?,
            )
        };
        state.orders.insert(
            order.payment_hash,
            CchOrderActorWithTrackingHandle::new(
                order_actor.clone(),
                invoice_tracking_handle,
                payment_tracking_handle,
            ),
        );

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

pub type LightningTrackingHandle = CancellationToken;
pub type FiberTrackingHandle = SubscriptionId;

pub enum TrackingHandle {
    Lightning(LightningTrackingHandle),
    Fiber(FiberTrackingHandle),
}

impl TrackingHandle {
    pub fn is_fiber(&self) -> bool {
        matches!(self, TrackingHandle::Fiber(_))
    }
}
