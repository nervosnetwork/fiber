use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use hex::ToHex;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{
    create_invoices_client, create_router_client, invoicesrpc, lnrpc, routerrpc, InvoicesClient,
    RouterClient, Uri,
};
use ractor::{call, RpcReplyPort};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{select, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::types::{Hash256, Pubkey};
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoice, Currency};
use crate::store::subscription::{
    InvoiceUpdatedEvent, InvoiceUpdatedPayload, PaymentUpdatedEvent, PaymentUpdatedPayload,
    StoreUpdatedEvent,
};

use super::error::CchDbError;
use super::{CchConfig, CchError, CchOrderStatus, CchOrdersDb, ReceiveBTCOrder, SendBTCOrder};

pub const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;
pub const DEFAULT_ORDER_EXPIRY_SECONDS: u64 = 86400; // 24 hours

pub async fn start_cch(
    config: CchConfig,
    tracker: TaskTracker,
    token: CancellationToken,
    root_actor: ActorCell,
    network_actor: ActorRef<NetworkActorMessage>,
    pubkey: Pubkey,
) -> Result<ActorRef<CchMessage>> {
    let (actor, _handle) = Actor::spawn_linked(
        Some("cch actor".to_string()),
        CchActor::new(config, tracker, token, network_actor, pubkey),
        (),
        root_actor,
    )
    .await?;
    Ok(actor)
}

#[derive(Debug)]
pub struct SettleSendBTCOrderEvent {
    payment_hash: String,
    preimage: Option<String>,
    status: CchOrderStatus,
}

#[derive(Debug)]
pub struct SettleReceiveBTCOrderEvent {
    payment_hash: String,
    preimage: Option<String>,
    status: CchOrderStatus,
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
    SendBTC(SendBTC, RpcReplyPort<Result<SendBTCOrder, CchError>>),
    ReceiveBTC(ReceiveBTC, RpcReplyPort<Result<ReceiveBTCOrder, CchError>>),

    GetReceiveBTCOrder(String, RpcReplyPort<Result<ReceiveBTCOrder, CchError>>),

    SettleSendBTCOrder(SettleSendBTCOrderEvent),
    SettleReceiveBTCOrder(SettleReceiveBTCOrderEvent),

    StoreUpdatedEvent(StoreUpdatedEvent),
}

