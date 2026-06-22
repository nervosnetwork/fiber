use anyhow::{anyhow, Context, Result};
use lightning_invoice::Bolt11Invoice;
use lightning_invoice::Currency as LnCurrency;
use lnd_grpc_tonic_client::{invoicesrpc, Uri};
use ractor::{
    port::OutputPortSubscriberTrait as _, Actor, ActorProcessingErr, ActorRef, OutputPort,
    RpcReplyPort,
};
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tentacle::secio::SecioKeyPair;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::cch::actions::{ActionDispatcher, CchOrderAction};
use crate::cch::cch_fiber_agent::{CchFiberAgentActor, CchFiberAgentHttpBackend, CchFiberAgentRef};
use crate::cch::order::CchOrderStateMachine;
use crate::cch::scheduler::{CchOrderSchedulerActor, SchedulerArgs, SchedulerMessage};
use crate::cch::trackers::{
    CchTrackingEvent, LndConnectionInfo, LndTrackerActor, LndTrackerArgs, LndTrackerMessage,
    RedactedCchTrackingEvent,
};
use crate::cch::{
    CchConfig, CchError, CchOrderStore, CchStoreError, SwapAcceptorActor, SwapAcceptorMessage,
};
use crate::fiber::NetworkActorMessage;
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::store::store_impl::StoreChange;
use crate::time::{Duration, SystemTime, UNIX_EPOCH};
use fiber_types::{
    CchInvoice, CchOrder, CchOrderStatus, HashAlgorithm, NewOrderResult, SwapDirection,
    SwapProposal, SwapProposalResponse,
};
use fiber_types::{Hash256, Privkey};
use jsonrpsee::SubscriptionSink;

pub const ACTION_RETRY_BASE_MILLIS: u64 = 1000; // 1 second initial delay
pub const ACTION_RETRY_MAX_MILLIS: u64 = 600_000; // 10 minute max delay

/// Average time per Bitcoin block in milliseconds (10 minutes = 600 seconds = 600,000 ms).
pub const BTC_BLOCK_TIME_MILLIS: u64 = 600_000;

/// Maximum number of startup resume requests kept in flight at once. The
/// `post_start` task streams resumption in batches of this size, awaiting each
/// batch before sending the next so it never floods the mailbox.
const RESUME_BATCH_SIZE: usize = 10;

fn calculate_retry_delay(retry_count: u32) -> Duration {
    // Exponential backoff starting from ACTION_RETRY_BASE_MILLIS, capped at ACTION_RETRY_MAX_MILLIS
    let max_shift = (ACTION_RETRY_MAX_MILLIS / ACTION_RETRY_BASE_MILLIS).ilog2();
    let delay = ACTION_RETRY_BASE_MILLIS.saturating_mul(1 << retry_count.min(max_shift));
    Duration::from_millis(delay.min(ACTION_RETRY_MAX_MILLIS))
}

/// Compute the remaining lifetime of a submitted Fiber invoice as seconds from
/// `now`, returning `Ok(None)` when the invoice carries no explicit expiry
/// (CKB invoices may omit it). Returns `Err(OutgoingInvoiceExpiryTooShort)`
/// when the invoice has already expired.
fn remaining_invoice_expiry_seconds(
    invoice: &CkbInvoice,
    now: Duration,
) -> Result<Option<u64>, CchError> {
    let Some(expiry) = invoice.expiry_time() else {
        return Ok(None);
    };
    invoice
        .data
        .timestamp
        .checked_add(expiry.as_millis())
        .and_then(|expiry_at| {
            u64::try_from(expiry_at / 1000)
                .unwrap_or(u64::MAX)
                .checked_sub(now.as_secs())
        })
        .map(Some)
        .ok_or(CchError::OutgoingInvoiceExpiryTooShort)
}

#[derive(Clone, Debug, Deserialize)]
pub struct SendBTC {
    pub btc_pay_req: String,
    pub currency: Currency,
    /// Identity of the Fiber-side asset to use for this swap. `None` denotes
    /// native CKB; otherwise the full UDT type script identifies the asset.
    /// Must appear in `CchConfig::fiber_asset_allowlist`.
    pub fiber_type_script: Option<ckb_jsonrpc_types::Script>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReceiveBTC {
    pub fiber_pay_req: String,
}

pub enum CchMessage {
    SendBTC(SendBTC, RpcReplyPort<Result<NewOrderResult, CchError>>),
    ReceiveBTC(ReceiveBTC, RpcReplyPort<Result<NewOrderResult, CchError>>),

    GetCchOrder(Hash256, RpcReplyPort<Result<CchOrder, CchError>>),

    TrackingEvent(CchTrackingEvent),

    /// Store change event from the Fiber node (either in-process or via WebSocket).
    StoreChangeEvent(StoreChange),

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

    /// Register a new operator-side subscription sink for `subscribe_swap_proposals`.
    /// The CCH actor forwards this to the [`SwapAcceptorActor`].
    SubscribeSwapProposals(SubscriptionSink),

    /// Operator's response to a previously emitted swap proposal, delivered
    /// via the `submit_swap_proposal_response` RPC method. The CCH actor
    /// resolves the proposal: on accept it mints the counterparty invoice and
    /// creates the order as `Pending`; on reject it drops the pending proposal.
    SubmitSwapProposalResponse(SwapProposalResponse, RpcReplyPort<Result<(), CchError>>),

    /// Fired by a spawned timer when a proposal's deadline elapses. If the
    /// referenced proposal is still pending, it is dropped (no order is
    /// created).
    ProposalTimeout {
        proposal_id: Hash256,
    },

    /// Resume a single persisted order on startup, identified by its
    /// `payment_hash`. The startup task spawned in `post_start` issues these in
    /// bounded batches (awaiting each batch's replies before sending the next)
    /// so records are streamed and resumed without flooding the mailbox or
    /// buffering the whole table in memory.
    ResumeOrder(Hash256, RpcReplyPort<()>),

    /// Resume a single persisted pending proposal on startup, identified by its
    /// `payment_hash`. Throttled the same way as [`CchMessage::ResumeOrder`].
    ResumePendingProposal(Hash256, RpcReplyPort<()>),

    /// Test-only: hand back the actor ref of the embedded
    /// [`SwapAcceptorActor`] so unit tests can inspect the broadcaster.
    #[cfg(test)]
    TestGetAcceptor(RpcReplyPort<ActorRef<SwapAcceptorMessage>>),

    /// Test-only: snapshot the currently-pending proposal ids so tests can
    /// learn the actor-generated `proposal_id` without brute-forcing the
    /// internal id-derivation counter.
    #[cfg(test)]
    TestPendingProposalIds(RpcReplyPort<Vec<Hash256>>),

