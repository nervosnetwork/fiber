#![allow(dead_code, unreachable_code)]

use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use hex::ToHex;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{
    create_invoices_client, create_router_client, invoicesrpc, lnrpc, routerrpc, InvoicesClient,
    RouterClient, Uri,
};
use std::collections::HashSet;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{select, sync::mpsc, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::command::{ReceiveBTC, TestPayBTC};
use super::error::CchDbError;
use super::{
    CchCommand, CchConfig, CchError, CchOrderStatus, CchOrdersDb, ReceiveBTCOrder, SendBTC,
    SendBTCOrder,
};

pub const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;
pub const DEFAULT_ORDER_EXPIRY_SECONDS: u64 = 86400; // 24 hours

pub async fn start_cch(
    config: CchConfig,
    command_receiver: mpsc::Receiver<CchCommand>,
    token: CancellationToken,
    tracker: TaskTracker,
) -> Result<()> {
    const CHANNEL_SIZE: usize = 4000;
    let (event_sender, event_receiver) = mpsc::channel(CHANNEL_SIZE);

    let lnd_rpc_url: Uri = config.lnd_rpc_url.clone().try_into()?;
    let cert = match config.resolve_lnd_cert_path() {
        Some(path) => Some(tokio::fs::read(path).await?),
        None => None,
    };
    let macaroon = tokio::fs::read(config.resolve_lnd_macaroon_path()).await?;
    let lnd_connection = LndConnectionInfo {
        uri: lnd_rpc_url,
        cert,
        macaroon,
    };

    let service = CchService {
        config,
        command_receiver,
        token,
        tracker,
        lnd_connection,
        event_receiver,
        event_sender,
        orders_db: Default::default(),
    };
    service.spawn();

    Ok(())
}

#[derive(Debug)]
struct SettleSendBTCOrderEvent {
    pub payment_hash: String,
    pub preimage: Option<String>,
    pub status: CchOrderStatus,
}

#[derive(Debug)]
struct SettleReceiveBTCOrderEvent {
    pub payment_hash: String,
    pub preimage: Option<String>,
    pub status: CchOrderStatus,
}

#[derive(Debug)]
enum Event {
    SettleSendBTCOrder(SettleSendBTCOrderEvent),
    SettleReceiveBTCOrder(SettleReceiveBTCOrderEvent),
}

struct CchService {
    config: CchConfig,
    command_receiver: mpsc::Receiver<CchCommand>,
    token: CancellationToken,
    tracker: TaskTracker,
    lnd_connection: LndConnectionInfo,
    event_receiver: mpsc::Receiver<Event>,
    event_sender: mpsc::Sender<Event>,
    orders_db: CchOrdersDb,
}

impl CchService {
    // TODO: setup tracking on existing orders on startup
    pub fn spawn(self) {
        let tracker = self.tracker.clone();
        let lnd_connection = self.lnd_connection.clone();
        let event_sender = self.event_sender.clone();
        let token = self.token.clone();

        tracker.spawn(async move {
            self.run().await;
        });

        let payments_tracker = LndPaymentsTracker::new(lnd_connection, event_sender, token);
        tracker.spawn(async move { payments_tracker.run().await });

        // TODO: start a task to cleanup expired orders
    }

    pub async fn run(mut self) {
        loop {
            select! {
                _ = self.token.cancelled() => {
                    log::debug!("Cancellation received, shutting down cch service");
                    break;
                }
                command = self.command_receiver.recv() => {
                    match command {
                        None => {
                            log::debug!("Command receiver completed, shutting down tentacle service");
                            break;
                        }
                        Some(command) => {
                            let command_name = command.name();
                            log::info!("Process cch command {}", command_name);

                            if let Err(err) = self.process_command(command).await {
                                log::error!("Error processing command {}: {:?}", command_name, err);
                            }
                        }
                    }
                }
                event = self.event_receiver.recv() => {
                    match event {
                        None => {
                            log::debug!("Event receiver completed, shutting down tentacle service");
                            break;
                        }
                        Some(event) => {
                            if let Err(err) = self.process_event(event).await {
                                log::error!("Error processing event: {:?}", err);
                            }
                        }
                    }
                }
            }
        }
    }

    async fn process_command(&mut self, command: CchCommand) -> Result<()> {
        log::debug!("CchCommand received: {:?}", command);
        match command {
            CchCommand::SendBTC(send_btc) => self.send_btc(send_btc).await,
            CchCommand::TestPayBTC(test_pay_btc) => self.test_pay_btc(test_pay_btc).await,
            CchCommand::ReceiveBTC(receive_btc) => self.receive_btc(receive_btc).await,
        }
    }

    async fn send_btc(&mut self, send_btc: SendBTC) -> Result<()> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        let invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        log::debug!("BTC invoice: {:?}", invoice);

        let expiry = invoice
            .expires_at()
            .and_then(|expired_at| expired_at.checked_sub(duration_since_epoch))
            .map(|duration| duration.as_secs())
            .or_else(|| {
                self.config
                    .allow_expired_btc_invoice
                    .then_some(self.config.order_expiry)
            })
            .ok_or(CchError::BTCInvoiceExpired)?;

        let amount_msat = invoice
            .amount_milli_satoshis()
            .ok_or(CchError::BTCInvoiceMissingAmount)?;

        log::debug!("SendBTC expiry: {:?}", expiry);
        let (ratio_ckb_shannons, ratio_btc_msat) =
            match (self.config.ratio_ckb_shannons, self.config.ratio_btc_msat) {
                (Some(ratio_ckb_shannons), Some(ratio_btc_msat)) => {
                    (ratio_ckb_shannons, ratio_btc_msat)
                }
                _ => return Err(CchError::CKBAssetNotAllowed.into()),
            };
        let order_value = ((ratio_ckb_shannons as u128) * (amount_msat as u128)
            / (ratio_btc_msat as u128)) as u64;
        let fee = order_value * self.config.fee_rate_per_million_shannons / 1_000_000
            + self.config.base_fee_shannons;

        let order = SendBTCOrder {
            timestamp: duration_since_epoch.as_secs(),
            expiry,
            ckb_final_tlc_expiry: self.config.ckb_final_tlc_expiry,
            btc_pay_req: send_btc.btc_pay_req,
            payment_hash: invoice.payment_hash().encode_hex(),
            payment_preimage: None,
            amount_shannons: order_value + fee,
            status: CchOrderStatus::Pending,
        };

        // TODO(cch): Return it as the RPC response
        log::info!("SendBTCOrder: {}", serde_json::to_string(&order)?);
        self.orders_db.insert_send_btc_order(order).await?;

        Ok(())
    }

    async fn test_pay_btc(&mut self, test_pay_btc: TestPayBTC) -> Result<()> {
        let mut payment_hashes = HashSet::new();
        payment_hashes.insert(test_pay_btc.payment_hash.clone());

        let order = self
            .orders_db
            .get_send_btc_order(&test_pay_btc.payment_hash)
            .await?;
        if order.status != CchOrderStatus::Pending {
            return Err(CchError::SendBTCOrderAlreadyPaid.into());
        }

        let req = routerrpc::SendPaymentRequest {
            payment_request: order.btc_pay_req,
            timeout_seconds: BTC_PAYMENT_TIMEOUT_SECONDS,
            ..Default::default()
        };
        log::debug!("[test_pay_btc] SendPaymentRequest: {:?}", req);

        let mut client = self.lnd_connection.create_router_client().await?;
        // TODO: set a fee
        let mut stream = client.send_payment_v2(req).await?.into_inner();
        // Wait for the first message then quit
        select! {
            payment_result_opt = stream.next() => {
                log::debug!("[test_pay_btc] payment result: {:?}", payment_result_opt);
                if let Some(Ok(payment)) = payment_result_opt {
                    self.orders_db
                        .update_send_btc_order(
                            &test_pay_btc.payment_hash,
                            None,
                            lnrpc::payment::PaymentStatus::try_from(payment.status)?.into(),
                        )
                        .await?;
                }
            }
            _ = self.token.cancelled() => {
                log::debug!("Cancellation received, shutting down cch service");
                return Ok(());
            }
        }

        Ok(())
    }

    async fn receive_btc(&mut self, receive_btc: ReceiveBTC) -> Result<()> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let hash_bin = hex::decode(&receive_btc.payment_hash)?;

        let (ratio_ckb_shannons, ratio_btc_msat) =
            match (self.config.ratio_ckb_shannons, self.config.ratio_btc_msat) {
                (Some(ratio_ckb_shannons), Some(ratio_btc_msat)) => {
                    (ratio_ckb_shannons, ratio_btc_msat)
                }
                _ => return Err(CchError::CKBAssetNotAllowed.into()),
            };
        let order_value = ((ratio_ckb_shannons as u128) * (receive_btc.amount_msat as u128)
            / (ratio_btc_msat as u128)) as u64;
        let fee = order_value * self.config.fee_rate_per_million_shannons / 1_000_000
            + self.config.base_fee_shannons;
        // TODO: check that the amount is larger than the minimal allowed CKB payment.
        let amount_shannons = order_value
            .checked_sub(fee)
            .ok_or(CchError::ReceiveBTCOrderAmountTooSmall)?;

        let mut client = self.lnd_connection.create_invoices_client().await?;
        let req = invoicesrpc::AddHoldInvoiceRequest {
            hash: hash_bin,
            value_msat: receive_btc.amount_msat as i64,
            expiry: DEFAULT_ORDER_EXPIRY_SECONDS as i64,
            cltv_expiry: self.config.btc_final_tlc_expiry + receive_btc.final_tlc_expiry,
            ..Default::default()
        };
        let invoice = client.add_hold_invoice(req).await?.into_inner();
        let btc_pay_req = invoice.payment_request;

        let order = ReceiveBTCOrder {
            timestamp: duration_since_epoch.as_secs(),
            expiry: DEFAULT_ORDER_EXPIRY_SECONDS,
            ckb_final_tlc_expiry: receive_btc.final_tlc_expiry,
            btc_pay_req,
            payment_hash: receive_btc.payment_hash.clone(),
            payment_preimage: None,
            amount_shannons,
            // TODO: check whether this node has the peer with this pubkey.
            payee_pubkey: receive_btc.payee_pubkey,
            status: CchOrderStatus::Pending,
        };

        // TODO(cch): Return it as the RPC response
        log::info!("ReceiveBTCOrder: {}", serde_json::to_string(&order)?);
        self.orders_db.insert_receive_btc_order(order).await?;

        let invoice_tracker = LndInvoiceTracker::new(
            receive_btc.payment_hash,
            self.lnd_connection.clone(),
            self.event_sender.clone(),
            self.token.clone(),
        );
        self.tracker
            .spawn(async move { invoice_tracker.run().await });

        Ok(())
    }

    async fn process_event(&mut self, event: Event) -> Result<()> {
        log::debug!("Event received: {:?}", event);
        match event {
            Event::SettleSendBTCOrder(event) => {
                // TODO: settle the received CKB payment using the found preimage
                if event.preimage.is_some() {
                    log::info!(
                        "SettleSendBTCOrder: payment_hash={}, status={:?}",
                        event.payment_hash,
                        event.status
                    );
                }
                match self
                    .orders_db
                    .update_send_btc_order(&event.payment_hash, event.preimage, event.status)
                    .await
                {
                    Err(CchDbError::NotFound(_)) => {
                        // ignore payments not found in the db
                        Ok(())
                    }
                    result => result.map_err(Into::into),
                }
            }
            Event::SettleReceiveBTCOrder(event) => {
                if event.preimage.is_some() {
                    log::info!(
                        "SettleReceiveBTCOrder: payment_hash={}, status={:?}",
                        event.payment_hash,
                        event.status
                    );
                    // TODO: 1. Create a CKB payment to the payee to get preimage when event.status is Accepted
                    // TODO: 2. Subscribe to the CKB payment events, once it's settled, use the preimage to settle the BTC payment via invoicesrpc `settle_invoice`.
                    match self
                        .orders_db
                        .update_receive_btc_order(&event.payment_hash, event.preimage, event.status)
                        .await
                    {
                        Err(CchDbError::NotFound(_)) => {
                            // ignore payments not found in the db
                            Ok(())
                        }
                        result => result.map_err(Into::into),
                    }
                } else {
                    Ok(())
                }
            }
        }
    }
}