impl From<StoreUpdatedEvent> for CchMessage {
    fn from(event: StoreUpdatedEvent) -> Self {
        CchMessage::StoreUpdatedEvent(event)
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
}

pub struct CchState {
    lnd_connection: LndConnectionInfo,
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
            lnd_connection,
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
                let result = self.send_btc(state, send_btc).await;
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::ReceiveBTC(receive_btc, port) => {
                let result = self.receive_btc(myself, state, receive_btc).await;
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::GetReceiveBTCOrder(payment_hash, port) => {
                let result = state
                    .orders_db
                    .get_receive_btc_order(&payment_hash)
                    .await
                    .map_err(Into::into);
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::SettleSendBTCOrder(event) => {
                tracing::debug!("settle_send_btc_order {:?}", event);
                if let Err(err) = self.settle_send_btc_order(state, event).await {
                    tracing::error!("settle_send_btc_order failed: {}", err);
                }
                Ok(())
            }
            CchMessage::SettleReceiveBTCOrder(event) => {
                tracing::debug!("settle_receive_btc_order {:?}", event);
                if let Err(err) = self.settle_receive_btc_order(state, event).await {
                    tracing::error!("settle_receive_btc_order failed: {}", err);
                }
                Ok(())
            }
            CchMessage::StoreUpdatedEvent(event) => {
                tracing::debug!(
                    store_updated_event = ?event,
                    "Cch actor received store updated event"
                );
                if let Err(err) = self.handle_store_updated_event(state, event).await {
                    tracing::error!("handle_store_updated_event failed: {}", err);
                }
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
    ) -> Self {
        Self {
            config,
            tracker,
            token,
            network_actor,
            pubkey,
        }
    }

    async fn send_btc(
        &self,
        state: &mut CchState,
        send_btc: SendBTC,
    ) -> Result<SendBTCOrder, CchError> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        let invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        tracing::debug!("BTC invoice: {:?}", invoice);

        let expiry = invoice
            .expires_at()
            .and_then(|expired_at| expired_at.checked_sub(duration_since_epoch))
            .map(|duration| duration.as_secs())
            .ok_or(CchError::BTCInvoiceExpired)?;

        let amount_msat = invoice
            .amount_milli_satoshis()
            .ok_or(CchError::BTCInvoiceMissingAmount)? as u128;

        let fee_sats = amount_msat * (self.config.fee_rate_per_million_sats as u128)
            / 1_000_000_000u128
            + (self.config.base_fee_sats as u128);

        let wrapped_btc_type_script: ckb_jsonrpc_types::Script =
            self.config.get_wrapped_btc_script().into();
        let mut order = SendBTCOrder {
            expires_after: expiry,
            wrapped_btc_type_script,
            fee_sats,
            currency: send_btc.currency,
            created_at: duration_since_epoch.as_secs(),
            ckb_final_tlc_expiry_delta: self.config.ckb_final_tlc_expiry_delta,
            btc_pay_req: send_btc.btc_pay_req,
            fiber_payee_pubkey: self.pubkey,
            fiber_pay_invoice: Default::default(),
            payment_hash: format!("0x{}", invoice.payment_hash().encode_hex::<String>()),
            payment_preimage: None,
            amount_sats: amount_msat.div_ceil(1_000u128) + fee_sats,
            status: CchOrderStatus::Pending,
        };
        let fiber_invoice = order.generate_ckb_invoice()?;

        let message = move |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::AddInvoice(
                fiber_invoice.clone(),
                None,
                rpc_reply,
            ))
        };

        call!(&self.network_actor, message).expect("call actor")?;

        state.orders_db.insert_send_btc_order(order.clone()).await?;

        Ok(order)
    }

    async fn handle_store_updated_event(
        &self,
        state: &mut CchState,
        event: StoreUpdatedEvent,
    ) -> Result<()> {
        match event {
            StoreUpdatedEvent::InvoiceUpdated(invoice_updated_event) => {
                self.handle_invoice_updated_event(state, invoice_updated_event)
                    .await?;
            }
            StoreUpdatedEvent::PaymentUpdated(payment_updated_event) => {
                self.handle_payment_updated_event(state, payment_updated_event)
                    .await?;
            }
        }
        Ok(())
    }

    // On receiving new TLC, check whether it matches the SendBTC order
    async fn handle_invoice_updated_event(
        &self,
        state: &mut CchState,
        event: InvoiceUpdatedEvent,
    ) -> Result<()> {
        if !matches!(
            event.payload,
            InvoiceUpdatedPayload::Received {
                is_finished: true,
                ..
            }
        ) {
            // TODO: handle other states
            return Ok(());
        }
        let payment_hash = format!("{:#x}", event.invoice_hash);
        tracing::debug!("[inbounding tlc] payment hash: {}", payment_hash);

        let mut order = match state.orders_db.get_send_btc_order(&payment_hash).await {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(order) => order,
        };

        if order.status != CchOrderStatus::Pending {
            return Err(CchError::SendBTCOrderAlreadyPaid.into());
        }

        order.status = CchOrderStatus::Accepted;
        state.orders_db.update_send_btc_order(order.clone()).await?;

        let req = routerrpc::SendPaymentRequest {
            payment_request: order.btc_pay_req.clone(),
            timeout_seconds: BTC_PAYMENT_TIMEOUT_SECONDS,
            ..Default::default()
        };
        tracing::debug!("[inbounding tlc] SendPaymentRequest: {:?}", req);

        let mut client = state.lnd_connection.create_router_client().await?;
        // TODO: set a fee
        let mut stream = client.send_payment_v2(req).await?.into_inner();
        // Wait for the first message then quit
        select! {
            payment_result_opt = stream.next() => {
                tracing::debug!("[inbounding tlc] payment result: {:?}", payment_result_opt);
                if let Some(Ok(payment)) = payment_result_opt {
                    // TODO: the payment result here may indicate a failure, we need to handle it
                    order.status = lnrpc::payment::PaymentStatus::try_from(payment.status)?.into();
                    state.orders_db
                        .update_send_btc_order(order)
                        .await?;
                }
            }
            _ = self.token.cancelled() => {
                tracing::debug!("Cancellation received, shutting down cch service");
                return Ok(());
            }
        }

        Ok(())
    }

