use anyhow::{Context, Result};
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{invoicesrpc, Uri};
use ractor::{
    call, port::OutputPortSubscriberTrait as _, Actor, ActorProcessingErr, ActorRef, OutputPort,
    RpcReplyPort,
};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use tentacle::secio::SecioKeyPair;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::cch::actions::{ActionDispatcher, CchOrderAction};
use crate::cch::order::CchOrderStateMachine;
use crate::cch::trackers::{
    CchTrackingEvent, LndConnectionInfo, LndTrackerActor, LndTrackerArgs, LndTrackerMessage,
};
use crate::cch::{
    CchConfig, CchDbError, CchError, CchInvoice, CchOrder, CchOrderStatus, CchOrdersDb,
};
use crate::ckb::contracts::{get_script_by_contract, Contract};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::types::{Hash256, Privkey};
use crate::fiber::ASSUME_NETWORK_ACTOR_ALIVE;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_ORDER_EXPIRY_SECONDS: u64 = 86400; // 24 hours
pub const ACTION_RETRY_BASE_MILLIS: u64 = 1000; // 1 second initial delay
pub const ACTION_RETRY_MAX_MILLIS: u64 = 600_000; // 10 minute max delay

fn calculate_retry_delay(retry_count: u32) -> Duration {
    // Exponential backoff starting from ACTION_RETRY_BASE_MILLIS, capped at ACTION_RETRY_MAX_MILLIS
    let max_shift = (ACTION_RETRY_MAX_MILLIS / ACTION_RETRY_BASE_MILLIS).ilog2();
    let delay = ACTION_RETRY_BASE_MILLIS.saturating_mul(1 << retry_count.min(max_shift));
    Duration::from_millis(delay.min(ACTION_RETRY_MAX_MILLIS))
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

    TrackingEvent(CchTrackingEvent),

    ExecuteAction {
        payment_hash: Hash256,
        action: CchOrderAction,
        retry_count: u32,
    },

    /// Test-only message to insert an order directly into the database
    #[cfg(test)]
    InsertOrder(CchOrder, RpcReplyPort<Result<(), CchError>>),
}

