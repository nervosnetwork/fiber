use anyhow::{anyhow, Context, Result};
use futures::StreamExt as _;
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{invoicesrpc, lnrpc, routerrpc, Uri};
use ractor::{
    call, port::OutputPortSubscriberTrait as _, Actor, ActorCell, ActorProcessingErr, ActorRef,
    OutputPort, RpcReplyPort,
};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use tentacle::secio::SecioKeyPair;
use tokio::select;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::ckb::contracts::{get_script_by_contract, Contract};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::payment::PaymentStatus;
use crate::fiber::payment::SendPaymentCommand;
use crate::fiber::types::{Hash256, Privkey};
use crate::fiber::ASSUME_NETWORK_ACTOR_ALIVE;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    error::CchDbError, CchConfig, CchError, CchIncomingEvent, CchIncomingPaymentStatus, CchInvoice,
    CchOrder, CchOrderStatus, CchOrdersDb, CchOutgoingPaymentStatus, LndConnectionInfo,
    LndTrackerActor, LndTrackerArgs, LndTrackerMessage,
};

pub const BTC_PAYMENT_TIMEOUT_SECONDS: i32 = 60;
pub const DEFAULT_ORDER_EXPIRY_SECONDS: u64 = 86400; // 24 hours

pub async fn start_cch(args: CchArgs, root_actor: ActorCell) -> Result<ActorRef<CchMessage>> {
    let (actor, _handle) =
        Actor::spawn_linked(Some("cch actor".to_string()), CchActor, args, root_actor).await?;
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

    Event(CchIncomingEvent),
}

impl From<CchIncomingEvent> for CchMessage {
    fn from(value: CchIncomingEvent) -> Self {
        CchMessage::Event(value)
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
    token: CancellationToken,
    network_actor: ActorRef<NetworkActorMessage>,
    node_keypair: (PublicKey, SecretKey),
    lnd_connection: LndConnectionInfo,
    lnd_tracker: ActorRef<LndTrackerMessage>,
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
        let lnd_connection = LndConnectionInfo::new(lnd_rpc_url, cert, macaroon);

        let private_key: Privkey = <[u8; 32]>::try_from(args.node_keypair.as_ref())
            .expect("valid length for key")
            .into();
        let secio_kp = SecioKeyPair::from(args.node_keypair);

        let node_keypair = (
            PublicKey::from_slice(secio_kp.public_key().inner_ref()).expect("valid public key"),
            private_key.into(),
        );

        // Create LND tracker port and subscribe
        let lnd_port = Arc::new(OutputPort::default());
        let lnd_tracker = LndTrackerActor::start(
            LndTrackerArgs {
                port: lnd_port.clone(),
                lnd_connection: lnd_connection.clone(),
                tracker: args.tracker,
                token: args.token.clone(),
            },
            myself.get_cell(),
        )
        .await?;
        myself.subscribe_to_port(&lnd_port);

        let state = CchState {
            config: args.config,
            token: args.token,
            network_actor: args.network_actor,
            orders_db: Default::default(),
            node_keypair,
            lnd_connection,
            lnd_tracker,
        };

        Ok(state)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
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
                let result = state.receive_btc(receive_btc).await;
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
            CchMessage::Event(event) => {
                tracing::debug!("event {:?}", event);
                if let Err(err) = state.handle_event(event).await {
                    tracing::error!("handle_event failed: {}", err);
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
        call!(self.network_actor, message).expect(ASSUME_NETWORK_ACTOR_ALIVE)?;

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

    async fn handle_fiber_invoice_changed_event(
        &mut self,
        mut order: CchOrder,
        status: CchIncomingPaymentStatus,
    ) -> Result<()> {
        if !(order.is_awaiting_fiber_invoice_event()) {
            return Ok(());
        }

        order.status = status.into();
        if status == CchIncomingPaymentStatus::Accepted {
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
                        let status: CchOutgoingPaymentStatus = lnrpc::payment::PaymentStatus::try_from(payment.status)?.into();
                        order.status = status.into();
                    }
                }
                _ = self.token.cancelled() => {
                    tracing::debug!("Cancellation received, shutting down cch service");
                }
            }
        }

        self.orders_db.update_cch_order(order).await?;
        Ok(())
    }

