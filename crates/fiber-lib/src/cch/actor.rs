use anyhow::{anyhow, Context, Result};
use futures::StreamExt as _;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{
    create_invoices_client, create_router_client, invoicesrpc, lnrpc, routerrpc, InvoicesClient,
    RouterClient, Uri,
};
use ractor::{call, RpcReplyPort};
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::Deserialize;
use std::str::FromStr;
use tentacle::secio::SecioKeyPair;
use tokio::{select, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::cch::order::CchInvoice;
use crate::ckb::contracts::{get_script_by_contract, Contract};
use crate::fiber::channel::TlcNotification;
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::payment::PaymentStatus;
use crate::fiber::types::{Hash256, Privkey};
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::time::{Duration, SystemTime, UNIX_EPOCH};

use super::error::CchDbError;
use super::{CchConfig, CchError, CchOrder, CchOrderStatus, CchOrdersDb};

pub const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;
pub const DEFAULT_ORDER_EXPIRY_SECONDS: u64 = 86400; // 24 hours

pub async fn start_cch(args: CchArgs, root_actor: ActorCell) -> Result<ActorRef<CchMessage>> {
    let (actor, _handle) =
        Actor::spawn_linked(Some("cch actor".to_string()), CchActor, args, root_actor).await?;
    Ok(actor)
}

#[derive(Debug)]
pub struct SettleSendBTCOrderEvent {
    payment_hash: Hash256,
    preimage: Option<Hash256>,
    status: CchOrderStatus,
}

#[derive(Debug)]
pub struct SettleReceiveBTCOrderEvent {
    payment_hash: Hash256,
    preimage: Option<Hash256>,
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
    SendBTC(SendBTC, RpcReplyPort<Result<CchOrder, CchError>>),
    ReceiveBTC(ReceiveBTC, RpcReplyPort<Result<CchOrder, CchError>>),

    GetCchOrder(Hash256, RpcReplyPort<Result<CchOrder, CchError>>),

    SettleSendBTCOrder(SettleSendBTCOrderEvent),
    SettleReceiveBTCOrder(SettleReceiveBTCOrderEvent),

    PendingReceivedTlcNotification(TlcNotification),
    SettledTlcNotification(TlcNotification),
}

#[derive(Clone)]
struct LndConnectionInfo {
    uri: Uri,
    cert: Option<Vec<u8>>,
    macaroon: Option<Vec<u8>>,
}

impl LndConnectionInfo {
    async fn create_router_client(
        &self,
    ) -> Result<RouterClient, lnd_grpc_tonic_client::channel::Error> {
        create_router_client(
            self.uri.clone(),
            self.cert.as_deref(),
            self.macaroon.as_deref(),
        )
        .await
    }

    async fn create_invoices_client(
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

#[derive(Default)]
pub struct CchActor;

pub struct CchArgs {
    pub config: CchConfig,
    pub tracker: TaskTracker,
    pub token: CancellationToken,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub node_keypair: crate::fiber::KeyPair,
}

pub struct CchState {
    config: CchConfig,
    tracker: TaskTracker,
    token: CancellationToken,
    network_actor: ActorRef<NetworkActorMessage>,
    node_keypair: (PublicKey, SecretKey),
    lnd_connection: LndConnectionInfo,
    orders_db: CchOrdersDb,
}

#[async_trait::async_trait]
impl Actor for CchActor {
    type Msg = CchMessage;
    type State = CchState;
    type Arguments = CchArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let lnd_rpc_url: Uri = args.config.lnd_rpc_url.clone().try_into()?;
        let cert = match args.config.resolve_lnd_cert_path() {
            Some(path) => Some(
                tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("read cert file {}", path.display()))?,
            ),
            None => None,
        };
        let macaroon = match args.config.resolve_lnd_macaroon_path() {
            Some(path) => Some(
                tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("read macaroon file {}", path.display()))?,
            ),
            None => None,
        };
        let lnd_connection = LndConnectionInfo {
            uri: lnd_rpc_url,
            cert,
            macaroon,
        };

        let private_key: Privkey = <[u8; 32]>::try_from(args.node_keypair.as_ref())
            .expect("valid length for key")
            .into();
        let secio_kp = SecioKeyPair::from(args.node_keypair);

        let node_keypair = (
            PublicKey::from_slice(secio_kp.public_key().inner_ref()).expect("valid public key"),
            private_key.into(),
        );

        let state = CchState {
            config: args.config,
            tracker: args.tracker,
            token: args.token,
            network_actor: args.network_actor,
            orders_db: Default::default(),
            node_keypair,
            lnd_connection,
        };

        let payments_tracker = LndPaymentsTracker::new(
            myself.clone(),
            state.lnd_connection.clone(),
            state.token.clone(),
        );
        state
            .tracker
            .spawn(async move { payments_tracker.run().await });

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
                let result = state.send_btc(send_btc).await;
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::ReceiveBTC(receive_btc, port) => {
                let result = state.receive_btc(myself, receive_btc).await;
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::GetCchOrder(payment_hash, port) => {
                let result = state
                    .orders_db
                    .get_cch_order(&payment_hash)
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
                if let Err(err) = state.settle_send_btc_order(event).await {
                    tracing::error!("settle_send_btc_order failed: {}", err);
                }
                Ok(())
            }
            CchMessage::SettleReceiveBTCOrder(event) => {
                tracing::debug!("settle_receive_btc_order {:?}", event);
                if let Err(err) = state.settle_receive_btc_order(event).await {
                    tracing::error!("settle_receive_btc_order failed: {}", err);
                }
                Ok(())
            }
            CchMessage::PendingReceivedTlcNotification(tlc_notification) => {
                if let Err(err) = state
                    .handle_pending_received_tlc_notification(tlc_notification)
                    .await
                {
                    tracing::error!("handle_pending_received_tlc_notification failed: {}", err);
                }
                Ok(())
            }
            CchMessage::SettledTlcNotification(tlc_notification) => {
                if let Err(err) = state
                    .handle_settled_tlc_notification(tlc_notification)
                    .await
                {
                    tracing::error!("handle_settled_tlc_notification failed: {}", err);
                }
                Ok(())
            }
        }
    }
}

