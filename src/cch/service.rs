#![allow(dead_code, unreachable_code)]

use anyhow::{anyhow, Result};
use futures::StreamExt as _;
use hex::ToHex;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{create_router_client, lnrpc, routerrpc, RouterClient, Uri};
use std::collections::HashSet;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{select, sync::mpsc, time::sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::command::TestPayBTC;
use super::error::CchDbError;
use super::{CchCommand, CchConfig, CchError, CchOrderStatus, CchOrdersDb, SendBTC, SendBTCOrder};

pub const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;

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
enum Event {
    SettleSendBTCOrder(SettleSendBTCOrderEvent),
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
            result = stream.next() => {
                log::debug!("[test_pay_btc] payment result: {:?}", result);
            }
            _ = self.token.cancelled() => {
                log::debug!("Cancellation received, shutting down cch service");
                return Ok(());
            }
        }

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
        // Reuse the stream or create a new subscription
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
}