    async fn handle_fiber_payment_changed_event(
        &mut self,
        mut order: CchOrder,
        payment_preimage: Option<Hash256>,
        status: CchOutgoingPaymentStatus,
    ) -> Result<()> {
        if !(order.is_awaiting_fiber_payment_event()) {
            return Ok(());
        }

        order.status = status.into();
        if status == CchOutgoingPaymentStatus::Settled {
            let payment_preimage =
                payment_preimage.ok_or(CchError::SettledPaymentMissingPreimage)?;
            order.payment_preimage = Some(payment_preimage);
            self.orders_db.update_cch_order(order.clone()).await?;

            // settle the lnd invoice
            let req = invoicesrpc::SettleInvoiceMsg {
                preimage: payment_preimage.into(),
            };
            tracing::debug!("[settled tlc] SettleInvoiceMsg: {:?}", req);

            let mut client = self.lnd_connection.create_invoices_client().await?;
            // TODO: set a fee
            let resp = client.settle_invoice(req).await?.into_inner();
            tracing::debug!("[settled tlc] SettleInvoiceResp: {:?}", resp);

            order.status = CchOrderStatus::Succeeded;
        }

        self.orders_db.update_cch_order(order).await?;
        Ok(())
    }

    async fn handle_event(&mut self, event: CchIncomingEvent) -> Result<()> {
        let order = match self.orders_db.get_cch_order(event.payment_hash()).await {
            Err(CchDbError::NotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(order) => order,
        };

        match event {
            CchIncomingEvent::InvoiceChanged { status, .. } => {
                if !order.is_awaiting_invoice_event() {
                    tracing::info!(
                        "ignore invoice event {:?} while order status is {:?}",
                        status,
                        order.status,
                    );
                    return Ok(());
                }
                if order.is_incoming_invoice_fiber() {
                    self.handle_fiber_invoice_changed_event(order, status).await
                } else {
                    self.handle_lnd_invoice_changed_event(order, status).await
                }
            }
            CchIncomingEvent::PaymentChanged {
                payment_preimage,
                status,
                ..
            } => {
                if !order.is_awaiting_payment_event() {
                    tracing::info!(
                        "ignore payment event {:?} while order status is {:?}",
                        status,
                        order.status,
                    );
                    return Ok(());
                }
                if order.is_outgoing_payment_fiber() {
                    self.handle_fiber_payment_changed_event(order, payment_preimage, status)
                        .await
                } else {
                    self.handle_lnd_payment_changed_event(order, payment_preimage, status)
                        .await
                }
            }
        }
    }

    async fn handle_lnd_payment_changed_event(
        &mut self,
        mut order: CchOrder,
        payment_preimage: Option<Hash256>,
        status: CchOutgoingPaymentStatus,
    ) -> Result<()> {
        if !(order.is_awaiting_lnd_payment_event()) {
            return Ok(());
        }

        order.status = status.into();
        if status == CchOutgoingPaymentStatus::Settled {
            let payment_preimage =
                payment_preimage.ok_or(CchError::SettledPaymentMissingPreimage)?;
            order.payment_preimage = Some(payment_preimage);
            self.orders_db.update_cch_order(order.clone()).await?;

            let command = move |rpc_reply| -> NetworkActorMessage {
                NetworkActorMessage::Command(NetworkActorCommand::SettleInvoice(
                    order.payment_hash,
                    payment_preimage,
                    rpc_reply,
                ))
            };
            call!(self.network_actor, command).expect(ASSUME_NETWORK_ACTOR_ALIVE)?;
            order.status = CchOrderStatus::Succeeded;
        }

        self.orders_db.update_cch_order(order).await?;
        Ok(())
    }

    async fn handle_lnd_invoice_changed_event(
        &mut self,
        mut order: CchOrder,
        status: CchIncomingPaymentStatus,
    ) -> Result<()> {
        if !(order.is_awaiting_lnd_invoice_event()) {
            return Ok(());
        }

        order.status = status.into();
        if status == CchIncomingPaymentStatus::Accepted {
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
                .expect(ASSUME_NETWORK_ACTOR_ALIVE)
                .map_err(|err| anyhow!("{}", err))?
                .status;

            if payment_status == PaymentStatus::Failed {
                order.status = CchOrderStatus::Failed;
            } else {
                order.status = CchOrderStatus::OutgoingInFlight;
            }
        }

        self.orders_db.update_cch_order(order).await?;
        Ok(())
    }

    async fn receive_btc(&mut self, receive_btc: ReceiveBTC) -> Result<CchOrder, CchError> {
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

        self.lnd_tracker
            .cast(LndTrackerMessage::TrackInvoice(payment_hash))
            .expect("cast message to LndTrackerActor");

        Ok(order)
    }
}