impl CchState {
    async fn send_btc(&mut self, send_btc: SendBTC) -> Result<CchOrder, CchError> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        let invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        tracing::debug!("BTC invoice: {:?}", invoice);
        let payment_hash = (*invoice.payment_hash()).into();

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

        let wrapped_btc_type_script: ckb_jsonrpc_types::Script = get_script_by_contract(
            Contract::SimpleUDT,
            hex::decode(
                self.config
                    .wrapped_btc_type_script_args
                    .trim_start_matches("0x"),
            )
            .map_err(|_| {
                CchError::HexDecodingError(self.config.wrapped_btc_type_script_args.clone())
            })?
            .as_ref(),
        )
        .into();
        let invoice_amount_sats = amount_msat.div_ceil(1_000u128) + fee_sats;

        let invoice = InvoiceBuilder::new(send_btc.currency)
            .amount(Some(invoice_amount_sats))
            .payment_hash(payment_hash)
            .hash_algorithm(HashAlgorithm::Sha256)
            .expiry_time(Duration::from_secs(expiry))
            .final_expiry_delta(self.config.ckb_final_tlc_expiry_delta)
            .udt_type_script(wrapped_btc_type_script.clone().into())
            .payee_pub_key(self.node_keypair.0)
            .build_with_sign(|hash| {
                Secp256k1::new().sign_ecdsa_recoverable(hash, &self.node_keypair.1)
            })?;

        let message = {
            let invoice = invoice.clone();
            move |rpc_reply| -> NetworkActorMessage {
                NetworkActorMessage::Command(NetworkActorCommand::AddInvoice(
                    invoice.clone(),
                    None,
                    rpc_reply,
                ))
            }
        };
        call!(self.network_actor, message).expect("call actor")?;

        let order = CchOrder {
            wrapped_btc_type_script,
            fee_sats,
            payment_hash,
            expires_after: expiry,
            created_at: duration_since_epoch.as_secs(),
            ckb_final_tlc_expiry_delta: self.config.ckb_final_tlc_expiry_delta,
            outgoing_pay_req: send_btc.btc_pay_req,
            incoming_invoice: CchInvoice::Fiber(invoice),
            payment_preimage: None,
            amount_sats: invoice_amount_sats,
            status: CchOrderStatus::Pending,
        };

        self.orders_db.insert_cch_order(order.clone()).await?;
        // TODO(now): save order and invoice into db: store.insert_invoice(invoice.clone())

