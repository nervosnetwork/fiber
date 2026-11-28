//! Strict simulation oracles and deterministic coverage accounting for `fiber-dst`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::{bail, ensure, Context as _, Result};
use fnn::fiber::channel::{ChannelActorState, ChannelActorStateStore};
use fnn::fiber::graph::NetworkGraphStateStore;
use fnn::fiber::payment::PaymentSessionExt;
use fnn::fiber::{ChannelState, Hash256, PaymentStatus};
use fnn::fiber_types::{CloseFlags, TlcErrorCode};
use fnn::invoice::{CkbInvoiceStatus, InvoiceStore};
use fnn::lsp::{LspPaymentDeliveryStatus, LspPaymentDeliveryStore, TenantId};
use fnn::simulation;
use fnn::store::NodeNamespace;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

use crate::harness::Harness;
use crate::host::NAMES;

/// Patterns of `ERROR`-level logs emitted by production Fiber actors that indicate a real
/// protocol or state-machine defect rather than an expected injected transport/disk fault.
const FORBIDDEN_ERROR_PATTERNS: &[(&str, &str)] = &[
    (
        "Unable to build and sign funding tx in state AwaitingChannelReady",
        "non-idempotent TxSignatures replay rejected as InvalidState",
    ),
    (
        "lock poisoned",
        "unexpected poisoned mutex in production store or registry",
    ),
];

/// Collected `ERROR` event captured during a single deterministic simulation run.
#[derive(Clone, Debug)]
pub(crate) struct CapturedError {
    pub(crate) target: String,
    pub(crate) message: String,
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else if self.message.is_empty() {
            self.message = format!("{}={value:?}", field.name());
        } else {
            self.message
                .push_str(&format!(" {}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else if self.message.is_empty() {
            self.message = format!("{}={value}", field.name());
        } else {
            self.message.push_str(&format!(" {}={value}", field.name()));
        }
    }
}

/// Shared handle collecting all `tracing::Level::ERROR` events emitted on the simulation thread.
#[derive(Clone, Default)]
pub(crate) struct ErrorOracle {
    errors: Arc<Mutex<Vec<CapturedError>>>,
}

impl ErrorOracle {
    pub(crate) fn layer(&self) -> ErrorOracleLayer {
        ErrorOracleLayer {
            errors: self.errors.clone(),
        }
    }

    /// Check captured `ERROR` events against forbidden protocol-violation patterns.
    /// When `allow_disk_fault_panics` is false, any `Actor unexpectedly panicked` error also fails.
    pub(crate) fn check(&self, disk_faults_armed: bool) -> Result<()> {
        let errors = self.errors.lock().expect("error oracle lock");
        for entry in errors.iter() {
            for &(pattern, reason) in FORBIDDEN_ERROR_PATTERNS {
                if entry.message.contains(pattern) {
                    bail!(
                        "ErrorOracle caught forbidden error ({reason}): [{}] {}",
                        entry.target,
                        entry.message
                    );
                }
            }
            if !disk_faults_armed
                && entry.message.contains("Actor unexpectedly panicked")
                && !entry.message.contains("disk I/O error")
                && !entry.message.contains("database or disk is full")
            {
                bail!(
                    "ErrorOracle caught unexpected actor panic: [{}] {}",
                    entry.target,
                    entry.message
                );
            }
        }
        Ok(())
    }

    /// Snapshot all captured error messages for diagnostics or targeted assertions.
    #[allow(dead_code)]
    pub(crate) fn snapshot(&self) -> Vec<CapturedError> {
        self.errors.lock().expect("error oracle lock").clone()
    }
}

pub(crate) struct ErrorOracleLayer {
    errors: Arc<Mutex<Vec<CapturedError>>>,
}

impl<S: Subscriber> Layer<S> for ErrorOracleLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::ERROR {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.errors
            .lock()
            .expect("error oracle lock")
            .push(CapturedError {
                target: event.metadata().target().to_owned(),
                message: visitor.message,
            });
    }
}

/// Install a scoped thread-local subscriber carrying `ErrorOracleLayer` (plus an optional
/// `RUST_LOG` stderr fmt layer when `RUST_LOG` is set) for the duration of `f`.
pub(crate) fn with_error_oracle<T>(f: impl FnOnce(ErrorOracle) -> T) -> T {
    let oracle = ErrorOracle::default();
    let layer = oracle.layer();
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        let subscriber = tracing_subscriber::registry().with(layer).with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || f(oracle))
    } else {
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || f(oracle))
    }
}

