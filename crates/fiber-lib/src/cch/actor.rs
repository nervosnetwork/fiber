use anyhow::{Context, Result};
use lightning_invoice::Bolt11Invoice;
use lnd_grpc_tonic_client::{invoicesrpc, Uri};
use ractor::{
    call, port::OutputPortSubscriberTrait as _, Actor, ActorProcessingErr, ActorRef, OutputPort,
    RpcReplyPort,
};
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use tentacle::secio::SecioKeyPair;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::cch::actions::{ActionDispatcher, CchOrderAction};
use crate::cch::order::CchOrderStateMachine;
use crate::cch::scheduler::{CchOrderSchedulerActor, SchedulerArgs, SchedulerMessage};
use crate::cch::trackers::{
    CchTrackingEvent, LndConnectionInfo, LndTrackerActor, LndTrackerArgs, LndTrackerMessage,
};
use crate::cch::{
    CchConfig, CchError, CchInvoice, CchOrder, CchOrderStatus, CchOrderStore, CchStoreError,
};
use crate::ckb::contracts::{get_script_by_contract, Contract};
use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::types::{Hash256, Privkey};
use crate::fiber::ASSUME_NETWORK_ACTOR_ALIVE;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::time::{Duration, SystemTime, UNIX_EPOCH};

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

    /// Schedule a retry for an action with backoff after a transient failure.
    ActionRetry {
        payment_hash: Hash256,
        action: CchOrderAction,
        retry_count: u32,
        reason: String,
    },

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

pub struct CchActor<S>(std::marker::PhantomData<S>);

impl<S> Default for CchActor<S> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

pub struct CchArgs<S> {
    pub config: CchConfig,
    pub tracker: TaskTracker,
    pub token: CancellationToken,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub node_keypair: crate::fiber::KeyPair,
    pub store: S,
}

pub struct CchState<S> {
    pub(super) config: CchConfig,
    pub(super) network_actor: ActorRef<NetworkActorMessage>,
    pub(super) node_keypair: (PublicKey, SecretKey),
    pub(super) lnd_connection: LndConnectionInfo,
    pub(super) lnd_tracker: ActorRef<LndTrackerMessage>,
    pub(super) scheduler: ActorRef<SchedulerMessage>,
    pub(super) store: S,
}

#[async_trait::async_trait]
impl<S: CchOrderStore + Send + Sync + Clone + 'static> Actor for CchActor<S> {
    type Msg = CchMessage;
    type State = CchState<S>;
    type Arguments = CchArgs<S>;

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

        // Start scheduler actor
        let scheduler = CchOrderSchedulerActor::start(
            SchedulerArgs {
                store: args.store.clone(),
                lnd_tracker: lnd_tracker.clone(),
            },
            myself.get_cell(),
        )
        .await?;

        let state = CchState {
            config: args.config,
            network_actor: args.network_actor,
            store: args.store,
            node_keypair,
            lnd_connection,
            lnd_tracker,
            scheduler,
        };

        Ok(state)
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time should always be after UNIX_EPOCH")
            .as_secs();

        // Load all orders from the database
        for mut order in state
            .store
            .get_cch_order_keys_iter()
            .into_iter()
            .filter_map(|payment_hash| state.store.get_cch_order(&payment_hash).ok())
        {
            // Only process active (non-final) orders
            if order.is_final() {
                state.schedule_job_for_final_order(&order);
                continue;
            }

            // Check if order is expired and mark as Failed if so
            if order.update_if_expired(current_time) {
                let payment_hash = order.payment_hash;
                state.store.update_cch_order(order.clone());
                state.schedule_job_for_final_order(&order);
                tracing::info!("Marked expired order {:x} as Failed", payment_hash);
                continue;
            }

            // Schedule expiry job for non-final orders
            state.schedule_job_for_non_final_order(&order);

            // Resume tracking for non-expired active orders
            let actions = ActionDispatcher::on_starting(&order);
            if let Err(err) = append_actions(myself.clone(), order.payment_hash, actions) {
                tracing::error!(
                    "Failed to schedule resume actions for order {:x}: {}",
                    order.payment_hash,
                    err
                );
            } else {
                tracing::debug!("Resumed tracking for active order {:x}", order.payment_hash);
            }
        }

        Ok(())
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
                    // Schedule jobs for new order
                    state.schedule_job_for_non_final_order(order);
                    let actions = ActionDispatcher::on_starting(order);
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
                    // Schedule jobs for new order
                    state.schedule_job_for_non_final_order(order);
                    let actions = ActionDispatcher::on_starting(order);
                    append_actions(myself, order.payment_hash, actions)?;
                }
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::GetCchOrder(payment_hash, port) => {
                let result = state.store.get_cch_order(&payment_hash).map_err(Into::into);
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
            CchMessage::ActionRetry {
                payment_hash,
                action,
                retry_count,
                reason,
            } => {
                if state.get_active_order_or_none(&payment_hash)?.is_none() {
                    return Ok(());
                }
                schedule_action_retry(&myself, payment_hash, action, retry_count, &reason);
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
                if let Err(err) =
                    ActionDispatcher::execute(state, &myself, &order, action, retry_count).await
                {
                    schedule_action_retry(
                        &myself,
                        payment_hash,
                        action,
                        retry_count,
                        &err.to_string(),
                    );
                }

                Ok(())
            }
            #[cfg(test)]
            CchMessage::InsertOrder(order, port) => {
                let result = state.store.insert_cch_order(order).map_err(Into::into);
                if !port.is_closed() {
                    let _ = port.send(result);
                }
                Ok(())
            }
        }
    }
}