    async fn handle_payment_updated_event(
        &self,
        state: &mut CchState,
        event: PaymentUpdatedEvent,
    ) -> Result<()> {
        let payment_hash = format!("{:#x}", event.payment_hash);
        tracing::debug!("[settled tlc] payment hash: {}", payment_hash);

        match state.orders_db.get_receive_btc_order(&payment_hash).await {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            _ => {
                // ignore
            }
        };

        let preimage = match event.payload {
            PaymentUpdatedPayload::Success { preimage } => preimage,
            PaymentUpdatedPayload::Failed => {
                // TODO: handle failed payment
                return Ok(());
            }
            _ => {
                tracing::debug!("Ignore payment update");
                return Ok(());
            }
        };

        tracing::debug!("[settled tlc] preimage: {:#x}", preimage);

        // settle the lnd invoice
        let req = invoicesrpc::SettleInvoiceMsg {
            preimage: preimage.as_ref().to_vec(),
        };
        tracing::debug!("[settled tlc] SettleInvoiceMsg: {:?}", req);

        let mut client = state.lnd_connection.create_invoices_client().await?;
        // TODO: set a fee
        let resp = client.settle_invoice(req).await?.into_inner();
        tracing::debug!("[settled tlc] SettleInvoiceResp: {:?}", resp);

        Ok(())
    }

    async fn settle_send_btc_order(
        &self,
        state: &mut CchState,
        event: SettleSendBTCOrderEvent,
    ) -> Result<()> {
        let mut order = match state
            .orders_db
            .get_send_btc_order(&event.payment_hash)
            .await
        {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(order) => order,
        };

        let hash = Hash256::from_str(&event.payment_hash)?;

        order.status = event.status;
        if let Some(preimage_str) = event.preimage {
            tracing::info!(
                "SettleSendBTCOrder: payment_hash={}, status={:?}",
                event.payment_hash,
                event.status
            );
            let preimage = Hash256::from_str(&preimage_str)?;
            order.payment_preimage = Some(preimage_str);

            let message = move |rpc_reply| -> NetworkActorMessage {
                NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                    hash, preimage, rpc_reply,
                ))
            };