/// Deterministic fault and protocol-branch coverage counters accumulated across one DST run.
#[derive(Clone, Default)]
pub(crate) struct CoverageTracker {
    hits: Rc<RefCell<BTreeMap<&'static str, u64>>>,
}

impl CoverageTracker {
    pub(crate) fn hit(&self, key: &'static str) {
        *self.hits.borrow_mut().entry(key).or_insert(0) += 1;
    }

    pub(crate) fn add(&self, key: &'static str, delta: u64) {
        if delta > 0 {
            *self.hits.borrow_mut().entry(key).or_insert(0) += delta;
        }
    }

    pub(crate) fn format_summary(&self) -> String {
        let hits = self.hits.borrow();
        hits.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Identifies either a public simulated Fiber node or a hosted LSP tenant endpoint.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Endpoint {
    Public(usize),
    Tenant(TenantId),
}

#[allow(dead_code)]
fn endpoint_store(harness: &Harness, endpoint: &Endpoint) -> Result<fnn::store::Store> {
    match endpoint {
        Endpoint::Public(node) => harness.store(*node),
        Endpoint::Tenant(tenant_id) => {
            let lsp_node = harness
                .lsp_node
                .context("Endpoint::Tenant requires a configured LSP node")?;
            let store = harness.store(lsp_node)?;
            Ok(store.namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str())))
        }
    }
}

#[allow(dead_code)]
fn endpoint_channel_state(
    harness: &Harness,
    endpoint: &Endpoint,
    channel_id: &Hash256,
) -> Result<Option<ChannelActorState>> {
    match endpoint {
        Endpoint::Public(node) => Ok(harness.channel(*node, channel_id).map(|rc| (*rc).clone())),
        Endpoint::Tenant(_) => {
            let store = endpoint_store(harness, endpoint)?;
            Ok(store.get_channel_actor_state(channel_id))
        }
    }
}

#[allow(dead_code)]
fn endpoint_payment_status(
    harness: &Harness,
    endpoint: &Endpoint,
    payment_hash: Hash256,
) -> Result<Option<PaymentStatus>> {
    match endpoint {
        Endpoint::Public(node) => Ok(harness.payment_status(*node, payment_hash)),
        Endpoint::Tenant(_) => {
            let store = endpoint_store(harness, endpoint)?;
            Ok(store
                .get_payment_session(payment_hash)
                .map(|session| session.effective_status(&store)))
        }
    }
}

#[allow(dead_code)]
fn endpoint_invoice_status(
    harness: &Harness,
    endpoint: &Endpoint,
    payment_hash: Hash256,
) -> Result<Option<CkbInvoiceStatus>> {
    let store = endpoint_store(harness, endpoint)?;
    Ok(store.get_invoice_status(&payment_hash))
}

#[allow(dead_code)]
fn check_channel_liveness(
    harness: &Harness,
    scope_label: &str,
    state: &ChannelActorState,
) -> Result<()> {
    let channel_id = state.get_id();
    ensure!(
        !state.signing_context.is_awaiting_signature(),
        "LivenessOracle: {scope_label} channel {channel_id} stuck in AwaitingSignature"
    );
    ensure!(
        !matches!(state.state, ChannelState::ShuttingDown(_)),
        "LivenessOracle: {scope_label} channel {channel_id} stuck in ShuttingDown({:?})",
        state.state
    );
    ensure!(
        !state.reestablishing,
        "LivenessOracle: {scope_label} channel {channel_id} stuck in reestablishing"
    );
    ensure!(
        state.is_closed() || !state.tlc_state.waiting_ack,
        "LivenessOracle: {scope_label} channel {channel_id} stuck waiting_ack"
    );
    ensure!(
        !state.is_waiting_onchain_settlement(),
        "LivenessOracle: {scope_label} channel {channel_id} stuck waiting_onchain_settlement"
    );
    if state.is_closed() {
        if let Some(tx_hash) = state.shutdown_transaction_hash.clone() {
            let tx_hash = Hash256::from(tx_hash);
            if matches!(
                state.state,
                ChannelState::Closed(flags)
                    if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                        || flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
            ) {
                ensure!(
                    harness.chain.commitment_settled(tx_hash),
                    "LivenessOracle: {scope_label} uncooperatively closed channel {channel_id} commitment {tx_hash} not settled on chain"
                );
            } else if let Some(funding_outpoint) = state.get_funding_transaction_outpoint() {
                let funding_tx: Hash256 = funding_outpoint.tx_hash().into();
                ensure!(
                    harness.chain.funding_spend(funding_tx) == Some(tx_hash),
                    "LivenessOracle: {scope_label} cooperatively closed channel {channel_id} tx {tx_hash} not confirmed on chain"
                );
            }
        }
    }
    Ok(())
}