impl From<CchTrackingEvent> for CchMessage {
    fn from(value: CchTrackingEvent) -> Self {
        CchMessage::TrackingEvent(value)
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
    pub(super) config: CchConfig,
    pub(super) network_actor: ActorRef<NetworkActorMessage>,
    pub(super) node_keypair: (PublicKey, SecretKey),
    pub(super) lnd_connection: LndConnectionInfo,
    pub(super) lnd_tracker: ActorRef<LndTrackerMessage>,
    pub(super) orders_db: CchOrdersDb,
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
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CchMessage::SendBTC(send_btc, port) => {
                let result = state.send_btc(send_btc).await;
                if let Ok(order) = &result {
                    let actions = ActionDispatcher::on_entering(order);
                    append_actions(myself, order.payment_hash, actions)?;
                }
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::ReceiveBTC(receive_btc, port) => {
                let result = state.receive_btc(receive_btc).await;
                if let Ok(order) = &result {
                    let actions = ActionDispatcher::on_entering(order);
                    append_actions(myself, order.payment_hash, actions)?;
                }
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
                    .map_err(Into::into);
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::TrackingEvent(event) => {
                tracing::debug!("event {:?}", event);
                let payment_hash = *event.payment_hash();
                match state.handle_tracking_event(event).await {
                    Ok(actions) => {
                        append_actions(myself, payment_hash, actions)?;
                    }
                    Err(err) => {
                        // Ignore errors because events come from external systems
                        tracing::error!(
                            "handle_tracking_event for payment hash {:x} failed: {}",
                            payment_hash,
                            err
                        );
                    }
                }
                Ok(())
            }
            CchMessage::ExecuteAction {
                payment_hash,
                action,
                retry_count,
            } => {
                let order = match state.get_active_order_or_none(&payment_hash)? {
                    None => return Ok(()),
                    Some(order) => order,
                };
                if let Err(err) = ActionDispatcher::execute(state, &myself, &order, action).await {
                    let delay = calculate_retry_delay(retry_count);
                    tracing::error!(
                        "failed to execute action {:?} (retry {}): {}, retrying in {:?}",
                        action,
                        retry_count,
                        err,
                        delay
                    );
                    // Retry the action later with exponential backoff. The action
                    // executor will only cease retrying if it handles the error
                    // internally and returns OK.
                    myself.send_after(delay, move || CchMessage::ExecuteAction {
                        payment_hash,
                        action,
                        retry_count: retry_count.saturating_add(1),
                    });
                }

                Ok(())
            }
            #[cfg(test)]
            CchMessage::InsertOrder(order, port) => {
                let result = state.orders_db.insert_cch_order(order).map_err(Into::into);
                if !port.is_closed() {
                    let _ = port.send(result);
                }
                Ok(())
            }
        }
    }
}

impl CchState {
    /// Get a CCH order by payment hash, returning None if not found.
    /// This handles the common pattern of checking for NotFound vs other errors.
    fn get_order_or_none(&mut self, payment_hash: &Hash256) -> Result<Option<CchOrder>, CchError> {
        match self.orders_db.get_cch_order(payment_hash) {
            Err(CchDbError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
            Ok(order) => Ok(Some(order)),
        }
    }

    /// Get a CCH order by payment hash, returning None if not found or the order status is final.
    fn get_active_order_or_none(
        &mut self,
        payment_hash: &Hash256,
    ) -> Result<Option<CchOrder>, CchError> {
        Ok(self
            .get_order_or_none(payment_hash)?
            .filter(|order| !order.is_final()))
    }

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
            amount_sats: invoice_amount_sats,
            ckb_final_tlc_expiry_delta: self.config.ckb_final_tlc_expiry_delta,
            created_at: duration_since_epoch.as_secs(),
            expires_after: expiry,
            failure_reason: None,
            incoming_invoice: CchInvoice::Fiber(invoice),
            outgoing_pay_req: send_btc.btc_pay_req,
            payment_preimage: None,
            status: CchOrderStatus::Pending,
            fee_sats,
            payment_hash,
            wrapped_btc_type_script,
        };

        self.orders_db.insert_cch_order(order.clone())?;
        Ok(order)
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
            ckb_final_tlc_expiry_delta: final_tlc_minimum_expiry_delta,
            created_at: duration_since_epoch.as_secs(),
            expires_after: DEFAULT_ORDER_EXPIRY_SECONDS,
            failure_reason: None,
            incoming_invoice: CchInvoice::Lightning(incoming_invoice),
            outgoing_pay_req: receive_btc.fiber_pay_req,
            payment_preimage: None,
            status: CchOrderStatus::Pending,
            amount_sats,
            fee_sats,
            payment_hash,
            wrapped_btc_type_script,
        };

        self.orders_db.insert_cch_order(order.clone())?;
        Ok(order)
    }

    async fn handle_tracking_event(
        &mut self,
        event: CchTrackingEvent,
    ) -> Result<Vec<CchOrderAction>> {
        let mut order = match self.get_active_order_or_none(event.payment_hash())? {
            None => return Ok(vec![]),
            Some(order) => order,
        };

        if CchOrderStateMachine::apply(&mut order, event.into())?.is_some() {
            self.orders_db.update_cch_order(order.clone())?;
            Ok(ActionDispatcher::on_entering(&order))
        } else {
            Ok(vec![])
        }
    }
}

fn append_actions(
    myself: ActorRef<CchMessage>,
    payment_hash: Hash256,
    actions: Vec<CchOrderAction>,
) -> Result<(), ActorProcessingErr> {
    for action in actions {
        myself.send_message(CchMessage::ExecuteAction {
            payment_hash,
            action,
            retry_count: 0,
        })?;
    }
    Ok(())
}