    /// Test-only message to insert an order directly into the database
    #[cfg(test)]
    InsertOrder(CchOrder, RpcReplyPort<Result<(), CchError>>),
}

impl From<CchTrackingEvent> for CchMessage {
    fn from(value: CchTrackingEvent) -> Self {
        CchMessage::TrackingEvent(value)
    }
}

impl From<StoreChange> for CchMessage {
    fn from(value: StoreChange) -> Self {
        CchMessage::StoreChangeEvent(value)
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
    pub network_actor: Option<ActorRef<NetworkActorMessage>>,
    pub node_keypair: Option<crate::fiber::KeyPair>,
    pub store: S,
    /// The CKB network currency this node is configured for.
    /// Used to validate that incoming invoices match the expected network.
    pub currency: Currency,
}

#[derive(Clone)]
pub struct CchState<S> {
    pub(super) config: CchConfig,
    pub(super) fiber_agent_ref: CchFiberAgentRef,
    pub(super) node_keypair: Option<(PublicKey, SecretKey)>,
    pub(super) lnd_connection: LndConnectionInfo,
    pub(super) lnd_tracker: ActorRef<LndTrackerMessage>,
    pub(super) scheduler: ActorRef<SchedulerMessage>,
    pub(super) store: S,
    /// The CKB network currency this node is configured for.
    pub(super) currency: Currency,
    pub(super) acceptor: ActorRef<SwapAcceptorMessage>,
    /// Proposals awaiting an operator decision, mapping `proposal_id` to the
    /// underlying `payment_hash`. Owned by the actor and mutated only inside
    /// `handle` / `post_start`, so no synchronisation is needed — the
    /// single-threaded mailbox serialises all access. Rebuilt from persisted
    /// [`SwapProposal`] records on startup.
    pub(super) pending_proposals: HashMap<Hash256, Hash256>,
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
        // Validate generic config invariants (e.g. the outgoing fee budget percentage).
        args.config.validate().map_err(|e| anyhow!(e))?;

        // Validate that we have either an in-process network actor or a fiber RPC URL
        if args.network_actor.is_none() {
            if args.config.fiber_rpc_url.is_none() {
                return Err(anyhow!(
                    "Cch requires either in-process network actor or configured fiber RPC URL"
                )
                .into());
            }
            ensure_fiber_http_url(args.config.fiber_rpc_url.clone())?;
        }

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

        let node_keypair = args.node_keypair.map(|kp| {
            let private_key: Privkey = <[u8; 32]>::try_from(kp.as_ref())
                .expect("valid length for key")
                .into();
            let secio_kp = SecioKeyPair::from(kp);
            (
                PublicKey::from_slice(secio_kp.public_key().inner_ref()).expect("valid public key"),
                private_key.into(),
            )
        });