/// Liveness / Escape-Hatch Oracle asserting bounded convergence with zero stuck channels,
/// payments, or hosted LSP deliveries.
#[allow(dead_code)]
pub(crate) struct LivenessOracle;

#[allow(dead_code)]
impl LivenessOracle {
    pub(crate) fn assert_converged(
        harness: &Harness,
        tracked_payments: &[(Endpoint, Hash256)],
        tracked_deliveries: &[Hash256],
    ) -> Result<()> {
        for (node, name) in NAMES.iter().enumerate().take(harness.peers.len()) {
            if !harness.node_available(node) {
                continue;
            }
            let store = harness.store(node)?;
            let label = format!("node {name}");
            for state in store.get_all_channel_states() {
                check_channel_liveness(harness, &label, &state)?;
            }
        }

        if let Some(lsp_node) = harness.lsp_node {
            if harness.node_available(lsp_node) {
                let store = harness.store(lsp_node)?;
                for ns in store.hosted_tenant_namespaces() {
                    let tenant_store = store.namespaced(ns.clone());
                    let label = format!("tenant {ns:?}");
                    for state in tenant_store.get_all_channel_states() {
                        check_channel_liveness(harness, &label, &state)?;
                    }
                }
                if !tracked_deliveries.is_empty() {
                    let lsp_store = store.namespaced(NodeNamespace::lsp_metadata());
                    for &payment_hash in tracked_deliveries {
                        let deliveries = lsp_store
                            .list_lsp_payment_deliveries_by_payment_hash(&payment_hash)
                            .map_err(|err| {
                                anyhow::anyhow!(
                                    "LivenessOracle: list deliveries for {payment_hash}: {err}"
                                )
                            })?;
                        ensure!(
                            !deliveries.is_empty(),
                            "LivenessOracle: tracked LSP delivery {payment_hash} missing from store"
                        );
                        for delivery in deliveries {
                            ensure!(
                                delivery.status.is_final(),
                                "LivenessOracle: LSP delivery {payment_hash} stuck in non-final status {:?}",
                                delivery.status
                            );
                        }
                    }
                }
            }
        }

        for (endpoint, payment_hash) in tracked_payments {
            let status = endpoint_payment_status(harness, endpoint, *payment_hash)?;
            ensure!(
                status.is_some_and(|st| st.is_final()),
                "LivenessOracle: payment {payment_hash} on {endpoint:?} not final: {status:?}"
            );
        }

        Ok(())
    }
}

/// Deterministic resource & DoS-bound snapshot collected from `Harness`.
#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResourceSnapshot {
    pub(crate) sim_steps: u64,
    pub(crate) open_channels: BTreeMap<usize, usize>,
    pub(crate) pending_channels: BTreeMap<usize, usize>,
    pub(crate) actor_children: BTreeMap<usize, usize>,
    pub(crate) store_rows: BTreeMap<usize, u64>,
    pub(crate) store_bytes: BTreeMap<usize, u64>,
    pub(crate) sqlite_write_ops: BTreeMap<usize, u64>,
    pub(crate) sqlite_batch_commits: BTreeMap<usize, u64>,
    pub(crate) lsp_pending_deliveries: usize,
    pub(crate) lsp_registered_tenants: usize,
    pub(crate) lsp_active_tenants: usize,
}