impl<S: CchOrderStore> CchState<S> {
    /// Get a CCH order by payment hash, returning None if not found.
    /// This handles the common pattern of checking for NotFound vs other errors.
    fn get_order_or_none(&self, payment_hash: &Hash256) -> Result<Option<CchOrder>, CchError> {
        match self.store.get_cch_order(payment_hash) {
            Err(CchStoreError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
            Ok(order) => Ok(Some(order)),
        }
    }

    /// Get a CCH order by payment hash, returning None if not found or the order status is final.
    fn get_active_order_or_none(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<CchOrder>, CchError> {
        Ok(self
            .get_order_or_none(payment_hash)?
            .filter(|order| !order.is_final()))
    }

    fn schedule_job_for_non_final_order(&self, order: &CchOrder) {
        if let Err(err) = self
            .scheduler
            .send_message(SchedulerMessage::ScheduleExpiry {
                payment_hash: order.payment_hash,
                created_at: order.created_at,
                expiry_delta_seconds: order.expiry_delta_seconds,
            })
        {
            tracing::error!(
                "Failed to schedule expiry job for order {:x}: {}",
                order.payment_hash,
                err
            );
        }
    }

    fn schedule_job_for_final_order(&self, order: &CchOrder) {
        let payment_hash = order.payment_hash;
        if let Err(err) = self
            .scheduler
            .send_message(SchedulerMessage::SchedulePrune {
                payment_hash,
                created_at: order.created_at,
                expiry_delta_seconds: order.expiry_delta_seconds,
            })
        {
            tracing::error!(
                "Failed to schedule prune job for final order {:x}: {}",
                payment_hash,
                err
            );
        }
    }

    fn schedule_job_on_entering(&self, order: &CchOrder) {
        if order.is_final() {
            self.schedule_job_for_final_order(order);
        } else {
            self.schedule_job_for_non_final_order(order);
        }
    }

    async fn send_btc(&self, send_btc: SendBTC) -> Result<CchOrder, CchError> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        let invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        tracing::debug!("BTC invoice: {:?}", invoice);
        let payment_hash = Hash256::from(*invoice.payment_hash());

        // Validate that outgoing BTC invoice's final CLTV is less than half of incoming CKB invoice's final TLC expiry.
        // This ensures the CCH operator has sufficient time to settle the incoming side before the outgoing side expires.
        // BTC uses blocks (~10 min each), CKB uses seconds.
        let btc_final_cltv_seconds = invoice.min_final_cltv_expiry_delta() * 600;
        let ckb_final_tlc_seconds = self.config.ckb_final_tlc_expiry_delta_seconds;
        if btc_final_cltv_seconds >= ckb_final_tlc_seconds / 2 {
            return Err(CchError::BTCInvoiceFinalTlcExpiryDeltaTooLarge);
        }

        let outgoing_invoice_expiry_delta_seconds = invoice
            .expires_at()
            .and_then(|expired_at| expired_at.checked_sub(duration_since_epoch))
            .map(|duration| duration.as_secs())
            .ok_or(CchError::BTCInvoiceExpired)?;
        if outgoing_invoice_expiry_delta_seconds
            < self.config.min_outgoing_invoice_expiry_delta_seconds
        {
            return Err(CchError::OutgoingInvoiceExpiryTooShort);
        }

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
            .expiry_time(Duration::from_secs(outgoing_invoice_expiry_delta_seconds))
            .final_expiry_delta(self.config.ckb_final_tlc_expiry_delta_seconds * 1000)
            .udt_type_script(wrapped_btc_type_script.clone().into())
            .payee_pub_key(self.node_keypair.0)
            .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &self.node_keypair.1))?;

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
            created_at: duration_since_epoch.as_secs(),
            expiry_delta_seconds: self.config.order_expiry_delta_seconds,
            failure_reason: None,
            incoming_invoice: CchInvoice::Fiber(invoice),
            outgoing_pay_req: send_btc.btc_pay_req,
            payment_preimage: None,
            status: CchOrderStatus::Pending,
            fee_sats,
            payment_hash,
            wrapped_btc_type_script,
        };

        self.store.insert_cch_order(order.clone())?;
        Ok(order)
    }

    async fn receive_btc(&self, receive_btc: ReceiveBTC) -> Result<CchOrder, CchError> {
        let invoice = CkbInvoice::from_str(&receive_btc.fiber_pay_req)?;
        let payment_hash = *invoice.payment_hash();
        let amount_sats = invoice.amount().ok_or(CchError::CKBInvoiceMissingAmount)?;

        // Validate that outgoing CKB invoice's final TLC is less than half of incoming BTC invoice's final CLTV expiry.
        // This ensures the CCH operator has sufficient time to settle the incoming side before the outgoing side expires.
        // CKB uses milliseconds, BTC uses blocks (~10 min each).
        let ckb_final_tlc_millis = invoice
            .final_tlc_minimum_expiry_delta()
            .copied()
            .unwrap_or(0);
        let btc_final_cltv_millis = self.config.btc_final_tlc_expiry_delta_blocks * 600 * 1000;
        if ckb_final_tlc_millis >= btc_final_cltv_millis / 2 {
            return Err(CchError::CKBInvoiceFinalTlcExpiryDeltaTooLarge);
        }

        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;
        // Convert timestamp + expiry_time to the expiry time relative to `duration_since`.
        let outgoing_invoice_expiry_delta_seconds = match invoice.expiry_time() {
            Some(expiry) => invoice
                .data
                .timestamp
                .checked_add(expiry.as_millis())
                .and_then(|expiry_at| {
                    u64::try_from(expiry_at / 1000)
                        .unwrap_or(u64::MAX)
                        .checked_sub(duration_since_epoch.as_secs())
                })
                .ok_or(CchError::OutgoingInvoiceExpiryTooShort)?,
            // CKB invoice has no default expiry, use minimal * 2 to create the invoice
            None => self.config.min_outgoing_invoice_expiry_delta_seconds * 2,
        };
        if outgoing_invoice_expiry_delta_seconds
            < self.config.min_outgoing_invoice_expiry_delta_seconds
        {
            return Err(CchError::OutgoingInvoiceExpiryTooShort);
        }

        let fee_sats = amount_sats * (self.config.fee_rate_per_million_sats as u128)
            / 1_000_000u128
            + (self.config.base_fee_sats as u128);
        if amount_sats <= fee_sats {
            return Err(CchError::ReceiveBTCOrderAmountTooSmall);
        }
        if amount_sats > (i64::MAX / 1_000i64) as u128 {
            return Err(CchError::ReceiveBTCOrderAmountTooLarge);
        }

        // Verify wrapped_btc_type_script matches invoice UDT type script
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

        // Verify invoice UDT type script matches configured wrapped_btc_type_script
        if let Some(invoice_udt_script) = invoice.udt_type_script() {
            let invoice_script: ckb_jsonrpc_types::Script = invoice_udt_script.clone().into();
            if invoice_script.code_hash != wrapped_btc_type_script.code_hash
                || invoice_script.hash_type != wrapped_btc_type_script.hash_type
                || invoice_script.args != wrapped_btc_type_script.args
            {
                return Err(CchError::WrappedBTCTypescriptMismatch);
            }
        } else {
            return Err(CchError::WrappedBTCTypescriptMismatch);
        }

        // Validate hash algorithm - must be SHA256 for LND compatibility
        let hash_algorithm = invoice.hash_algorithm().copied().unwrap_or_default();
        if hash_algorithm != HashAlgorithm::Sha256 {
            return Err(CchError::CKBInvoiceIncompatibleHashAlgorithm);
        }

        let mut client = self.lnd_connection.create_invoices_client().await?;
        let req = invoicesrpc::AddHoldInvoiceRequest {
            hash: payment_hash.as_ref().to_vec(),
            value_msat: (amount_sats * 1_000u128) as i64,
            expiry: outgoing_invoice_expiry_delta_seconds as i64,
            cltv_expiry: self.config.btc_final_tlc_expiry_delta_blocks,
            ..Default::default()
        };
        let add_invoice_resp = client
            .add_hold_invoice(req)
            .await
            .map_err(|err| CchError::LndRpcError(err.to_string()))?
            .into_inner();
        let incoming_invoice = Bolt11Invoice::from_str(&add_invoice_resp.payment_request)?;

        let order = CchOrder {
            created_at: duration_since_epoch.as_secs(),
            expiry_delta_seconds: self.config.order_expiry_delta_seconds,
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

        self.store.insert_cch_order(order.clone())?;
        Ok(order)
    }

    async fn handle_tracking_event(&self, event: CchTrackingEvent) -> Result<Vec<CchOrderAction>> {
        let mut order = match self.get_active_order_or_none(event.payment_hash())? {
            None => return Ok(vec![]),
            Some(order) => order,
        };

        if CchOrderStateMachine::apply(&mut order, event.into())?.is_some() {
            self.store.update_cch_order(order.clone());
            self.schedule_job_on_entering(&order);
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

fn schedule_action_retry(
    myself: &ActorRef<CchMessage>,
    payment_hash: Hash256,
    action: CchOrderAction,
    retry_count: u32,
    reason: &str,
) {
    let delay = calculate_retry_delay(retry_count);
    tracing::error!(
        "action {:?} for payment hash {:x} failed (retry {}): {}. Retrying in {:?}",
        action,
        payment_hash,
        retry_count,
        reason,
        delay
    );
    // Retry the action later with exponential backoff. The action executor will
    // cease retrying only when it handles the error internally and returns OK.
    myself.send_after(delay, move || CchMessage::ExecuteAction {
        payment_hash,
        action,
        retry_count: retry_count.saturating_add(1),
    });
}