        Ok(order)
    }

    // On receiving new TLC, check whether it matches the SendBTC order
    async fn handle_pending_received_tlc_notification(
        &mut self,
        tlc_notification: TlcNotification,
    ) -> Result<()> {
        let payment_hash = tlc_notification.tlc.payment_hash;
        tracing::debug!("[inbounding tlc] payment hash: {}", payment_hash);

        let mut order = match self.orders_db.get_cch_order(&payment_hash).await {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(order) if order.is_from_fiber_to_lightning() => order,
            // Ignore if the order is not from fiber to lightning
            Ok(_) => return Ok(()),
        };

        if order.status != CchOrderStatus::Pending {
            return Err(CchError::SendBTCOrderAlreadyPaid.into());
        }

        if tlc_notification.tlc.amount < order.amount_sats {
            // TODO: split the payment into multiple parts
            return Err(CchError::SendBTCReceivedAmountTooSmall.into());
        }

        order.status = CchOrderStatus::IncomingAccepted;
        self.orders_db.update_cch_order(order.clone()).await?;

        let req = routerrpc::SendPaymentRequest {
            payment_request: order.outgoing_pay_req.clone(),
            timeout_seconds: BTC_PAYMENT_TIMEOUT_SECONDS,
            ..Default::default()
        };
        tracing::debug!("[inbounding tlc] SendPaymentRequest: {:?}", req);

        let mut client = self.lnd_connection.create_router_client().await?;
        // TODO: set a fee
        let mut stream = client.send_payment_v2(req).await?.into_inner();
        // Wait for the first message then quit
        select! {
            payment_result_opt = stream.next() => {
                tracing::debug!("[inbounding tlc] payment result: {:?}", payment_result_opt);
                if let Some(Ok(payment)) = payment_result_opt {
                    order.status = lnrpc::payment::PaymentStatus::try_from(payment.status)?.into();
                    self.orders_db
                        .update_cch_order(order)
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

    async fn handle_settled_tlc_notification(
        &mut self,
        tlc_notification: TlcNotification,
    ) -> Result<()> {
        let payment_hash = tlc_notification.tlc.payment_hash;
        tracing::debug!("[settled tlc] payment hash: {}", payment_hash);

        let mut order = match self.orders_db.get_cch_order(&payment_hash).await {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            // Ignore if the order is from fiber to lightning
            Ok(order) if order.is_from_fiber_to_lightning() => return Ok(()),
            Ok(order) => order,
        };

        let preimage = tlc_notification
            .tlc
            .payment_preimage
            .ok_or(CchError::ReceiveBTCMissingPreimage)?;

        tracing::debug!("[settled tlc] preimage: {:#x}", preimage);

        order.status = CchOrderStatus::OutgoingSettled;
        order.payment_preimage = Some(preimage);
        self.orders_db.update_cch_order(order.clone()).await?;

        // settle the lnd invoice
        let req = invoicesrpc::SettleInvoiceMsg {
            preimage: preimage.into(),
        };
        tracing::debug!("[settled tlc] SettleInvoiceMsg: {:?}", req);

        let mut client = self.lnd_connection.create_invoices_client().await?;
        // TODO: set a fee
        let resp = client.settle_invoice(req).await?.into_inner();
        tracing::debug!("[settled tlc] SettleInvoiceResp: {:?}", resp);

        order.status = CchOrderStatus::Succeeded;
        self.orders_db.update_cch_order(order.clone()).await?;

        Ok(())
    }

    async fn settle_send_btc_order(&mut self, event: SettleSendBTCOrderEvent) -> Result<()> {
        let payment_hash = event.payment_hash;

        let mut order = match self.orders_db.get_cch_order(&event.payment_hash).await {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(order) if !order.is_from_fiber_to_lightning() => return Ok(()),
            Ok(order) => order,
        };

        order.status = event.status;
        if let Some(preimage) = event.preimage {
            tracing::info!(
                "SettleSendBTCOrder: payment_hash={}, status={:?}",
                payment_hash,
                event.status
            );
            order.payment_preimage = Some(preimage);
            self.orders_db.update_cch_order(order.clone()).await?;

            let command = move |rpc_reply| -> NetworkActorMessage {
                NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                    payment_hash,
                    preimage,
                    rpc_reply,
                ))
            };
            call!(self.network_actor, command).expect("call actor")?;
            order.status = CchOrderStatus::Succeeded;
        }

        self.orders_db.update_cch_order(order).await?;

        Ok(())
    }

    async fn receive_btc(
        &mut self,
        myself: ActorRef<CchMessage>,
        receive_btc: ReceiveBTC,
    ) -> Result<CchOrder, CchError> {
        let invoice = CkbInvoice::from_str(&receive_btc.fiber_pay_req)?;
        let payment_hash = *invoice.payment_hash();
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

        let mut client = self.lnd_connection.create_invoices_client().await?;
        let req = invoicesrpc::AddHoldInvoiceRequest {
            hash: payment_hash.as_ref().to_vec(),
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
        let incoming_invoice = Bolt11Invoice::from_str(&add_invoice_resp.payment_request)?;

        let wrapped_btc_type_script: ckb_jsonrpc_types::Script = get_script_by_contract(
            Contract::SimpleUDT,
            hex::decode(
                self.config
                    .wrapped_btc_type_script_args
                    .trim_start_matches("0x"),
            )
            .map_err(|_| {
                CchError::HexDecodingError(self.config.wrapped_btc_type_script_args.clone())
            })?
            .as_ref(),
        )
        .into();
        let order = CchOrder {
            created_at: duration_since_epoch.as_secs(),
            expires_after: DEFAULT_ORDER_EXPIRY_SECONDS,
            ckb_final_tlc_expiry_delta: final_tlc_minimum_expiry_delta,
            outgoing_pay_req: receive_btc.fiber_pay_req,
            incoming_invoice: CchInvoice::Lightning(incoming_invoice),
            payment_hash,
            payment_preimage: None,
            amount_sats,
            fee_sats,
            status: CchOrderStatus::Pending,
            wrapped_btc_type_script,
        };

        self.orders_db.insert_cch_order(order.clone()).await?;

        let invoice_tracker = LndInvoiceTracker::new(
            myself,
            payment_hash,
            self.lnd_connection.clone(),
            self.token.clone(),
        );
        self.tracker
            .spawn(async move { invoice_tracker.run().await });

        Ok(order)
    }

    async fn settle_receive_btc_order(&mut self, event: SettleReceiveBTCOrderEvent) -> Result<()> {
        let mut order = match self.orders_db.get_cch_order(&event.payment_hash).await {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(order) if order.is_from_fiber_to_lightning() => return Ok(()),
            Ok(order) => order,
        };

        order.status = event.status;
        order.payment_preimage = event.preimage;

        if event.status == CchOrderStatus::IncomingAccepted {
            let message = |rpc_reply| -> NetworkActorMessage {
                NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                    SendPaymentCommand {
                        invoice: Some(order.outgoing_pay_req.clone()),
                        ..Default::default()
                    },
                    rpc_reply,
                ))
            };

            let payment_status = call!(self.network_actor, message)
                .expect("call actor")
                .map_err(|err| anyhow!("{}", err))?
                .status;

            let mut order = order.clone();
            if payment_status == PaymentStatus::Failed {
                order.status = CchOrderStatus::Failed;
            } else {
                order.status = CchOrderStatus::OutgoingInFlight;
            }
        }

        self.orders_db.update_cch_order(order.clone()).await?;
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
        let preimage = if !payment.payment_preimage.is_empty() {
            Some(Hash256::from_str(&payment.payment_preimage)?)
        } else {
            None
        };

        let event = CchMessage::SettleSendBTCOrder(SettleSendBTCOrderEvent {
            payment_hash: Hash256::from_str(&payment.payment_hash)?,
            preimage,
            status: lnrpc::payment::PaymentStatus::try_from(payment.status)
                .map(Into::into)
                .unwrap_or(CchOrderStatus::OutgoingInFlight),
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
    payment_hash: Hash256,
    lnd_connection: LndConnectionInfo,
    token: CancellationToken,
}

impl LndInvoiceTracker {
    fn new(
        cch_actor: ActorRef<CchMessage>,
        payment_hash: Hash256,
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
                r_hash: self.payment_hash.into(),
            })
            .await?
            .into_inner();

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
        let preimage = if !invoice.r_preimage.is_empty() {
            Some(Hash256::try_from(invoice.r_preimage.as_slice())?)
        } else {
            None
        };
        let event = CchMessage::SettleReceiveBTCOrder(SettleReceiveBTCOrderEvent {
            payment_hash: Hash256::try_from(invoice.r_hash.as_slice())?,
            preimage,
            status,
        });
        self.cch_actor.cast(event)?;
        // Quit tracker when the status is final
        Ok(status == CchOrderStatus::Succeeded || status == CchOrderStatus::Failed)
    }
}