/// Upper bounds checked between a baseline (`before`) and post-attack (`after`) `ResourceSnapshot`.
#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub(crate) struct ResourceLimits {
    pub(crate) max_steps: Option<u64>,
    pub(crate) max_open_channels: Option<usize>,
    pub(crate) max_pending_channels: Option<usize>,
    pub(crate) max_actor_children: Option<usize>,
    pub(crate) max_store_row_growth: Option<u64>,
    pub(crate) max_store_byte_growth: Option<u64>,
    pub(crate) max_sqlite_write_ops: Option<u64>,
    pub(crate) max_sqlite_batch_commits: Option<u64>,
    pub(crate) max_lsp_pending_deliveries: Option<usize>,
    pub(crate) max_lsp_registered_tenants: Option<usize>,
    pub(crate) max_lsp_active_tenants: Option<usize>,
}

#[allow(dead_code)]
impl ResourceLimits {
    pub(crate) fn check(&self, before: &ResourceSnapshot, after: &ResourceSnapshot) -> Result<()> {
        if let Some(limit) = self.max_steps {
            let delta = after.sim_steps.saturating_sub(before.sim_steps);
            ensure!(
                delta <= limit,
                "ResourceOracle: sim step delta {delta} exceeded max_steps {limit}"
            );
        }
        if let Some(limit) = self.max_open_channels {
            for (&node, &count) in &after.open_channels {
                ensure!(
                    count <= limit,
                    "ResourceOracle: node {} open_channels={count} exceeded limit {limit}",
                    NAMES[node]
                );
            }
        }
        if let Some(limit) = self.max_pending_channels {
            for (&node, &count) in &after.pending_channels {
                ensure!(
                    count <= limit,
                    "ResourceOracle: node {} pending_channels={count} exceeded limit {limit}",
                    NAMES[node]
                );
            }
        }
        if let Some(limit) = self.max_actor_children {
            for (&node, &count) in &after.actor_children {
                ensure!(
                    count <= limit,
                    "ResourceOracle: node {} actor_children={count} exceeded limit {limit}",
                    NAMES[node]
                );
            }
        }
        if let Some(limit) = self.max_store_row_growth {
            for (&node, &after_rows) in &after.store_rows {
                let before_rows = before.store_rows.get(&node).copied().unwrap_or(0);
                let growth = after_rows.saturating_sub(before_rows);
                ensure!(
                    growth <= limit,
                    "ResourceOracle: node {} store_row_growth={growth} ({before_rows} -> {after_rows}) exceeded limit {limit}",
                    NAMES[node]
                );
            }
        }
        if let Some(limit) = self.max_store_byte_growth {
            for (&node, &after_bytes) in &after.store_bytes {
                let before_bytes = before.store_bytes.get(&node).copied().unwrap_or(0);
                let growth = after_bytes.saturating_sub(before_bytes);
                ensure!(
                    growth <= limit,
                    "ResourceOracle: node {} store_byte_growth={growth} ({before_bytes} -> {after_bytes}) exceeded limit {limit}",
                    NAMES[node]
                );
            }
        }
        if let Some(limit) = self.max_sqlite_write_ops {
            for (&node, &after_ops) in &after.sqlite_write_ops {
                let before_ops = before.sqlite_write_ops.get(&node).copied().unwrap_or(0);
                let growth = after_ops.saturating_sub(before_ops);
                ensure!(
                    growth <= limit,
                    "ResourceOracle: node {} sqlite_write_ops_growth={growth} ({before_ops} -> {after_ops}) exceeded limit {limit}",
                    NAMES[node]
                );
            }
        }
        if let Some(limit) = self.max_sqlite_batch_commits {
            for (&node, &after_commits) in &after.sqlite_batch_commits {
                let before_commits = before.sqlite_batch_commits.get(&node).copied().unwrap_or(0);
                let growth = after_commits.saturating_sub(before_commits);
                ensure!(
                    growth <= limit,
                    "ResourceOracle: node {} sqlite_batch_commits_growth={growth} ({before_commits} -> {after_commits}) exceeded limit {limit}",
                    NAMES[node]
                );
            }
        }
        if let Some(limit) = self.max_lsp_pending_deliveries {
            ensure!(
                after.lsp_pending_deliveries <= limit,
                "ResourceOracle: lsp_pending_deliveries={} exceeded limit {limit}",
                after.lsp_pending_deliveries
            );
        }
        if let Some(limit) = self.max_lsp_registered_tenants {
            ensure!(
                after.lsp_registered_tenants <= limit,
                "ResourceOracle: lsp_registered_tenants={} exceeded limit {limit}",
                after.lsp_registered_tenants
            );
        }
        if let Some(limit) = self.max_lsp_active_tenants {
            ensure!(
                after.lsp_active_tenants <= limit,
                "ResourceOracle: lsp_active_tenants={} exceeded limit {limit}",
                after.lsp_active_tenants
            );
        }
        Ok(())
    }
}