        // Create LND tracker port and subscribe
        let lnd_port = Arc::new(OutputPort::default());
        let lnd_tracker = LndTrackerActor::start(
            LndTrackerArgs {
                port: lnd_port.clone(),
                lnd_connection: lnd_connection.clone(),
                tracker: args.tracker.clone(),
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

        // Start the swap acceptor (operator-side proposal channel).
        let (acceptor, _) =
            ractor::Actor::spawn_linked(None, SwapAcceptorActor, (), myself.get_cell()).await?;

        // Set up store change subscription
        if args.network_actor.is_none() {
            // Separate service mode: subscribe to Fiber node's store changes via WebSocket
            let ws_url = ensure_fiber_ws_url(args.config.fiber_rpc_url.clone())?;
            let myself_clone = myself.clone();
            let token = args.token.clone();
            args.tracker.spawn(async move {
                subscribe_store_changes_ws(ws_url, myself_clone, token).await;
            });
        }

        let fiber_agent_ref = if let Some(network_actor) = args.network_actor {
            CchFiberAgentRef::InProcess(network_actor)
        } else {
            let url = args
                .config
                .fiber_rpc_url
                .as_deref()
                .expect("validated in pre_start");
            let backend = CchFiberAgentHttpBackend::try_new(url)?;
            let (rpc_actor, _handle) =
                ractor::Actor::spawn_linked(None, CchFiberAgentActor, backend, myself.get_cell())
                    .await?;
            CchFiberAgentRef::Rpc(rpc_actor)
        };

        let state = CchState {
            config: args.config,
            fiber_agent_ref,
            store: args.store,
            node_keypair,
            lnd_connection,
            lnd_tracker,
            scheduler,
            currency: args.currency,
            acceptor,
            pending_proposals: HashMap::new(),
        };

        Ok(state)
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // Snapshot just the keys (small) for both tables, then resume each
        // record. The spawned task drives resumption by issuing `ResumeOrder` /
        // `ResumePendingProposal` requests in batches of `RESUME_BATCH_SIZE`,
        // awaiting each batch's replies before sending the next, so we never
        // buffer the whole database in memory nor flood the mailbox; each record
        // is loaded and processed with `&mut state` back on the actor.
        let order_keys: Vec<Hash256> = state.store.get_cch_order_keys_iter().into_iter().collect();
        let proposal_keys: Vec<Hash256> = state
            .store
            .get_cch_pending_proposal_keys_iter()
            .into_iter()
            .collect();

        tokio::spawn(async move {
            for batch in order_keys.chunks(RESUME_BATCH_SIZE) {
                let calls = batch.iter().map(|payment_hash| {
                    let payment_hash = *payment_hash;
                    myself.call(
                        move |reply| CchMessage::ResumeOrder(payment_hash, reply),
                        None,
                    )
                });
                // Bail out if the actor stopped (any call errors).
                if futures::future::join_all(calls)
                    .await
                    .into_iter()
                    .any(|r| r.is_err())
                {
                    return;
                }
            }
            for batch in proposal_keys.chunks(RESUME_BATCH_SIZE) {
                let calls = batch.iter().map(|payment_hash| {
                    let payment_hash = *payment_hash;
                    myself.call(
                        move |reply| CchMessage::ResumePendingProposal(payment_hash, reply),
                        None,
                    )
                });
                if futures::future::join_all(calls)
                    .await
                    .into_iter()
                    .any(|r| r.is_err())
                {
                    return;
                }
            }
        });

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
                let outcome = state.send_btc(send_btc).await;
                let reply = outcome.map(|outcome| state.complete_new_order(&myself, outcome));
                if !port.is_closed() {
                    let _ = port.send(reply);
                }
                Ok(())
            }
            CchMessage::ReceiveBTC(receive_btc, port) => {
                let outcome = state.receive_btc(receive_btc).await;
                let reply = outcome.map(|outcome| state.complete_new_order(&myself, outcome));
                if !port.is_closed() {
                    let _ = port.send(reply);
                }
                Ok(())
            }
            CchMessage::GetCchOrder(payment_hash, port) => {
                // No order exists during the proposal phase, so this naturally
                // returns `NotFound` until the proposal is accepted.
                let result = state.store.get_cch_order(&payment_hash).map_err(Into::into);
                if !port.is_closed() {
                    // ignore error
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::TrackingEvent(event) => {
                tracing::debug!("tracking event {:?}", RedactedCchTrackingEvent(&event));
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
            CchMessage::StoreChangeEvent(change) => {
                let summary = redacted_store_change_summary(&change);
                tracing::debug!(
                    "store change event kind={} payment_hash={:x} has_payment_preimage={}",
                    summary.kind,
                    summary.payment_hash,
                    summary.has_payment_preimage
                );
                let events = state.map_store_change_to_events(&change);
                for event in events {
                    let payment_hash = *event.payment_hash();
                    match state.handle_tracking_event(event).await {
                        Ok(actions) => {
                            append_actions(myself.clone(), payment_hash, actions)?;
                        }
                        Err(err) => {
                            tracing::error!(
                                "handle_tracking_event for payment hash {:x} failed: {}",
                                payment_hash,
                                err
                            );
                        }
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
            CchMessage::SubscribeSwapProposals(sink) => {
                // Replay the currently-pending proposals to the new subscriber
                // off the mailbox. The sink is `Clone` (shares one channel), so
                // we register the original for live broadcasts and stream the
                // history to a clone, loading proposals from the store one at a
                // time rather than buffering them all in memory.
                let store = state.store.clone();
                let replay_sink = sink.clone();
                tokio::spawn(crate::cch::acceptor::replay_pending_proposals(
                    store,
                    replay_sink,
                ));
                state
                    .acceptor
                    .send_message(SwapAcceptorMessage::AddSink(sink))?;
                Ok(())
            }
            CchMessage::SubmitSwapProposalResponse(response, port) => {
                let result = state.resolve_proposal(&myself, response).await;
                if !port.is_closed() {
                    let _ = port.send(result);
                }
                Ok(())
            }
            CchMessage::ProposalTimeout { proposal_id } => {
                state.handle_proposal_timeout(proposal_id);
                Ok(())
            }
            CchMessage::ResumeOrder(payment_hash, reply) => {
                state.resume_order_on_startup(&myself, payment_hash);
                let _ = reply.send(());
                Ok(())
            }
            CchMessage::ResumePendingProposal(payment_hash, reply) => {
                state.resume_pending_proposal_on_startup(&myself, payment_hash);
                let _ = reply.send(());
                Ok(())
            }
            #[cfg(test)]
            CchMessage::TestGetAcceptor(port) => {
                if !port.is_closed() {
                    let _ = port.send(state.acceptor.clone());
                }
                Ok(())
            }
            #[cfg(test)]
            CchMessage::TestPendingProposalIds(port) => {
                if !port.is_closed() {
                    let _ = port.send(state.pending_proposals.keys().copied().collect());
                }
                Ok(())
            }
        }
    }
}

/// Maps a CKB network currency to the expected Lightning Network invoice currency.
///
/// - Fibb (CKB mainnet) → Bitcoin mainnet
/// - Fibt (CKB testnet) → Bitcoin testnet
/// - Fibd (CKB devnet) → Bitcoin regtest
fn expected_ln_currency(currency: Currency) -> LnCurrency {
    match currency {
        Currency::Fibb => LnCurrency::Bitcoin,
        Currency::Fibt => LnCurrency::BitcoinTestnet,
        Currency::Fibd => LnCurrency::Regtest,
    }
}

/// Outcome of building a new CCH order from a `send_btc` / `receive_btc`
/// request. On the fast path the order is already persisted (`Ready`); on the
/// proposal path only a [`SwapProposal`] is persisted (no order exists yet) and
/// the handler broadcasts the proposal and arms its timeout (`AwaitingProposal`).
enum NewOrderOutcome {
    /// Fast path: the counterparty invoice was minted and the order is
    /// `Pending`, ready to start tracking.
    Ready(CchOrder),
    /// Proposal path: a [`SwapProposal`] was persisted (no order yet) and
    /// awaits an operator decision.
    AwaitingProposal(SwapProposal),
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

    /// Get a pending swap proposal by payment hash, returning None if not
    /// found. Mirrors [`Self::get_order_or_none`] for the proposals table.
    fn get_pending_proposal_or_none(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<SwapProposal>, CchError> {
        match self.store.get_cch_pending_proposal(payment_hash) {
            Err(CchStoreError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
            Ok(pending) => Ok(Some(pending)),
        }
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

    /// Verify that `script` (None = native CKB) is present in the configured
    /// `fiber_asset_allowlist`. Two scripts are considered equal iff their
    /// code_hash, hash_type, and args all match.
    fn ensure_fiber_asset_allowlisted(
        &self,
        script: &Option<ckb_jsonrpc_types::Script>,
    ) -> Result<(), CchError> {
        let matches = |a: &Option<ckb_jsonrpc_types::Script>,
                       b: &Option<ckb_jsonrpc_types::Script>| match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => {
                x.code_hash == y.code_hash && x.hash_type == y.hash_type && x.args == y.args
            }
            _ => false,
        };
        if self
            .config
            .fiber_asset_allowlist
            .iter()
            .any(|allowed| matches(allowed, script))
        {
            Ok(())
        } else {
            Err(CchError::FiberAssetNotAllowlisted)
        }
    }

    /// Look up the fixed exchange rate for `script` (None = native CKB).
    ///
    /// Returns `Some(rate)` (the configured `smallest_units_per_sat`) when an
    /// entry is present and non-zero. Returns `None` otherwise; callers MUST
    /// then route the swap through the operator [`SwapAcceptorActor`] (the
    /// "proposal path" in the CCH multi-asset spec) instead of computing the
    /// counterparty amount directly.
    fn lookup_fixed_rate(&self, script: &Option<ckb_jsonrpc_types::Script>) -> Option<u128> {
        let matches = |a: &Option<ckb_jsonrpc_types::Script>,
                       b: &Option<ckb_jsonrpc_types::Script>| match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => {
                x.code_hash == y.code_hash && x.hash_type == y.hash_type && x.args == y.args
            }
            _ => false,
        };
        self.config
            .fixed_rate_assets
            .iter()
            .find(|entry| matches(&entry.fiber_asset, script))
            .map(|entry| entry.smallest_units_per_sat)
            .filter(|rate| *rate > 0)
    }

    /// Process the outcome of a `send_btc` / `receive_btc` computation:
    /// schedule jobs, broadcast a proposal when one is pending, and kick off
    /// the order's action flow. Returns the order to reply to the RPC client.
    ///
    /// Runs in the actor mailbox with `&mut self`, so all state mutation (the
    /// `pending_proposals` map) is naturally serialised — no locking needed.
    fn complete_new_order(
        &mut self,
        myself: &ActorRef<CchMessage>,
        outcome: NewOrderOutcome,
    ) -> NewOrderResult {
        match outcome {
            NewOrderOutcome::Ready(order) => {
                self.schedule_job_for_non_final_order(&order);
                let actions = ActionDispatcher::on_starting(&order);
                if let Err(err) = append_actions(myself.clone(), order.payment_hash, actions) {
                    tracing::error!(
                        "Failed to append actions for new order {:x}: {}",
                        order.payment_hash,
                        err
                    );
                }
                NewOrderResult::Order(order)
            }
            NewOrderOutcome::AwaitingProposal(proposal) => {
                self.pending_proposals
                    .insert(proposal.proposal_id, proposal.payment_hash);
                // No order-expiry job here: no order exists yet, and the
                // proposal's own timeout governs the pending phase. The order
                // expiry is scheduled when the order is created on accept.
                self.broadcast_proposal(myself, proposal.clone());
                NewOrderResult::PendingProposal(proposal)
            }
        }
    }

    /// Broadcast a proposal to operator subscribers and arm its timeout.
    fn broadcast_proposal(&self, myself: &ActorRef<CchMessage>, proposal: SwapProposal) {
        let proposal_id = proposal.proposal_id;
        let expires_at = proposal.expires_at;
        if let Err(err) = self
            .acceptor
            .send_message(SwapAcceptorMessage::Broadcast(Box::new(proposal)))
        {
            tracing::error!(
                "Failed to broadcast swap proposal {:x}: {}",
                proposal_id,
                err
            );
        }
        self.spawn_proposal_timeout(myself, proposal_id, expires_at);
    }

    /// Spawn a fire-and-forget timer that sends [`CchMessage::ProposalTimeout`]
    /// once the proposal's `expires_at` deadline is reached.
    fn spawn_proposal_timeout(
        &self,
        myself: &ActorRef<CchMessage>,
        proposal_id: Hash256,
        expires_at: u64,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let remaining = expires_at.saturating_sub(now);
        let myself = myself.clone();
        tokio::spawn(async move {
            if remaining > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
            }
            let _ = myself.send_message(CchMessage::ProposalTimeout { proposal_id });
        });
    }

    /// Resume a persisted order on startup, identified by `payment_hash`:
    /// final orders are scheduled for pruning; expired ones are failed; active
    /// ones get their expiry job re-armed and their action flow restarted.
    /// Missing records (raced with a prune) are ignored. Called once per key by
    /// the startup task so records are streamed rather than buffered.
    fn resume_order_on_startup(&mut self, myself: &ActorRef<CchMessage>, payment_hash: Hash256) {
        let mut order = match self.get_order_or_none(&payment_hash) {
            Ok(Some(order)) => order,
            Ok(None) => return,
            Err(err) => {
                tracing::error!(
                    "Failed to load order {:x} on startup: {}",
                    payment_hash,
                    err
                );
                return;
            }
        };

        // Only process active (non-final) orders.
        if order.is_final() {
            self.schedule_job_for_final_order(&order);
            return;
        }

        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Mark expired orders as Failed.
        if order.update_if_expired(current_time) {
            self.store.update_cch_order(order.clone());
            self.schedule_job_for_final_order(&order);
            tracing::info!("Marked expired order {:x} as Failed", payment_hash);
            return;
        }

        // Re-arm the expiry job and resume tracking.
        self.schedule_job_for_non_final_order(&order);
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

    /// Re-broadcast a persisted pending proposal on startup, identified by
    /// `payment_hash`, re-arming its timeout — or drop it immediately when its
    /// deadline has already elapsed. Missing records (raced with resolution)
    /// are ignored. Called once per key by the startup task.
    fn resume_pending_proposal_on_startup(
        &mut self,
        myself: &ActorRef<CchMessage>,
        payment_hash: Hash256,
    ) {
        let proposal = match self.get_pending_proposal_or_none(&payment_hash) {
            Ok(Some(proposal)) => proposal,
            Ok(None) => return,
            Err(err) => {
                tracing::error!(
                    "Failed to load pending proposal {:x} on startup: {}",
                    payment_hash,
                    err
                );
                return;
            }
        };

        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if proposal.expires_at <= current_time {
            tracing::info!(
                "Proposal for payment hash {:x} expired before restart; dropping it",
                payment_hash
            );
            self.store.delete_cch_pending_proposal(&payment_hash);
            return;
        }
        self.pending_proposals
            .insert(proposal.proposal_id, payment_hash);
        self.broadcast_proposal(myself, proposal);
    }

    /// Split an operator-supplied gross BTC total into the configured hub-fee
    /// component and the body, using the inverse of the fast-path formula:
    ///
    /// ```text
    /// gross = body + body * fee_rate / 1_000_000 + base_fee_sats * 1000
    /// body  = (gross - base_fee_msat) * 1_000_000 / (1_000_000 + fee_rate)
    /// fee   = gross - body
    /// ```
    ///
    /// Returns the fee component in millisatoshi. If the configured base fee
    /// already exceeds the operator total (or any intermediate computation
    /// would saturate), reports the entire gross as fee — the caller will
    /// surface this through normal accounting rather than panic.
    fn derive_btc_fee_msat_from_total(&self, gross_msat: u128) -> u128 {
        let base_fee_msat = (self.config.base_fee_sats as u128).saturating_mul(1_000);
        let denominator =
            1_000_000u128.saturating_add(self.config.fee_rate_per_million_sats as u128);
        let after_base = match gross_msat.checked_sub(base_fee_msat) {
            Some(v) => v,
            None => return gross_msat,
        };
        let body = after_base
            .checked_mul(1_000_000u128)
            .map(|v| v / denominator)
            .unwrap_or(after_base);
        gross_msat.saturating_sub(body)
    }

    /// Resolve a pending proposal once the operator answers. Looks up the
    /// pending proposal via the `proposal_id -> payment_hash` map.
    ///
    /// * Reject: delete the pending proposal — no order is ever created.
    /// * Accept: mint the counterparty invoice, build the `Pending` order,
    ///   persist it (and delete the pending proposal), then start tracking.
    ///
    /// A malformed accept (missing/zero amount) leaves the pending proposal in
    /// place so the operator can retry.
    async fn resolve_proposal(
        &mut self,
        myself: &ActorRef<CchMessage>,
        response: SwapProposalResponse,
    ) -> Result<(), CchError> {
        let proposal_id = response.proposal_id;
        let payment_hash = self
            .pending_proposals
            .get(&proposal_id)
            .copied()
            .ok_or(CchError::SwapProposalUnknown)?;

        let proposal = match self.get_pending_proposal_or_none(&payment_hash)? {
            Some(proposal) => proposal,
            None => {
                // The pending proposal disappeared; drop the stale mapping.
                self.pending_proposals.remove(&proposal_id);
                return Err(CchError::SwapProposalUnknown);
            }
        };

        if !response.accept {
            let reason = response
                .reject_reason
                .unwrap_or_else(|| "rejected by operator".to_string());
            tracing::info!(
                "Proposal {:x} rejected by operator: {}",
                proposal_id,
                reason
            );
            self.store.delete_cch_pending_proposal(&payment_hash);
            self.pending_proposals.remove(&proposal_id);
            return Ok(());
        }

        // Accept path: validate the operator-supplied counterparty amount
        // BEFORE minting anything, so a malformed accept leaves the pending
        // proposal in place and the operator can retry.
        let amount = response
            .counterparty_leg_amount
            .ok_or(CchError::SwapProposalResponseMissingAmount)?;
        if amount == 0 {
            return Err(CchError::SwapProposalResponseInvalidAmount);
        }

        // Build the order: mint the counterparty invoice and assemble the
        // `Pending` order from the proposal.
        let order = match proposal.direction {
            SwapDirection::SendBTC => self.build_send_btc_order(&proposal, amount).await?,
            SwapDirection::ReceiveBTC => self.build_receive_btc_order(&proposal, amount).await?,
        };

        // Persist the freshly created order and retire the pending proposal.
        self.store.insert_cch_order(order.clone())?;
        self.store.delete_cch_pending_proposal(&payment_hash);
        self.pending_proposals.remove(&proposal_id);
        self.schedule_job_for_non_final_order(&order);
        let actions = ActionDispatcher::on_starting(&order);
        if let Err(err) = append_actions(myself.clone(), order.payment_hash, actions) {
            tracing::error!(
                "Failed to append actions for resumed order {:x}: {}",
                order.payment_hash,
                err
            );
        }
        Ok(())
    }

    /// Mint the Fiber-leg counterparty invoice for an accepted `SendBTC`
    /// proposal and assemble the resulting `Pending` order.
    async fn build_send_btc_order(
        &self,
        proposal: &SwapProposal,
        fiber_invoice_amount: u128,
    ) -> Result<CchOrder, CchError> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let invoice = Bolt11Invoice::from_str(&proposal.submitted_invoice)?;
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

        let minted = self
            .mint_fiber_invoice(
                self.currency,
                proposal.payment_hash,
                &proposal.fiber_asset,
                fiber_invoice_amount,
                outgoing_invoice_expiry_delta_seconds,
            )
            .await?;

        Ok(CchOrder {
            created_at: proposal.created_at,
            expiry_delta_seconds: self.config.order_expiry_delta_seconds,
            fiber_type_script: proposal.fiber_asset.clone(),
            outgoing_pay_req: proposal.submitted_invoice.clone(),
            incoming_invoice: CchInvoice::Fiber(minted),
            payment_hash: proposal.payment_hash,
            payment_preimage: None,
            // Both are known up-front on `SendBTC`, so the proposal carries them.
            lightning_invoice_amount: proposal.lightning_invoice_amount.unwrap_or(0),
            btc_fee_msat: proposal.fee_on_btc_side_msat.unwrap_or(0),
            fiber_invoice_amount,
            status: CchOrderStatus::Pending,
            failure_reason: None,
        })
    }

    /// Mint the BTC-leg hold invoice for an accepted `ReceiveBTC` proposal and
    /// assemble the resulting `Pending` order. `btc_total_msat` is the gross
    /// operator-supplied BTC amount (body + hub fee).
    async fn build_receive_btc_order(
        &self,
        proposal: &SwapProposal,
        btc_total_msat: u128,
    ) -> Result<CchOrder, CchError> {
        let invoice = CkbInvoice::from_str(&proposal.submitted_invoice)?;
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let outgoing_invoice_expiry_delta_seconds =
            self.receive_btc_outgoing_expiry(&invoice, duration_since_epoch)?;
        let incoming_invoice = self
            .mint_btc_hold_invoice(
                proposal.payment_hash,
                btc_total_msat,
                outgoing_invoice_expiry_delta_seconds,
            )
            .await?;
        let btc_fee_msat = self.derive_btc_fee_msat_from_total(btc_total_msat);

        Ok(CchOrder {
            created_at: proposal.created_at,
            expiry_delta_seconds: self.config.order_expiry_delta_seconds,
            fiber_type_script: proposal.fiber_asset.clone(),
            outgoing_pay_req: proposal.submitted_invoice.clone(),
            incoming_invoice: CchInvoice::Lightning(incoming_invoice),
            payment_hash: proposal.payment_hash,
            payment_preimage: None,
            lightning_invoice_amount: btc_total_msat,
            btc_fee_msat,
            // Known up-front on `ReceiveBTC`, so the proposal carries it.
            fiber_invoice_amount: proposal.fiber_invoice_amount.unwrap_or(0),
            status: CchOrderStatus::Pending,
            failure_reason: None,
        })
    }

    /// Drop a still-pending proposal whose deadline has elapsed. No order is
    /// created — the pending proposal record is simply removed.
    fn handle_proposal_timeout(&mut self, proposal_id: Hash256) {
        let Some(payment_hash) = self.pending_proposals.remove(&proposal_id) else {
            return;
        };
        match self.get_pending_proposal_or_none(&payment_hash) {
            Ok(Some(_)) => {
                tracing::info!(
                    "Proposal {:x} for payment hash {:x} timed out; dropping it",
                    proposal_id,
                    payment_hash
                );
                self.store.delete_cch_pending_proposal(&payment_hash);
            }
            Ok(None) => {}
            Err(err) => tracing::error!(
                "Failed to load pending proposal {:x} on proposal timeout: {}",
                payment_hash,
                err
            ),
        }
    }

    async fn send_btc(&self, send_btc: SendBTC) -> Result<NewOrderOutcome, CchError> {
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        // Validate that the currency matches the configured CKB network (#981)
        if send_btc.currency != self.currency {
            return Err(CchError::CKBInvoiceNetworkMismatch {
                expected: self.currency,
                actual: send_btc.currency,
            });
        }

        let invoice = Bolt11Invoice::from_str(&send_btc.btc_pay_req)?;
        tracing::debug!(
            "BTC invoice parsed payment_hash={:x} currency={:?} has_amount={}",
            Hash256::from(*invoice.payment_hash()),
            invoice.currency(),
            invoice.amount_milli_satoshis().is_some()
        );

        // Validate that the BTC invoice network matches the expected BTC network (#978)
        let expected_ln_currency = expected_ln_currency(self.currency);
        let actual_ln_currency = invoice.currency();
        if actual_ln_currency != expected_ln_currency {
            return Err(CchError::BTCInvoiceNetworkMismatch {
                expected: format!("{:?}", expected_ln_currency),
                actual: format!("{:?}", actual_ln_currency),
            });
        }

        let payment_hash = Hash256::from(*invoice.payment_hash());

        // Validate that outgoing BTC invoice's final CLTV is less than half of incoming CKB invoice's final TLC expiry.
        // This ensures the CCH operator has sufficient time to settle the incoming side before the outgoing side expires.
        // BTC uses blocks (~10 min each), CKB uses seconds.
        let btc_final_cltv_seconds = invoice
            .min_final_cltv_expiry_delta()
            .checked_mul(600)
            .ok_or(CchError::BTCInvoiceFinalTlcExpiryDeltaTooLarge)?;
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

        // Hub fee on the BTC leg, computed in millisatoshi to avoid rounding.
        // base_fee_sats and the proportional fee are both expressed in sats in
        // the config; convert to msat at the boundary (1 sat = 1000 msat).
        let btc_fee_msat = amount_msat
            .checked_mul(self.config.fee_rate_per_million_sats as u128)
            .and_then(|v| v.checked_div(1_000_000u128))
            .and_then(|v| v.checked_add((self.config.base_fee_sats as u128) * 1_000))
            .ok_or(CchError::SendBTCOrderAmountTooLarge)?;

        let fiber_type_script = send_btc.fiber_type_script.clone();
        self.ensure_fiber_asset_allowlisted(&fiber_type_script)?;

        // `lightning_invoice_amount` always equals the Lightning-leg invoice
        // amount. For `SendBTC` the Lightning leg is the outgoing Bolt11 the hub
        // pays, whose amount is the submitted Bolt11 amount and is fee-EXCLUSIVE;
        // the hub collects `btc_fee_msat` on the Fiber (incoming) leg instead.
        let lightning_invoice_amount = amount_msat;
        // The Fiber leg must cover the Bolt11 amount plus the hub fee, so it is
        // priced off the gross (fee-inclusive) BTC amount, not the Lightning
        // invoice amount.
        let btc_gross_msat = amount_msat
            .checked_add(btc_fee_msat)
            .ok_or(CchError::SendBTCOrderAmountTooLarge)?;

        let now = duration_since_epoch.as_secs();

        // Either the fast path (fixed-rate asset, hub computes the Fiber-leg
        // amount and mints the invoice immediately) or the proposal path
        // (operator supplies the Fiber-leg amount asynchronously).
        match self.lookup_fixed_rate(&fiber_type_script) {
            Some(rate) => {
                // `rate` is smallest-units-per-sat. Round UP so the hub never
                // under-collects the Fiber leg relative to the BTC payout for
                // sub-satoshi remainders (mirrors the spec's fast-path rule).
                //   fiber_smallest_units = ceil(btc_gross_msat * rate / 1000)
                let fiber_invoice_amount = btc_gross_msat
                    .checked_mul(rate)
                    .and_then(|v| v.checked_add(999))
                    .map(|v| v / 1_000u128)
                    .ok_or(CchError::SendBTCOrderAmountTooLarge)?;

                let minted = self
                    .mint_fiber_invoice(
                        send_btc.currency,
                        payment_hash,
                        &fiber_type_script,
                        fiber_invoice_amount,
                        outgoing_invoice_expiry_delta_seconds,
                    )
                    .await?;

                let order = CchOrder {
                    created_at: now,
                    expiry_delta_seconds: self.config.order_expiry_delta_seconds,
                    failure_reason: None,
                    incoming_invoice: CchInvoice::Fiber(minted),
                    outgoing_pay_req: send_btc.btc_pay_req,
                    payment_preimage: None,
                    status: CchOrderStatus::Pending,
                    lightning_invoice_amount,
                    btc_fee_msat,
                    fiber_invoice_amount,
                    payment_hash,
                    fiber_type_script,
                };
                self.store.insert_cch_order(order.clone())?;
                Ok(NewOrderOutcome::Ready(order))
            }
            None => {
                let proposal = SwapProposal {
                    proposal_id: crate::cch::acceptor::next_proposal_id(&payment_hash),
                    order_id: payment_hash,
                    direction: SwapDirection::SendBTC,
                    payment_hash,
                    fiber_asset: fiber_type_script,
                    fiber_invoice_amount: None,
                    lightning_invoice_amount: Some(lightning_invoice_amount),
                    configured_fee_rate_per_million_sats: self.config.fee_rate_per_million_sats,
                    configured_base_fee_sats: self.config.base_fee_sats,
                    fee_on_btc_side_msat: Some(btc_fee_msat),
                    submitted_invoice: send_btc.btc_pay_req,
                    expires_at: now.saturating_add(self.config.swap_proposal_timeout_seconds),
                    created_at: now,
                };
                self.store.insert_cch_pending_proposal(proposal.clone())?;
                Ok(NewOrderOutcome::AwaitingProposal(proposal))
            }
        }
    }

    /// Build, sign, and register a Fiber (CKB) counterparty invoice for a
    /// `SendBTC` order. Shared by the fast path and the proposal-resume path.
    async fn mint_fiber_invoice(
        &self,
        currency: Currency,
        payment_hash: Hash256,
        fiber_type_script: &Option<ckb_jsonrpc_types::Script>,
        fiber_invoice_amount: u128,
        outgoing_invoice_expiry_delta_seconds: u64,
    ) -> Result<CkbInvoice, CchError> {
        let invoice_builder = InvoiceBuilder::new(currency)
            .amount(Some(fiber_invoice_amount))
            .payment_hash(payment_hash)
            .hash_algorithm(HashAlgorithm::Sha256)
            .expiry_time(Duration::from_secs(outgoing_invoice_expiry_delta_seconds))
            .final_expiry_delta(self.config.ckb_final_tlc_expiry_delta_seconds * 1000);
        let invoice_builder = if let Some(script) = fiber_type_script {
            invoice_builder.udt_type_script(script.clone().into())
        } else {
            invoice_builder
        };

        let invoice = if let Some((public_key, secret_key)) = &self.node_keypair {
            invoice_builder
                .payee_pub_key(*public_key)
                .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, secret_key))
        } else {
            invoice_builder.build()
        }?;

        self.fiber_agent_ref.call_add_invoice(invoice).await
    }

    async fn receive_btc(&self, receive_btc: ReceiveBTC) -> Result<NewOrderOutcome, CchError> {
        // `from_str` requires the invoice to carry a valid signature, so parsing
        // here also guarantees the Fiber invoice is signed.
        let invoice = CkbInvoice::from_str(&receive_btc.fiber_pay_req)?;

        // Validate that the CKB invoice currency matches the configured network (#982)
        if invoice.currency != self.currency {
            return Err(CchError::CKBInvoiceNetworkMismatch {
                expected: self.currency,
                actual: invoice.currency,
            });
        }

        let payment_hash = *invoice.payment_hash();
        // Reserve before any await / store mutation so two concurrent
        // `receive_btc` requests for the same payment hash cannot race
        // through the store's check-then-put `insert_cch_order`.
        let fiber_invoice_amount = invoice.amount().ok_or(CchError::CKBInvoiceMissingAmount)?;

        // Resolve the Fiber-side asset directly from the submitted invoice.
        // `None` denotes native CKB; otherwise the invoice's `UdtScript`
        // attribute identifies the asset. Validate against the allowlist and
        // either look up the configured fixed rate (fast path) or take the
        // proposal path through the operator acceptor.
        let fiber_type_script: Option<ckb_jsonrpc_types::Script> =
            invoice.udt_type_script().map(|s| s.clone().into());
        self.ensure_fiber_asset_allowlisted(&fiber_type_script)?;

        // Validate hash algorithm before any operator round-trip — must be
        // SHA256 for LND compatibility. Hoisted above the proposal branch so
        // obviously invalid invoices are rejected without fanning out a
        // proposal to operators.
        let hash_algorithm = invoice.hash_algorithm().copied().unwrap_or_default();
        if hash_algorithm != HashAlgorithm::Sha256 {
            return Err(CchError::CKBInvoiceIncompatibleHashAlgorithm);
        }

        // Validate that outgoing CKB invoice's final TLC is less than half of incoming BTC invoice's final CLTV expiry.
        // This ensures the CCH operator has sufficient time to settle the incoming side before the outgoing side expires.
        // CKB uses milliseconds, BTC uses blocks (~10 min each).
        let ckb_final_tlc_millis = invoice
            .final_tlc_minimum_expiry_delta()
            .copied()
            .unwrap_or(0);
        let btc_final_cltv_millis = self
            .config
            .btc_final_tlc_expiry_delta_blocks
            .checked_mul(BTC_BLOCK_TIME_MILLIS)
            .ok_or_else(|| {
                CchError::ConfigError(format!(
                    "btc_final_tlc_expiry_delta_blocks ({}) is too large and causes overflow when converting to milliseconds",
                    self.config.btc_final_tlc_expiry_delta_blocks
                ))
            })?;
        if ckb_final_tlc_millis >= btc_final_cltv_millis / 2 {
            return Err(CchError::CKBInvoiceFinalTlcExpiryDeltaTooLarge);
        }

        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;

        // Pre-validate the submitted invoice's expiry deadline before doing any
        // further work.
        if let Some(remaining) = remaining_invoice_expiry_seconds(&invoice, duration_since_epoch)? {
            if remaining < self.config.min_outgoing_invoice_expiry_delta_seconds {
                return Err(CchError::OutgoingInvoiceExpiryTooShort);
            }
        }

        match self.lookup_fixed_rate(&fiber_type_script) {
            Some(rate) => {
                // Fast path. `rate` is smallest-units-per-sat, so:
                //   btc_msat_before_fee = fiber_smallest_units * 1000 / rate
                let btc_amount_msat_before_fee = fiber_invoice_amount
                    .checked_mul(1_000u128)
                    .map(|v| v / rate)
                    .ok_or(CchError::ReceiveBTCOrderAmountTooLarge)?;

                // For `ReceiveBTC` the Lightning leg is the hold invoice the hub
                // mints; its amount is fee-INCLUSIVE. The fee is BTC-denominated
                // and computed against the fee-exclusive amount (msat).
                let fee_msat = btc_amount_msat_before_fee
                    .checked_mul(self.config.fee_rate_per_million_sats as u128)
                    .and_then(|v| v.checked_div(1_000_000u128))
                    .and_then(|v| v.checked_add((self.config.base_fee_sats as u128) * 1_000))
                    .ok_or(CchError::ReceiveBTCOrderAmountTooLarge)?;
                let lightning_invoice_amount = btc_amount_msat_before_fee
                    .checked_add(fee_msat)
                    .ok_or(CchError::ReceiveBTCOrderAmountTooLarge)?;
                // `lightning_invoice_amount` equals the minted invoice amount;
                // recover the stored fee from it with the inverse formula so both
                // the fast path and the proposal path derive `btc_fee_msat`
                // identically from the fee-inclusive Lightning-leg amount.
                let btc_fee_msat = self.derive_btc_fee_msat_from_total(lightning_invoice_amount);

                let outgoing_invoice_expiry_delta_seconds =
                    self.receive_btc_outgoing_expiry(&invoice, duration_since_epoch)?;
                let incoming_invoice = self
                    .mint_btc_hold_invoice(
                        payment_hash,
                        lightning_invoice_amount,
                        outgoing_invoice_expiry_delta_seconds,
                    )
                    .await?;

                let order = CchOrder {
                    created_at: duration_since_epoch.as_secs(),
                    expiry_delta_seconds: self.config.order_expiry_delta_seconds,
                    failure_reason: None,
                    incoming_invoice: CchInvoice::Lightning(incoming_invoice),
                    outgoing_pay_req: receive_btc.fiber_pay_req,
                    payment_preimage: None,
                    status: CchOrderStatus::Pending,
                    lightning_invoice_amount,
                    btc_fee_msat,
                    fiber_invoice_amount,
                    payment_hash,
                    fiber_type_script,
                };
                self.store.insert_cch_order(order.clone())?;
                Ok(NewOrderOutcome::Ready(order))
            }
            None => {
                // Proposal path: persist a pending proposal (no order yet) and
                // let the operator supply the BTC-leg amount asynchronously.
                // The hold invoice is minted, and the order created, on accept.
                let now = duration_since_epoch.as_secs();
                let proposal = SwapProposal {
                    proposal_id: crate::cch::acceptor::next_proposal_id(&payment_hash),
                    order_id: payment_hash,
                    direction: SwapDirection::ReceiveBTC,
                    payment_hash,
                    fiber_asset: fiber_type_script,
                    fiber_invoice_amount: Some(fiber_invoice_amount),
                    lightning_invoice_amount: None,
                    configured_fee_rate_per_million_sats: self.config.fee_rate_per_million_sats,
                    configured_base_fee_sats: self.config.base_fee_sats,
                    fee_on_btc_side_msat: None,
                    submitted_invoice: receive_btc.fiber_pay_req,
                    expires_at: now.saturating_add(self.config.swap_proposal_timeout_seconds),
                    created_at: now,
                };
                self.store.insert_cch_pending_proposal(proposal.clone())?;
                Ok(NewOrderOutcome::AwaitingProposal(proposal))
            }
        }
    }

    /// Compute the outgoing (LND hold) invoice expiry delta for a `ReceiveBTC`
    /// order from the submitted Fiber invoice's remaining lifetime.
    fn receive_btc_outgoing_expiry(
        &self,
        invoice: &CkbInvoice,
        duration_since_epoch: Duration,
    ) -> Result<u64, CchError> {
        let outgoing_invoice_expiry_delta_seconds =
            match remaining_invoice_expiry_seconds(invoice, duration_since_epoch)? {
                Some(remaining) => remaining,
                // CKB invoice has no default expiry, use minimal * 2 to create the invoice
                None => self.config.min_outgoing_invoice_expiry_delta_seconds * 2,
            };
        if outgoing_invoice_expiry_delta_seconds
            < self.config.min_outgoing_invoice_expiry_delta_seconds
        {
            return Err(CchError::OutgoingInvoiceExpiryTooShort);
        }
        Ok(outgoing_invoice_expiry_delta_seconds)
    }

    /// Create an LND hold invoice for the BTC leg of a `ReceiveBTC` order.
    /// Shared by the fast path and the proposal-resume path.
    async fn mint_btc_hold_invoice(
        &self,
        payment_hash: Hash256,
        lightning_invoice_amount: u128,
        outgoing_invoice_expiry_delta_seconds: u64,
    ) -> Result<Bolt11Invoice, CchError> {
        let total_msat = i64::try_from(lightning_invoice_amount)
            .map_err(|_| CchError::ReceiveBTCOrderAmountTooLarge)?;
        let mut client = self.lnd_connection.create_invoices_client().await?;
        let req = invoicesrpc::AddHoldInvoiceRequest {
            hash: payment_hash.as_ref().to_vec(),
            value_msat: total_msat,
            expiry: outgoing_invoice_expiry_delta_seconds as i64,
            cltv_expiry: self.config.btc_final_tlc_expiry_delta_blocks,
            ..Default::default()
        };
        let add_invoice_resp = client
            .add_hold_invoice(req.clone())
            .await
            .map_err(|err| CchError::LndRpcError(format!("{}, request: {:?}", err, req)))?
            .into_inner();
        Ok(Bolt11Invoice::from_str(&add_invoice_resp.payment_request)?)
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

    /// Map a StoreChange into CchTrackingEvent(s).
    /// This replaces the old CchFiberStoreWatcher: the mapping logic now lives in the actor.
    fn map_store_change_to_events(&self, change: &StoreChange) -> Vec<CchTrackingEvent> {
        match change {
            StoreChange::PutCkbInvoiceStatus {
                payment_hash,
                invoice_status,
            } => vec![CchTrackingEvent::InvoiceChanged {
                payment_hash: *payment_hash,
                status: *invoice_status,
                failure_reason: None,
            }],
            StoreChange::PutPaymentSession {
                payment_hash,
                payment_session,
            } => {
                use fiber_types::payment::PaymentStatus;
                let status = payment_session.status;
                // For successful payments, we need the preimage. If it's not in the same
                // store change batch, the PutPreimage event will follow.
                if status == PaymentStatus::Success {
                    // Defer to PutPreimage
                    vec![]
                } else {
                    vec![CchTrackingEvent::PaymentChanged {
                        payment_hash: *payment_hash,
                        payment_preimage: None,
                        status,
                        failure_reason: None,
                    }]
                }
            }
            StoreChange::PutPreimage {
                payment_hash,
                payment_preimage,
            } => {
                use fiber_types::payment::PaymentStatus;
                vec![CchTrackingEvent::PaymentChanged {
                    payment_hash: *payment_hash,
                    payment_preimage: Some(*payment_preimage),
                    status: PaymentStatus::Success,
                    failure_reason: None,
                }]
            }
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

fn ensure_fiber_http_url(url_opt: Option<String>) -> Result<String> {
    if let Some(url) = url_opt {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(url);
        }
    }
    Err(anyhow!("fiber_rpc_url must start with http:// or https://"))
}

fn ensure_fiber_ws_url(url_opt: Option<String>) -> Result<String> {
    let mut url = ensure_fiber_http_url(url_opt)?;
    // replace http with ws
    url.replace_range(..4, "ws");
    Ok(url)
}

/// Subscribe to store changes from a Fiber node via WebSocket RPC.
/// Reconnects automatically on failure until the cancellation token is triggered.
async fn subscribe_store_changes_ws(
    ws_url: String,
    actor: ActorRef<CchMessage>,
    token: CancellationToken,
) {
    use jsonrpsee::ws_client::WsClientBuilder;

    loop {
        if token.is_cancelled() {
            return;
        }

        tracing::info!(
            "Connecting to Fiber node WebSocket for store changes at {}",
            ws_url
        );

        let client = match WsClientBuilder::default().build(&ws_url).await {
            Ok(client) => client,
            Err(err) => {
                tracing::error!(
                    "Failed to connect to Fiber WebSocket at {}: {}. Retrying in 5s.",
                    ws_url,
                    err
                );
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    _ = token.cancelled() => return,
                }
            }
        };

        use jsonrpsee::core::client::SubscriptionClientT as _;
        let mut subscription = match client
            .subscribe::<StoreChange, _>(
                "subscribe_store_changes",
                jsonrpsee::rpc_params![],
                "unsubscribe_store_changes",
            )
            .await
        {
            Ok(sub) => sub,
            Err(err) => {
                tracing::error!(
                    "Failed to subscribe to store changes: {}. Retrying in 5s.",
                    err
                );
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    _ = token.cancelled() => return,
                }
            }
        };

        tracing::info!("Successfully subscribed to Fiber node store changes");

        loop {
            tokio::select! {
                item = subscription.next() => {
                    match item {
                        Some(Ok(change)) => {
                            let summary = redacted_store_change_summary(&change);
                            tracing::debug!(
                                "received store change via websocket kind={} payment_hash={:x} has_payment_preimage={}",
                                summary.kind,
                                summary.payment_hash,
                                summary.has_payment_preimage
                            );
                            if let Err(err) = actor.send_message(CchMessage::StoreChangeEvent(change)) {
                                tracing::error!("Failed to forward store change to CCH actor: {}", err);
                                return;
                            }
                        }
                        Some(Err(err)) => {
                            tracing::error!("WebSocket subscription error: {}. Reconnecting...", err);
                            break;
                        }
                        None => {
                            tracing::warn!("WebSocket subscription ended. Reconnecting...");
                            break;
                        }
                    }
                }
                _ = token.cancelled() => {
                    tracing::info!("Cancellation received, stopping WebSocket subscription");
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct RedactedStoreChangeSummary {
    pub kind: &'static str,
    pub payment_hash: Hash256,
    pub has_payment_preimage: bool,
}

pub(crate) fn redacted_store_change_summary(change: &StoreChange) -> RedactedStoreChangeSummary {
    match change {
        StoreChange::PutCkbInvoiceStatus { payment_hash, .. } => RedactedStoreChangeSummary {
            kind: "PutCkbInvoiceStatus",
            payment_hash: *payment_hash,
            has_payment_preimage: false,
        },
        StoreChange::PutPaymentSession { payment_hash, .. } => RedactedStoreChangeSummary {
            kind: "PutPaymentSession",
            payment_hash: *payment_hash,
            has_payment_preimage: false,
        },
        StoreChange::PutPreimage { payment_hash, .. } => RedactedStoreChangeSummary {
            kind: "PutPreimage",
            payment_hash: *payment_hash,
            has_payment_preimage: true,
        },
    }
}