            call!(&self.network_actor, message)
                .expect("call actor")
                .map_err(|msg| anyhow!(msg))?;
        }

        state.orders_db.update_send_btc_order(order).await?;

        Ok(())
    }

    async fn receive_btc(
        &self,
        myself: ActorRef<CchMessage>,
        state: &mut CchState,
        receive_btc: ReceiveBTC,
    ) -> Result<ReceiveBTCOrder, CchError> {
        let invoice = CkbInvoice::from_str(&receive_btc.fiber_pay_req)?;
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
            return Err(CchError::ReceiveBTCOrderAmountTooSmall);
        }
        if amount_sats > (i64::MAX / 1_000i64) as u128 {
            return Err(CchError::ReceiveBTCOrderAmountTooLarge);
        }

        let mut client = state.lnd_connection.create_invoices_client().await?;
        let req = invoicesrpc::AddHoldInvoiceRequest {
            hash: payment_hash.into(),
            value_msat: (amount_sats * 1_000u128) as i64,
            expiry: DEFAULT_ORDER_EXPIRY_SECONDS as i64,
            cltv_expiry: self.config.btc_final_tlc_expiry + final_tlc_minimum_expiry_delta,
            ..Default::default()
        };
        let add_invoice_resp = client
            .add_hold_invoice(req)
            .await
            .map_err(|err| CchError::LndRpcError(err.to_string()))?
            .into_inner();
        let btc_pay_req = add_invoice_resp.payment_request;

        let wrapped_btc_type_script: ckb_jsonrpc_types::Script =
            self.config.get_wrapped_btc_script().into();
        let order = ReceiveBTCOrder {
            created_at: duration_since_epoch.as_secs(),
            expires_after: DEFAULT_ORDER_EXPIRY_SECONDS,
            ckb_final_tlc_expiry_delta: final_tlc_minimum_expiry_delta,
            btc_pay_req,
            fiber_pay_req: receive_btc.fiber_pay_req,
            payment_hash: payment_hash_str.clone(),
            payment_preimage: None,
            amount_sats,
            fee_sats,
            status: CchOrderStatus::Pending,
            wrapped_btc_type_script,
        };

        state
            .orders_db
            .insert_receive_btc_order(order.clone())
            .await?;

        let invoice_tracker = LndInvoiceTracker::new(
            myself,
            payment_hash_str,
            state.lnd_connection.clone(),
            self.token.clone(),
        );
        self.tracker
            .spawn(async move { invoice_tracker.run().await });

        Ok(order)
    }

    async fn settle_receive_btc_order(
        &self,
        state: &mut CchState,
        event: SettleReceiveBTCOrderEvent,
    ) -> Result<()> {
        let mut order = match state
            .orders_db
            .get_receive_btc_order(&event.payment_hash)
            .await
        {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(order) => order,
        };
        order.status = event.status;
        order.payment_preimage = event.preimage.clone();

        if event.status == CchOrderStatus::Accepted {
            tracing::debug!(
                payment_hash = ?event.payment_hash,
                "Sending payment to fiber node because we received payment from LND",
            );
            let message = |rpc_reply| -> NetworkActorMessage {
                NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                    SendPaymentCommand {
                        invoice: Some(order.fiber_pay_req.clone()),
                        ..Default::default()
                    },
                    rpc_reply,
                ))
            };

            // TODO: handle payment failure here.
            let tlc_response = call!(self.network_actor, message)
                .expect("call actor")
                .map_err(|msg| anyhow!(msg))?;
            // TODO: handle payment failure here.
            if tlc_response.status == PaymentSessionStatus::Failed {
                order.status = CchOrderStatus::Failed;
            }
        }

        state
            .orders_db
            .update_receive_btc_order(order.clone())
            .await?;
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
        let event = CchMessage::SettleSendBTCOrder(SettleSendBTCOrderEvent {
            payment_hash: format!("0x{}", payment.payment_hash),
            preimage: (!payment.payment_preimage.is_empty())
                .then(|| format!("0x{}", payment.payment_preimage)),
            status: lnrpc::payment::PaymentStatus::try_from(payment.status)
                .map(Into::into)
                .unwrap_or(CchOrderStatus::InFlight),
        });
        self.cch_actor.cast(event).map_err(Into::into)
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
        let status = lnrpc::invoice::InvoiceState::try_from(invoice.state)
            .map(Into::into)
            .unwrap_or(CchOrderStatus::Pending);
        let event = CchMessage::SettleReceiveBTCOrder(SettleReceiveBTCOrderEvent {
            payment_hash: format!("0x{}", hex::encode(invoice.r_hash)),
            preimage: (!invoice.r_preimage.is_empty())
                .then(|| format!("0x{}", hex::encode(invoice.r_preimage))),
            status,
        });
        self.cch_actor.cast(event)?;
        // Quit tracker when the status is final
        Ok(status == CchOrderStatus::Succeeded || status == CchOrderStatus::Failed)
    }
}