/// Cross-Endpoint Consistency Oracle auditing payment/invoice bijection, closed-channel on-chain
/// symmetry, zero stranded on-chain settlements, and SQLite index/cache integrity.
#[allow(dead_code)]
pub(crate) struct ConsistencyOracle;

#[allow(dead_code)]
impl ConsistencyOracle {
    pub(crate) fn audit(
        harness: &Harness,
        payments: &[(Endpoint, Endpoint, Hash256)],
        closed_channels: &[(Endpoint, Endpoint, Hash256)],
    ) -> Result<()> {
        // 1. PaymentStatus::Success <-> CkbInvoiceStatus::Paid bijection.
        for (payer, payee, payment_hash) in payments {
            let pay_status = endpoint_payment_status(harness, payer, *payment_hash)?;
            let inv_status = endpoint_invoice_status(harness, payee, *payment_hash)?;
            let succeeded = pay_status == Some(PaymentStatus::Success);
            let paid = inv_status == Some(CkbInvoiceStatus::Paid);
            ensure!(
                succeeded == paid,
                "ConsistencyOracle: payment/invoice mismatch for {payment_hash}: payer={payer:?} ({pay_status:?}) vs payee={payee:?} ({inv_status:?})"
            );
            if succeeded
                && (matches!(payer, Endpoint::Tenant(_)) || matches!(payee, Endpoint::Tenant(_)))
            {
                if let Some(lsp_node) = harness.lsp_node {
                    if harness.node_available(lsp_node) {
                        let lsp_store = harness
                            .store(lsp_node)?
                            .namespaced(NodeNamespace::lsp_metadata());
                        let deliveries = lsp_store
                            .list_lsp_payment_deliveries_by_payment_hash(payment_hash)
                            .unwrap_or_default();
                        if !deliveries.is_empty() {
                            ensure!(
                                deliveries
                                    .iter()
                                    .any(|d| matches!(d.status, LspPaymentDeliveryStatus::Succeeded)),
                                "ConsistencyOracle: succeeded tenant payment {payment_hash} has no Succeeded LSP delivery"
                            );
                            ensure!(
                                deliveries.iter().all(|d| d.status.is_final()),
                                "ConsistencyOracle: succeeded tenant payment {payment_hash} has non-final LSP delivery"
                            );
                        }
                    }
                }
            }
        }

        // 2. Closed channel symmetry & on-chain spend verification.
        for (ep_a, ep_b, channel_id) in closed_channels {
            let state_a =
                endpoint_channel_state(harness, ep_a, channel_id)?.with_context(|| {
                    format!("ConsistencyOracle: missing closed channel {channel_id} on {ep_a:?}")
                })?;
            let state_b =
                endpoint_channel_state(harness, ep_b, channel_id)?.with_context(|| {
                    format!("ConsistencyOracle: missing closed channel {channel_id} on {ep_b:?}")
                })?;
            ensure!(
                state_a.is_closed() && state_b.is_closed(),
                "ConsistencyOracle: channel {channel_id} not closed on both endpoints ({ep_a:?}={:?}, {ep_b:?}={:?})",
                state_a.state,
                state_b.state
            );
            ensure!(
                state_a.shutdown_transaction_hash.is_some()
                    && state_a.shutdown_transaction_hash == state_b.shutdown_transaction_hash,
                "ConsistencyOracle: channel {channel_id} shutdown_transaction_hash mismatch ({ep_a:?}={:?}, {ep_b:?}={:?})",
                state_a.shutdown_transaction_hash,
                state_b.shutdown_transaction_hash
            );
            let close_tx = Hash256::from(
                state_a
                    .shutdown_transaction_hash
                    .clone()
                    .expect("verified Some"),
            );
            if let Some(funding_outpoint) = state_a
                .get_funding_transaction_outpoint()
                .or_else(|| state_b.get_funding_transaction_outpoint())
            {
                let funding_tx: Hash256 = funding_outpoint.tx_hash().into();
                ensure!(
                    harness.chain.funding_spend(funding_tx) == Some(close_tx),
                    "ConsistencyOracle: channel {channel_id} closing tx {close_tx} does not match on-chain funding spend {:?}",
                    harness.chain.funding_spend(funding_tx)
                );
            }
            let uncooperative = matches!(
                state_a.state,
                ChannelState::Closed(flags)
                    if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                        || flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
            ) || matches!(
                state_b.state,
                ChannelState::Closed(flags)
                    if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                        || flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
            );
            if uncooperative {
                ensure!(
                    harness.chain.commitment_settled(close_tx),
                    "ConsistencyOracle: uncooperative close {close_tx} for channel {channel_id} is not settled on chain"
                );
            }
        }

        // 3 & 4. Zero stranded waiting_onchain_settlement + payment-hash index + cache integrity
        // across all online public nodes and all hosted tenant namespaces.
        for (node, name) in NAMES.iter().enumerate().take(harness.peers.len()) {
            if !harness.node_available(node) {
                continue;
            }
            let store = harness.store(node)?;
            let label = format!("node {name}");
            audit_store_scope(&store, &label)?;
        }
        if let Some(lsp_node) = harness.lsp_node {
            if harness.node_available(lsp_node) {
                let store = harness.store(lsp_node)?;
                for ns in store.hosted_tenant_namespaces() {
                    let tenant_store = store.namespaced(ns.clone());
                    let label = format!("tenant {ns:?}");
                    audit_store_scope(&tenant_store, &label)?;
                }
            }
        }

        Ok(())
    }
}