struct LndPaymentsTracker {
    lnd_connection: LndConnectionInfo,
    event_sender: mpsc::Sender<Event>,
    token: CancellationToken,
}

impl LndPaymentsTracker {
    fn new(
        lnd_connection: LndConnectionInfo,
        event_sender: mpsc::Sender<Event>,
        token: CancellationToken,
    ) -> Self {
        Self {
            lnd_connection,
            event_sender,
            token,
        }
    }

    async fn run(self) {
        // TODO: clean up expired orders
        loop {
            select! {
                result = self.run_inner() => {
                    match result {
                        Ok(_) => {
                            break;
                        }
                        Err(err) => {
                            log::error!(
                                "Error tracking LND payments, retry 15 seconds later: {:?}",
                                err
                            );
                            select! {
                                _ = sleep(Duration::from_secs(15)) => {
                                    // continue
                                }
                                _ = self.token.cancelled() => {
                                    log::debug!("Cancellation received, shutting down cch service");
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = self.token.cancelled() => {
                    log::debug!("Cancellation received, shutting down cch service");
                    return;
                }
            }
        }
    }

    async fn run_inner(&self) -> Result<()> {
        log::debug!(
            "[LndPaymentsTracker] will connect {}",
            self.lnd_connection.uri
        );
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
                    log::debug!("Cancellation received, shutting down cch service");
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    async fn on_payment(&self, payment: lnrpc::Payment) -> Result<()> {
        log::debug!("[LndPaymentsTracker] payment: {:?}", payment);
        let event = Event::SettleSendBTCOrder(SettleSendBTCOrderEvent {
            payment_hash: payment.payment_hash,
            preimage: (!payment.payment_preimage.is_empty()).then_some(payment.payment_preimage),
            status: lnrpc::payment::PaymentStatus::try_from(payment.status)
                .map(Into::into)
                .unwrap_or(CchOrderStatus::InFlight),
        });
        self.event_sender.send(event).await.map_err(Into::into)
    }
}

/// Subscribe single invoice.
///
/// Lnd does not notify Accepted event in SubscribeInvoices rpc.
///
/// <https://github.com/lightningnetwork/lnd/blob/07b6af41dbe2a5a1c85e5c46cc41019b64640d90/invoices/invoiceregistry.go#L292-L293>
struct LndInvoiceTracker {
    payment_hash: String,
    lnd_connection: LndConnectionInfo,
    event_sender: mpsc::Sender<Event>,
    token: CancellationToken,
}

impl LndInvoiceTracker {
    fn new(
        payment_hash: String,
        lnd_connection: LndConnectionInfo,
        event_sender: mpsc::Sender<Event>,
        token: CancellationToken,
    ) -> Self {
        Self {
            payment_hash,
            lnd_connection,
            event_sender,
            token,
        }
    }

    async fn run(self) {
        loop {
            select! {
                result = self.run_inner() => {
                    match result {
                        Ok(_) => {
                            break;
                        }
                        Err(err) => {
                            log::error!(
                                "Error tracking LND invoices, retry 15 seconds later: {:?}",
                                err
                            );
                            select! {
                                _ = sleep(Duration::from_secs(15)) => {
                                    // continue
                                }
                                _ = self.token.cancelled() => {
                                    log::debug!("Cancellation received, shutting down cch service");
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = self.token.cancelled() => {
                    log::debug!("Cancellation received, shutting down cch service");
                    return;
                }
            }
        }
    }

    async fn run_inner(&self) -> Result<()> {
        log::debug!(
            "[LndInvoiceTracker] will connect {}",
            self.lnd_connection.uri
        );
        let mut client = self.lnd_connection.create_invoices_client().await?;
        // TODO: clean up expired orders
        let mut stream = client
            .subscribe_single_invoice(invoicesrpc::SubscribeSingleInvoiceRequest {
                r_hash: hex::decode(&self.payment_hash)?,
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
                    log::debug!("Cancellation received, shutting down cch service");
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    // Return true to quit the tracker
    async fn on_invoice(&self, invoice: lnrpc::Invoice) -> Result<bool> {
        log::debug!("[LndPaymentsTracker] invoice: {:?}", invoice);
        let status = lnrpc::invoice::InvoiceState::try_from(invoice.state)
            .map(Into::into)
            .unwrap_or(CchOrderStatus::Pending);
        let event = Event::SettleReceiveBTCOrder(SettleReceiveBTCOrderEvent {
            payment_hash: hex::encode(invoice.r_hash),
            preimage: (!invoice.r_preimage.is_empty()).then_some(hex::encode(invoice.r_preimage)),
            status,
        });
        self.event_sender.send(event).await?;
        // Quit tracker when the status is final
        Ok(status == CchOrderStatus::Succeeded || status == CchOrderStatus::Failed)
    }
}

#[derive(Clone)]
struct LndConnectionInfo {
    uri: Uri,
    cert: Option<Vec<u8>>,
    macaroon: Vec<u8>,
}

impl LndConnectionInfo {
    async fn create_router_client(
        &self,
    ) -> Result<RouterClient, lnd_grpc_tonic_client::channel::Error> {
        create_router_client(
            self.uri.clone(),
            self.cert.as_deref(),
            self.macaroon.as_ref(),
        )
        .await
    }

    async fn create_invoices_client(
        &self,
    ) -> Result<InvoicesClient, lnd_grpc_tonic_client::channel::Error> {
        create_invoices_client(
            self.uri.clone(),
            self.cert.as_deref(),
            self.macaroon.as_ref(),
        )
        .await
    }
}