#[allow(dead_code)]
fn audit_store_scope(store: &fnn::store::Store, label: &str) -> Result<()> {
    for state in store.get_all_channel_states() {
        let channel_id = state.get_id();
        ensure!(
            !state.is_waiting_onchain_settlement(),
            "ConsistencyOracle: {label} channel {channel_id} has stranded waiting_onchain_settlement"
        );
        if let Some(matches) = simulation::cached_channel_matches_disk(store, &channel_id) {
            ensure!(
                matches,
                "ConsistencyOracle: {label} channel {channel_id} cache diverges from disk"
            );
        }
    }
    simulation::verify_payment_hash_index(store)
        .map_err(|err| anyhow::anyhow!("ConsistencyOracle: {label} payment-hash index: {err}"))?;
    audit_payment_sessions_no_invalid_onion(store, label)?;
    Ok(())
}

/// Global Sphinx error-decoding oracle asserting that honest nodes never decode
/// `TlcErrorCode::InvalidOnionError` on any payment session or attempt.
pub(crate) struct OnionErrorOracle;

impl OnionErrorOracle {
    pub(crate) fn audit(harness: &Harness) -> Result<()> {
        for (node, name) in NAMES.iter().enumerate().take(harness.peers.len()) {
            if !harness.node_available(node) {
                continue;
            }
            let store = harness.store(node)?;
            let label = format!("node {name}");
            audit_payment_sessions_no_invalid_onion(&store, &label)?;
        }
        if let Some(lsp_node) = harness.lsp_node {
            if harness.node_available(lsp_node) {
                let store = harness.store(lsp_node)?;
                for ns in store.hosted_tenant_namespaces() {
                    let tenant_store = store.namespaced(ns.clone());
                    let label = format!("tenant {ns:?}");
                    audit_payment_sessions_no_invalid_onion(&tenant_store, &label)?;
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn audit_payment_sessions_no_invalid_onion(
    store: &fnn::store::Store,
    label: &str,
) -> Result<()> {
    for session in store.get_all_payment_sessions() {
        let payment_hash = session.payment_hash();
        ensure!(
            session.last_error_code != Some(TlcErrorCode::InvalidOnionError)
                && !session
                    .last_error
                    .as_deref()
                    .is_some_and(|err| err.contains("InvalidOnionError")),
            "OnionErrorOracle: {label} payment {payment_hash} decoded InvalidOnionError (code={:?}, last_error={:?})",
            session.last_error_code,
            session.last_error
        );
        for attempt in session.attempts() {
            ensure!(
                !attempt
                    .last_error
                    .as_deref()
                    .is_some_and(|err| err.contains("InvalidOnionError")),
                "OnionErrorOracle: {label} payment {payment_hash} attempt {} decoded InvalidOnionError (last_error={:?})",
                attempt.id,
                attempt.last_error
            );
        }
    }
    Ok(())
}
