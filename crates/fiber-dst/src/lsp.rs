use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use ckb_types::packed::{Bytes, CellInput, OutPoint, Script, Transaction};
use ckb_types::prelude::*;
use fiber_json_types::{
    ChannelSigningStatus, ChannelSigningTransition, CreatePreimageParams,
    GetChannelSigningStatusParams, GetChannelSigningStatusResult, GetInvoiceParams,
    GetInvoiceResult, GetPaymentCommandParams, GetPaymentCommandResult,
    GetWatchtowerSigningStatusParams, GetWatchtowerSigningStatusResult, InvoiceParams,
    InvoiceResult, ListChannelsParams, ListChannelsResult, NewInvoiceParams,
    OpenChannelWithExternalFundingParams, OpenChannelWithExternalFundingResult,
    PaymentStatus as JsonPaymentStatus, RegisterLspTenantParams, RegisterLspTenantResult,
    SendPaymentCommandParams, SettleInvoiceParams, ShutdownChannelParams,
    SubmitChannelSignatureParams, SubmitChannelSignatureResult, SubmitSignedFundingTxParams,
    SubmitSignedFundingTxResult, SubmitWatchtowerSignatureParams, SubmitWatchtowerSignatureResult,
    WatchtowerSigningStatus,
};
use fiber_lsp_sdk::{
    ChainVerifier, ChannelKeyId, CloseAuthorization, FundingVerifier, HostedSession,
    HostedSessionState, MemoryStore, NoopWatchtowerEventHandler, OnchainSpendAuthorization,
    OpeningNetwork, OpeningPersistence, PaymentAuthorization, ProcessOutcome, RootKey, RootSigner,
    SessionError, SigningPolicy, SubmitParams, TenantOpeningRpc, VerifiedCell,
};
use fnn::fiber::config::{DEFAULT_MIN_SHUTDOWN_FEE, MILLI_SECONDS_PER_EPOCH};
use fnn::fiber::{Hash256, Privkey, Pubkey};
use fnn::lsp::{LspConfig, LspServiceMessage, TenantId};
use fnn::simulation;

use crate::chain::SimulatedChain;
use crate::lsp_runtime::SimulatedLspRuntime;
use ractor::ActorRef;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use turmoil::net::{TcpListener, TcpStream};

use crate::host::{record, Trace, NAMES};

pub(crate) const LSP_RPC_PORT: u16 = 7300;
const RPC_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_RPC_FRAME: usize = 2 << 20;
const WALLET_POLL_INTERVAL: Duration = Duration::from_millis(25);
const WATCHTOWER_MAX_FEE: u64 = 100_000;

pub(crate) const WALLET_HOSTS: [&str; 4] = ["SDK-0", "SDK-1", "SDK-2", "SDK-3"];

#[derive(Clone)]
pub(crate) struct LspHandle {
    pub actor: ActorRef<LspServiceMessage>,
    pub admin_token: String,
    pub _runtime: Rc<SimulatedLspRuntime>,
}

pub(crate) type LspSlot = Rc<RefCell<Option<LspHandle>>>;

#[derive(Clone)]
pub(crate) struct LspHostConfig {
    pub dir: PathBuf,
    pub biscuit_seed: [u8; 32],
    pub config: LspConfig,
    pub tenant_liquidity: u128,
    pub slot: LspSlot,
}

#[derive(Serialize, Deserialize)]
struct WireRpcEnvelope {
    token: Option<String>,
    payload: String,
}

pub(crate) async fn serve_lsp_rpc(
    runtime: Rc<SimulatedLspRuntime>,
    disk: crate::sqlite_vfs::SimulatedDisk,
) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", LSP_RPC_PORT)).await?;
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            Some(result) = requests.join_next() => {
                if disk.hit().is_some() {
                    drop(requests);
                    return std::future::pending().await;
                }
                result?;
            }
            result = listener.accept() => {
                if disk.hit().is_some() {
                    drop(requests);
                    return std::future::pending().await;
                }
                let (mut socket, _) = result?;
                let runtime = runtime.clone();
                let disk = disk.clone();
                requests.spawn_local(async move {
                    let _ = tokio::time::timeout(RPC_TIMEOUT * 4, async {
                        let raw = read_message(&mut socket).await?;
                        if disk.hit().is_some() {
                            return Ok::<_, anyhow::Error>(());
                        }
                        let envelope: WireRpcEnvelope = serde_json::from_str(&raw)?;
                        let response = runtime
                            .call_rpc(envelope.token.as_deref(), &envelope.payload)
                            .await?;
                        write_message(&mut socket, &response).await?;
                        Ok::<_, anyhow::Error>(())
                    }).await;
                });
            }
        }
    }
}

/// Fault injected on the Turmoil TCP JSON-RPC transport between an external SDK wallet and LSP.
#[derive(Clone, Debug)]
pub(crate) enum LspRpcFault {
    /// Send the exact same serialized JSON-RPC request frame `copies` times back-to-back.
    DuplicateRequest {
        method: Option<&'static str>,
        copies: usize,
        once: bool,
    },
    /// Let LSP execute and commit the request, then drop the response so the wallet times out.
    DropResponseAfterCommit {
        method: Option<&'static str>,
        once: bool,
    },
    /// Drop the outgoing request before reaching the LSP.
    DropRequest {
        method: Option<&'static str>,
        once: bool,
    },
    /// Delay the outgoing request to simulate slow mobile networks.
    DelayRequest {
        method: Option<&'static str>,
        duration: Duration,
        once: bool,
    },
}

impl LspRpcFault {
    fn matches(&self, method: &str) -> bool {
        let filter = match self {
            Self::DuplicateRequest { method, .. }
            | Self::DropResponseAfterCommit { method, .. }
            | Self::DropRequest { method, .. }
            | Self::DelayRequest { method, .. } => *method,
        };
        filter.is_none_or(|expected| expected == method)
    }

    fn is_once(&self) -> bool {
        match self {
            Self::DuplicateRequest { once, .. }
            | Self::DropResponseAfterCommit { once, .. }
            | Self::DropRequest { once, .. }
            | Self::DelayRequest { once, .. } => *once,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct LspRpcControl {
    fault: Arc<Mutex<Option<LspRpcFault>>>,
    hits: Arc<AtomicU64>,
    duplicates_sent: Arc<AtomicU64>,
    responses_dropped: Arc<AtomicU64>,
    events: Arc<Mutex<Vec<String>>>,
}

impl LspRpcControl {
    pub fn set_fault(&self, fault: Option<LspRpcFault>) {
        *self.fault.lock().unwrap() = fault;
    }

    pub fn duplicates_sent(&self) -> u64 {
        self.duplicates_sent.load(Ordering::Relaxed)
    }

    pub fn responses_dropped(&self) -> u64 {
        self.responses_dropped.load(Ordering::Relaxed)
    }

    pub fn flush_events(&self, trace: &Trace) {
        let mut events = self.events.lock().unwrap();
        for event in events.drain(..) {
            record(trace, event);
        }
    }

    fn push_event(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }

    fn take_matching_fault(&self, method: &str) -> Option<LspRpcFault> {
        let mut slot = self.fault.lock().unwrap();
        let fault = slot.as_ref()?.clone();
        if !fault.matches(method) {
            return None;
        }
        if fault.is_once() {
            slot.take();
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(fault)
    }
}

#[derive(Clone)]
struct LspRpcClient {
    wallet_name: &'static str,
    lsp_host: &'static str,
    control: LspRpcControl,
}

impl LspRpcClient {
    async fn send_once(&self, token: Option<&str>, payload: &str) -> Result<String> {
        let envelope = serde_json::to_string(&WireRpcEnvelope {
            token: token.map(str::to_owned),
            payload: payload.to_owned(),
        })?;
        let response = tokio::time::timeout(RPC_TIMEOUT, async {
            let mut socket = TcpStream::connect((self.lsp_host, LSP_RPC_PORT)).await?;
            write_message(&mut socket, &envelope).await?;
            read_message(&mut socket).await
        })
        .await
        .map_err(|_| anyhow!("LSP RPC timeout"))??;
        Ok(response)
    }

    async fn call<R: DeserializeOwned>(
        &self,
        token: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<R> {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        })
        .to_string();
        let fault = self.control.take_matching_fault(method);
        let raw_response = match fault {
            Some(LspRpcFault::DropRequest { .. }) => {
                self.control
                    .push_event(format!("{} RPC dropped request {method}", self.wallet_name));
                bail!("simulated dropped RPC request {method}");
            }
            Some(LspRpcFault::DelayRequest { duration, .. }) => {
                self.control.push_event(format!(
                    "{} RPC delayed request {method} by {}ms",
                    self.wallet_name,
                    duration.as_millis()
                ));
                tokio::time::sleep(duration).await;
                self.send_once(token, &payload).await?
            }
            Some(LspRpcFault::DropResponseAfterCommit { .. }) => {
                let _committed = self.send_once(token, &payload).await?;
                self.control
                    .responses_dropped
                    .fetch_add(1, Ordering::Relaxed);
                self.control.push_event(format!(
                    "{} RPC dropped response after commit {method}",
                    self.wallet_name
                ));
                bail!("simulated dropped RPC response after commit for {method}");
            }
            Some(LspRpcFault::DuplicateRequest { copies, .. }) => {
                let copies = copies.max(2);
                let first = self.send_once(token, &payload).await?;
                for copy in 1..copies {
                    self.control.duplicates_sent.fetch_add(1, Ordering::Relaxed);
                    let dup = self.send_once(token, &payload).await;
                    let summary = match &dup {
                        Ok(body) => {
                            let val: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                            if let Some(err) = val.get("error") {
                                format!("err={}", err["message"].as_str().unwrap_or("rpc error"))
                            } else {
                                format!("ok={}", val["result"])
                            }
                        }
                        Err(err) => format!("transport_err={err}"),
                    };
                    self.control.push_event(format!(
                        "{} RPC duplicate #{copy} {method} -> {summary}",
                        self.wallet_name
                    ));
                }
                first
            }
            None => self.send_once(token, &payload).await?,
        };
        let value: Value = serde_json::from_str(&raw_response)
            .with_context(|| format!("decode JSON-RPC response for {method}: {raw_response}"))?;
        if let Some(error) = value.get("error") {
            bail!(
                "RPC {method} error ({}): {}",
                error["code"],
                error["message"].as_str().unwrap_or(&error.to_string())
            );
        }
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value(result)
            .with_context(|| format!("deserialize RPC {method} result from {raw_response}"))
    }
}

/// One-shot crash fault injected at exact persistence/RPC boundaries inside `WalletHost`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SdkSignerFault {
    /// Crash the SDK wallet immediately after `confirm()` mutates in-memory signer state,
    /// BEFORE writing `sdk_store.bin` to disk and BEFORE submitting `submit_channel_signature`.
    SkipPersistAfterConfirmCrash,
    /// Crash the SDK wallet immediately after `confirm()` persists `sdk_store.bin` to disk,
    /// BEFORE transmitting `submit_channel_signature` to the LSP.
    CrashAfterPersistBeforeSubmit,
}

pub(crate) struct WalletSlot {
    pub generation: u64,
    pub online: bool,
    pub tenant_id: Option<TenantId>,
    pub bound_channels: Vec<Hash256>,
    pub commands: Option<mpsc::UnboundedSender<WalletCommand>>,
    pub signing_paused: Rc<Cell<bool>>,
    pub sdk_fault: Rc<Cell<Option<SdkSignerFault>>>,
    pub sdk_faults_hit: u64,
    pub rpc: LspRpcControl,
    pub channel_sigs_applied: u64,
    pub channel_sigs_already_applied: u64,
    pub closing_sigs_applied: u64,
    pub token_refreshes: u64,
    pub watchtower_sigs_applied: u64,
    pub watchtower_sigs_already_applied: u64,
    pub watchtower_backoff_hits: u64,
    pub last_registration: Option<RegisterLspTenantParams>,
    pub last_channel_sig: Option<SubmitChannelSignatureParams>,
}

impl Default for WalletSlot {
    fn default() -> Self {
        Self {
            generation: 0,
            online: false,
            tenant_id: None,
            bound_channels: Vec::new(),
            commands: None,
            signing_paused: Rc::new(Cell::new(false)),
            sdk_fault: Rc::new(Cell::new(None)),
            sdk_faults_hit: 0,
            rpc: LspRpcControl::default(),
            channel_sigs_applied: 0,
            channel_sigs_already_applied: 0,
            closing_sigs_applied: 0,
            token_refreshes: 0,
            watchtower_sigs_applied: 0,
            watchtower_sigs_already_applied: 0,
            watchtower_backoff_hits: 0,
            last_registration: None,
            last_channel_sig: None,
        }
    }
}

pub(crate) type Wallets = Rc<RefCell<Vec<WalletSlot>>>;

pub(crate) enum WalletCommand {
    Register {
        reply: oneshot::Sender<Result<TenantId>>,
    },
    OpenChannel {
        funding_amount: u128,
        reply: oneshot::Sender<Result<Hash256>>,
    },
    CreateHoldInvoice {
        amount: u128,
        payment_hash: Hash256,
        preimage: Hash256,
        reply: oneshot::Sender<Result<InvoiceResult>>,
    },
    SettleInvoice {
        payment_hash: Hash256,
        preimage: Hash256,
        reply: oneshot::Sender<Result<()>>,
    },
    CancelInvoice {
        payment_hash: Hash256,
        reply: oneshot::Sender<Result<GetInvoiceResult>>,
    },
    GetInvoice {
        payment_hash: Hash256,
        reply: oneshot::Sender<Result<GetInvoiceResult>>,
    },
    SendPayment {
        channel_id: Hash256,
        invoice: String,
        amount: u128,
        payment_hash: Hash256,
        use_trampoline: bool,
        reply: oneshot::Sender<Result<GetPaymentCommandResult>>,
    },
    CooperativeClose {
        channel_id: Hash256,
        fee_rate: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    ListChannels {
        reply: oneshot::Sender<Result<ListChannelsResult>>,
    },
    ReplayLastRegistration {
        reply: oneshot::Sender<Result<String>>,
    },
    ReplayLastChannelSignature {
        conflicting: bool,
        reply: oneshot::Sender<Result<String>>,
    },
    CreateWatchPreimage {
        payment_hash: Hash256,
        preimage: Hash256,
        reply: oneshot::Sender<Result<()>>,
    },
}

struct SimChainVerifier {
    chain: SimulatedChain,
}

#[async_trait::async_trait]
impl ChainVerifier for SimChainVerifier {
    async fn live_cell(
        &self,
        outpoint: &OutPoint,
    ) -> Result<VerifiedCell, fiber_lsp_sdk::SignerError> {
        let (output, data) = self
            .chain
            .live_cell(outpoint)
            .ok_or_else(|| fiber_lsp_sdk::SignerError::InvalidContent("cell is not live".into()))?;
        Ok(VerifiedCell { output, data })
    }

    async fn verify_commitment_lineage(
        &self,
        source: Hash256,
        outpoint: &OutPoint,
    ) -> Result<(), fiber_lsp_sdk::SignerError> {
        self.chain
            .verify_commitment_lineage(source, outpoint)
            .map_err(fiber_lsp_sdk::SignerError::InvalidContent)
    }

    async fn verify_maturity(
        &self,
        _inputs: &[CellInput],
    ) -> Result<(), fiber_lsp_sdk::SignerError> {
        Ok(())
    }

    async fn median_time_ms(&self) -> Result<u64, fiber_lsp_sdk::SignerError> {
        Ok(fnn::now_timestamp_as_millis_u64())
    }
}

struct SimOpeningAdapter<'a> {
    rpc: &'a LspRpcClient,
    session_file: &'a Path,
    store: &'a MemoryStore,
    store_file: &'a Path,
    pending_outbound: &'a BTreeMap<Hash256, Hash256>,
    local_initiated_close: &'a BTreeSet<Hash256>,
}

#[async_trait::async_trait]
impl TenantOpeningRpc for SimOpeningAdapter<'_> {
    async fn open_channel_with_external_funding(
        &self,
        token: &str,
        request: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, SessionError> {
        self.rpc
            .call(
                Some(token),
                "open_channel_with_external_funding",
                json!([request]),
            )
            .await
            .map_err(|err| SessionError::Invalid(err.to_string()))
    }
}

#[async_trait::async_trait]
impl FundingVerifier for SimOpeningAdapter<'_> {
    async fn verify_funding(
        &self,
        _request: &OpenChannelWithExternalFundingParams,
        result: &OpenChannelWithExternalFundingResult,
    ) -> Result<Vec<OutPoint>, SessionError> {
        Ok(result
            .unsigned_funding_tx
            .inputs
            .iter()
            .map(|input| input.previous_output.clone().into())
            .collect())
    }
}

#[async_trait::async_trait]
impl OpeningPersistence for SimOpeningAdapter<'_> {
    async fn save_session(&self, state: &HostedSessionState) -> Result<(), SessionError> {
        persist_wallet_files(
            self.store,
            self.store_file,
            state,
            self.pending_outbound,
            self.local_initiated_close,
            self.session_file,
        )
        .map_err(|err| SessionError::Invalid(err.to_string()))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersistedWalletSession {
    tenant_token: Option<String>,
    token_expires_at_ms: Option<u64>,
    bindings: BTreeMap<String, String>,
    pending: Option<String>,
    opening_request: Option<Vec<u8>>,
    #[serde(default)]
    pending_outbound: BTreeMap<String, String>,
    #[serde(default)]
    local_initiated_close: BTreeSet<String>,
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn persist_wallet_files(
    store: &MemoryStore,
    store_file: &Path,
    state: &HostedSessionState,
    pending_outbound: &BTreeMap<Hash256, Hash256>,
    local_initiated_close: &BTreeSet<Hash256>,
    session_file: &Path,
) -> Result<()> {
    let snapshot = store
        .snapshot()
        .map_err(|err| anyhow!("snapshot SDK MemoryStore: {err}"))?;
    atomic_write(store_file, &snapshot)?;
    let persisted = PersistedWalletSession {
        tenant_token: state.tenant_token.clone(),
        token_expires_at_ms: state.token_expires_at_ms,
        bindings: state
            .bindings
            .iter()
            .map(|(channel, key)| (format!("{channel:#x}"), format!("{:#x}", key.0)))
            .collect(),
        pending: state.pending.map(|key| format!("{:#x}", key.0)),
        opening_request: state.opening_request.clone(),
        pending_outbound: pending_outbound
            .iter()
            .map(|(payment_hash, channel_id)| {
                (format!("{payment_hash:#x}"), format!("{channel_id:#x}"))
            })
            .collect(),
        local_initiated_close: local_initiated_close
            .iter()
            .map(|channel_id| format!("{channel_id:#x}"))
            .collect(),
    };
    atomic_write(session_file, &serde_json::to_vec(&persisted)?)?;
    Ok(())
}

fn parse_hex_hash(value: &str) -> Result<Hash256> {
    let bytes = hex::decode(value.trim_start_matches("0x"))?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected 32-byte hash"))?;
    Ok(Hash256::from(array))
}

#[allow(clippy::type_complexity)]
fn load_wallet_files(
    store_file: &Path,
    session_file: &Path,
) -> Result<(
    MemoryStore,
    HostedSessionState,
    BTreeMap<Hash256, Hash256>,
    BTreeSet<Hash256>,
    bool,
)> {
    let existed = store_file.exists();
    let store = if existed {
        let bytes = std::fs::read(store_file)?;
        MemoryStore::from_snapshot(&bytes)
            .map_err(|err| anyhow!("restore SDK MemoryStore: {err}"))?
    } else {
        MemoryStore::default()
    };
    let mut pending_outbound = BTreeMap::new();
    let mut local_initiated_close = BTreeSet::new();
    let state = if session_file.exists() {
        let persisted: PersistedWalletSession =
            serde_json::from_slice(&std::fs::read(session_file)?)?;
        let mut bindings = std::collections::HashMap::new();
        for (channel, key) in persisted.bindings {
            bindings.insert(
                parse_hex_hash(&channel)?,
                ChannelKeyId(parse_hex_hash(&key)?),
            );
        }
        for (payment_hash, channel_id) in persisted.pending_outbound {
            pending_outbound.insert(parse_hex_hash(&payment_hash)?, parse_hex_hash(&channel_id)?);
        }
        for channel_id in persisted.local_initiated_close {
            local_initiated_close.insert(parse_hex_hash(&channel_id)?);
        }
        HostedSessionState {
            tenant_token: persisted.tenant_token,
            token_expires_at_ms: persisted.token_expires_at_ms,
            bindings,
            pending: persisted
                .pending
                .map(|key| parse_hex_hash(&key).map(ChannelKeyId))
                .transpose()?,
            opening_request: persisted.opening_request,
        }
    } else {
        HostedSessionState::default()
    };
    Ok((
        store,
        state,
        pending_outbound,
        local_initiated_close,
        existed,
    ))
}

#[derive(Clone)]
pub(crate) struct WalletHost {
    pub index: usize,
    pub root_key: [u8; 32],
    pub funding_key: Privkey,
    pub lsp_index: usize,
    pub lsp_pubkey: Pubkey,
    pub lsp_slot: LspSlot,
    pub path: PathBuf,
    pub chain: SimulatedChain,
    pub wallets: Wallets,
    pub trace: Trace,
}

impl WalletHost {
    pub async fn run(self) -> Result<()> {
        let name = WALLET_HOSTS[self.index];
        let store_file = self.path.join("sdk_store.bin");
        let session_file = self.path.join("session_state.json");
        let (store, persisted_state, mut pending_outbound, mut local_initiated_close, existed) =
            load_wallet_files(&store_file, &session_file)?;
        let root_key = RootKey::import(self.root_key)
            .map_err(|err| anyhow!("import wallet root key: {err}"))?;
        let root_signer = if existed {
            RootSigner::open(root_key, store.clone())
                .await
                .map_err(|err| anyhow!("open persisted RootSigner: {err}"))?
        } else {
            RootSigner::create(root_key, store.clone())
                .await
                .map_err(|err| anyhow!("create RootSigner: {err}"))?
        };
        let mut session = HostedSession::new(root_signer)
            .with_policy(SigningPolicy::Always)
            .with_state(persisted_state);
        persist_wallet_files(
            &store,
            &store_file,
            session.state(),
            &pending_outbound,
            &local_initiated_close,
            &session_file,
        )?;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (generation, rpc_control, signing_paused, sdk_fault) = {
            let mut wallets = self.wallets.borrow_mut();
            let slot = &mut wallets[self.index];
            slot.generation += 1;
            slot.online = true;
            slot.tenant_id = Some(session.tenant_id());
            slot.bound_channels = sorted_bindings(session.state());
            slot.commands = Some(cmd_tx);
            (
                slot.generation,
                slot.rpc.clone(),
                slot.signing_paused.clone(),
                slot.sdk_fault.clone(),
            )
        };
        record(
            &self.trace,
            format_args!("{name} start {generation} tenant={}", session.tenant_id()),
        );

        let rpc = LspRpcClient {
            wallet_name: name,
            lsp_host: NAMES[self.lsp_index],
            control: rpc_control,
        };
        let wallet_lock = SimulatedChain::wallet_lock_script(self.funding_key.clone());
        let mut poll_tick = tokio::time::interval(WALLET_POLL_INTERVAL);
        poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        WalletCommand::Register { reply } => {
                            let res = self.register_tenant(
                                &mut session,
                                &store,
                                &store_file,
                                &pending_outbound,
                                &local_initiated_close,
                                &session_file,
                                &rpc,
                                false,
                            ).await;
                            let _ = reply.send(res);
                        }
                        WalletCommand::OpenChannel { funding_amount, reply } => {
                            let res = self.open_channel(
                                &mut session,
                                &store,
                                &store_file,
                                &mut pending_outbound,
                                &mut local_initiated_close,
                                &session_file,
                                &rpc,
                                &wallet_lock,
                                funding_amount,
                            ).await;
                            let _ = reply.send(res);
                        }
                        WalletCommand::CreateHoldInvoice { amount, payment_hash, preimage, reply } => {
                            let res = async {
                                let now = fnn::now_timestamp_as_millis_u64();
                                let channels = sorted_bindings(session.state());
                                ensure!(!channels.is_empty(), "wallet has no bound channel for invoice");
                                for bound_channel in channels {
                                    let signer = session
                                        .channel_signer(bound_channel)
                                        .await
                                        .map_err(|err| anyhow!("channel_signer for invoice: {err}"))?;
                                    signer
                                        .authorize_invoice(
                                            PaymentAuthorization {
                                                payment_hash,
                                                hash_algorithm: fnn::fiber_types::HashAlgorithm::CkbHash,
                                                inbound: true,
                                                amount,
                                                expires_at_ms: now.saturating_add(7_200_000),
                                                min_tlc_expiry_delta_ms: 1,
                                                max_tlc_expiry_ms: now.saturating_add(30 * 86_400_000),
                                            },
                                            preimage,
                                        )
                                        .await
                                        .map_err(|err| anyhow!("SDK authorize_invoice: {err}"))?;
                                }
                                persist_wallet_files(
                                    &store,
                                    &store_file,
                                    session.state(),
                                    &pending_outbound,
                                    &local_initiated_close,
                                    &session_file,
                                )?;
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let params = NewInvoiceParams {
                                    amount,
                                    description: Some("dst-lsp-hold".to_string()),
                                    currency: fiber_json_types::Currency::Fibd,
                                    payment_preimage: None,
                                    payment_hash: Some(payment_hash.into()),
                                    expiry: Some(3600),
                                    fallback_address: None,
                                    final_expiry_delta: None,
                                    udt_type_script: None,
                                    hash_algorithm: Some(fiber_json_types::HashAlgorithm::CkbHash),
                                    allow_mpp: None,
                                    allow_trampoline_routing: Some(true),
                                };
                                rpc.call::<InvoiceResult>(Some(&token), "new_invoice", json!([params])).await
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::SettleInvoice { payment_hash, preimage, reply } => {
                            let res = async {
                                let mut authorized = false;
                                for channel_id in sorted_bindings(session.state()) {
                                    let signer = session
                                        .channel_signer(channel_id)
                                        .await
                                        .map_err(|err| anyhow!("channel_signer for settle: {err}"))?;
                                    match signer.authorize_fulfillment(payment_hash, preimage).await {
                                        Ok(()) => {
                                            authorized = true;
                                        }
                                        Err(err) if err.to_string().contains("unknown payment") => continue,
                                        Err(err) => return Err(anyhow!("SDK authorize_fulfillment: {err}")),
                                    }
                                }
                                if !authorized {
                                    bail!("no bound channel has invoice authorization for {payment_hash}");
                                }
                                persist_wallet_files(
                                    &store,
                                    &store_file,
                                    session.state(),
                                    &pending_outbound,
                                    &local_initiated_close,
                                    &session_file,
                                )?;
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let params = SettleInvoiceParams {
                                    payment_hash: payment_hash.into(),
                                    payment_preimage: preimage.into(),
                                };
                                rpc.call::<Value>(Some(&token), "settle_invoice", json!([params])).await?;
                                Ok(())
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::CancelInvoice { payment_hash, reply } => {
                            let res = async {
                                for channel_id in sorted_bindings(session.state()) {
                                    let signer = session
                                        .channel_signer(channel_id)
                                        .await
                                        .map_err(|err| anyhow!("channel_signer for cancel: {err}"))?;
                                    match signer.authorize_cancellation(payment_hash).await {
                                        Ok(()) => {}
                                        Err(err) if err.to_string().contains("unknown payment") => continue,
                                        Err(err) => return Err(anyhow!("SDK authorize_cancellation: {err}")),
                                    }
                                }
                                persist_wallet_files(
                                    &store,
                                    &store_file,
                                    session.state(),
                                    &pending_outbound,
                                    &local_initiated_close,
                                    &session_file,
                                )?;
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let params = InvoiceParams {
                                    payment_hash: payment_hash.into(),
                                };
                                rpc.call::<GetInvoiceResult>(Some(&token), "cancel_invoice", json!([params])).await
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::GetInvoice { payment_hash, reply } => {
                            let res = async {
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let params = GetInvoiceParams {
                                    payment_hash: payment_hash.into(),
                                    tenant_id: None,
                                };
                                rpc.call::<GetInvoiceResult>(Some(&token), "get_invoice", json!([params])).await
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::SendPayment { channel_id, invoice, amount, payment_hash, use_trampoline, reply } => {
                            let res = async {
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let trampoline_hops = use_trampoline.then(|| vec![self.lsp_pubkey.into()]);
                                let make_params = |dry_run: bool| SendPaymentCommandParams {
                                    target_pubkey: None,
                                    amount: None,
                                    payment_hash: None,
                                    final_tlc_expiry_delta: None,
                                    tlc_expiry_limit: None,
                                    invoice: Some(invoice.clone()),
                                    timeout: None,
                                    max_fee_amount: None,
                                    max_fee_rate: None,
                                    max_parts: None,
                                    trampoline_hops: trampoline_hops.clone(),
                                    keysend: None,
                                    udt_type_script: None,
                                    allow_self_payment: None,
                                    custom_records: None,
                                    hop_hints: None,
                                    dry_run: dry_run.then_some(true),
                                };
                                let dry = rpc
                                    .call::<GetPaymentCommandResult>(
                                        Some(&token),
                                        "send_payment",
                                        json!([make_params(true)]),
                                    )
                                    .await?;
                                let now = fnn::now_timestamp_as_millis_u64();
                                let _ = channel_id;
                                for bound_channel in sorted_bindings(session.state()) {
                                    let signer = session
                                        .channel_signer(bound_channel)
                                        .await
                                        .map_err(|err| anyhow!("channel_signer for payment: {err}"))?;
                                    signer
                                        .authorize_payment(PaymentAuthorization {
                                            payment_hash,
                                            hash_algorithm: fnn::fiber_types::HashAlgorithm::CkbHash,
                                            inbound: false,
                                            amount: amount.saturating_add(dry.fee),
                                            expires_at_ms: now.saturating_add(7_200_000),
                                            min_tlc_expiry_delta_ms: 1,
                                            max_tlc_expiry_ms: now.saturating_add(30 * 86_400_000),
                                        })
                                        .await
                                        .map_err(|err| anyhow!("SDK authorize_payment: {err}"))?;
                                }
                                pending_outbound.insert(payment_hash, channel_id);
                                persist_wallet_files(
                                    &store,
                                    &store_file,
                                    session.state(),
                                    &pending_outbound,
                                    &local_initiated_close,
                                    &session_file,
                                )?;
                                rpc.call::<GetPaymentCommandResult>(
                                    Some(&token),
                                    "send_payment",
                                    json!([make_params(false)]),
                                )
                                .await
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::CooperativeClose { channel_id, fee_rate, reply } => {
                            let res = async {
                                local_initiated_close.insert(channel_id);
                                persist_wallet_files(
                                    &store,
                                    &store_file,
                                    session.state(),
                                    &pending_outbound,
                                    &local_initiated_close,
                                    &session_file,
                                )?;
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let params = ShutdownChannelParams {
                                    channel_id: channel_id.into(),
                                    close_script: Some(wallet_lock.clone().into()),
                                    fee_rate: Some(fee_rate),
                                    force: Some(false),
                                };
                                rpc.call::<Value>(Some(&token), "shutdown_channel", json!([params])).await?;
                                Ok(())
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::ListChannels { reply } => {
                            let res = async {
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let params = ListChannelsParams {
                                    pubkey: None,
                                    include_closed: Some(true),
                                    only_pending: None,
                                };
                                rpc.call::<ListChannelsResult>(Some(&token), "list_channels", json!([params])).await
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::ReplayLastRegistration { reply } => {
                            let res = async {
                                let params = self.wallets.borrow()[self.index]
                                    .last_registration
                                    .clone()
                                    .context("no previous registration to replay")?;
                                let admin_token = self.admin_token()?;
                                match rpc
                                    .call::<RegisterLspTenantResult>(
                                        Some(&admin_token),
                                        "lsp_register_tenant",
                                        json!([params]),
                                    )
                                    .await
                                {
                                    Ok(_) => Ok("unexpected_ok".to_string()),
                                    Err(err) => Ok(err.to_string()),
                                }
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::ReplayLastChannelSignature { conflicting, reply } => {
                            let res = async {
                                let mut params = self.wallets.borrow()[self.index]
                                    .last_channel_sig
                                    .clone()
                                    .context("no previous channel signature to replay")?;
                                if conflicting {
                                    params.partial_signature[0] ^= 0xff;
                                }
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                match rpc
                                    .call::<SubmitChannelSignatureResult>(
                                        Some(&token),
                                        "submit_channel_signature",
                                        json!([params]),
                                    )
                                    .await
                                {
                                    Ok(result) => Ok(format!("{result:?}")),
                                    Err(err) => Ok(err.to_string()),
                                }
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                        WalletCommand::CreateWatchPreimage { payment_hash, preimage, reply } => {
                            let res = async {
                                let token = session.tenant_token().context("wallet not registered")?.to_owned();
                                let params = CreatePreimageParams {
                                    payment_hash: payment_hash.into(),
                                    preimage: preimage.into(),
                                };
                                rpc.call::<Value>(Some(&token), "create_preimage", json!([params])).await?;
                                Ok(())
                            }.await;
                            rpc.control.flush_events(&self.trace);
                            let _ = reply.send(res);
                        }
                    }
                }
                _ = poll_tick.tick() => {
                    if !signing_paused.get() && session.tenant_token().is_some() {
                        if let Ok(true) = self.poll_all_channels(
                            &mut session,
                            &store,
                            &store_file,
                            &mut pending_outbound,
                            &mut local_initiated_close,
                            &session_file,
                            &rpc,
                            &wallet_lock,
                            &sdk_fault,
                        ).await {
                            let mut wallets = self.wallets.borrow_mut();
                            let slot = &mut wallets[self.index];
                            slot.online = false;
                            slot.commands = None;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    fn admin_token(&self) -> Result<String> {
        self.lsp_slot
            .borrow()
            .as_ref()
            .map(|handle| handle.admin_token.clone())
            .context("LSP host is offline")
    }

    #[allow(clippy::too_many_arguments)]
    async fn register_tenant(
        &self,
        session: &mut HostedSession<MemoryStore>,
        store: &MemoryStore,
        store_file: &Path,
        pending_outbound: &BTreeMap<Hash256, Hash256>,
        local_initiated_close: &BTreeSet<Hash256>,
        session_file: &Path,
        rpc: &LspRpcClient,
        force: bool,
    ) -> Result<TenantId> {
        if !force && session.tenant_token().is_some() {
            return Ok(session.tenant_id());
        }
        let mut request_id_bytes = [0u8; 32];
        simulation::fill_random(&mut request_id_bytes);
        let request_id = Hash256::from(request_id_bytes);
        let signed_at = fnn::now_timestamp_as_millis_u64();
        let params = session
            .begin_registration(self.lsp_pubkey, request_id, signed_at)
            .map_err(|err| anyhow!("begin registration: {err}"))?;
        self.wallets.borrow_mut()[self.index].last_registration = Some(params.clone());
        let admin_token = self.admin_token()?;
        let registered: RegisterLspTenantResult = rpc
            .call(Some(&admin_token), "lsp_register_tenant", json!([params]))
            .await?;
        session
            .finish_registration(registered)
            .map_err(|err| anyhow!("finish registration: {err}"))?;
        persist_wallet_files(
            store,
            store_file,
            session.state(),
            pending_outbound,
            local_initiated_close,
            session_file,
        )?;
        if force {
            self.wallets.borrow_mut()[self.index].token_refreshes += 1;
            record(
                &self.trace,
                format_args!(
                    "{} refreshed token tenant={}",
                    WALLET_HOSTS[self.index],
                    session.tenant_id()
                ),
            );
        } else {
            record(
                &self.trace,
                format_args!(
                    "{} registered tenant={}",
                    WALLET_HOSTS[self.index],
                    session.tenant_id()
                ),
            );
        }
        Ok(session.tenant_id())
    }

    #[allow(clippy::too_many_arguments)]
    async fn open_channel(
        &self,
        session: &mut HostedSession<MemoryStore>,
        store: &MemoryStore,
        store_file: &Path,
        pending_outbound: &mut BTreeMap<Hash256, Hash256>,
        local_initiated_close: &mut BTreeSet<Hash256>,
        session_file: &Path,
        rpc: &LspRpcClient,
        wallet_lock: &Script,
        funding_amount: u128,
    ) -> Result<Hash256> {
        let material = if let Some(key_id) = session.pending_channel_key_id() {
            session
                .open_channel(key_id)
                .await
                .map_err(|err| anyhow!("open pending channel signer: {err}"))?
                .channel_open_material(false)
                .await
                .map_err(|err| anyhow!("pending channel open material: {err}"))?
        } else {
            let material = session
                .allocate_pending_channel()
                .await
                .map_err(|err| anyhow!("allocate pending channel: {err}"))?;
            persist_wallet_files(
                store,
                store_file,
                session.state(),
                pending_outbound,
                local_initiated_close,
                session_file,
            )?;
            material
        };
        let (funding_lock, commitment_lock, commitment_cell_deps) =
            self.chain.opening_network_scripts();
        let network = OpeningNetwork {
            funding_lock,
            commitment_lock,
            commitment_cell_deps,
            epoch_duration_ms: MILLI_SECONDS_PER_EPOCH,
            minimum_shutdown_fee: DEFAULT_MIN_SHUTDOWN_FEE,
        };
        let request = OpenChannelWithExternalFundingParams {
            pubkey: self.lsp_pubkey.into(),
            funding_amount,
            funding_lock_script: wallet_lock.clone().into(),
            funding_lock_script_cell_deps: None,
            funding_udt_type_script: None,
            shutdown_script: wallet_lock.clone().into(),
            public: Some(false),
            commitment_delay_epoch: Some(
                ckb_types::core::EpochNumberWithFraction::new(1, 0, 1)
                    .full_value()
                    .into(),
            ),
            commitment_fee_rate: Some(1_000),
            funding_fee_rate: Some(1_000),
            tlc_expiry_delta: None,
            tlc_min_value: None,
            tlc_fee_proportional_millionths: Some(0),
            max_tlc_value_in_flight: None,
            max_tlc_number_in_flight: None,
            external_channel_signer: Some(fiber_lsp_sdk::json::open_material_to_rpc(&material)),
        };
        let adapter = SimOpeningAdapter {
            rpc,
            session_file,
            store,
            store_file,
            pending_outbound,
            local_initiated_close,
        };
        let mut open_result = None;
        for attempt in 0..4 {
            match session
                .open_tenant_channel(request.clone(), &network, &adapter, &adapter, &adapter)
                .await
            {
                Ok(result) => {
                    open_result = Some(result);
                    break;
                }
                Err(err)
                    if attempt < 3
                        && (err.to_string().contains("simulated dropped")
                            || err.to_string().contains("timeout")) =>
                {
                    rpc.control.flush_events(&self.trace);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(err) => return Err(anyhow!("open_tenant_channel: {err}")),
            }
        }
        let result = open_result.context("open_tenant_channel exhausted retries")?;
        let channel_id: Hash256 = result.channel_id.into();
        persist_wallet_files(
            store,
            store_file,
            session.state(),
            pending_outbound,
            local_initiated_close,
            session_file,
        )?;
        self.wallets.borrow_mut()[self.index].bound_channels = sorted_bindings(session.state());
        record(
            &self.trace,
            format_args!(
                "{} opened external funding draft channel={channel_id}",
                WALLET_HOSTS[self.index]
            ),
        );

        let token = session
            .tenant_token()
            .context("wallet not registered")?
            .to_owned();
        let unsigned_tx: Transaction = result.unsigned_funding_tx.into();
        let no_sdk_fault = Cell::new(None);
        for _ in 0..400 {
            let _ = self
                .poll_channel(
                    session,
                    store,
                    store_file,
                    pending_outbound,
                    local_initiated_close,
                    session_file,
                    rpc,
                    &token,
                    channel_id,
                    &no_sdk_fault,
                )
                .await;
            let signer = session
                .channel_signer(channel_id)
                .await
                .map_err(|err| anyhow!("open bound channel signer: {err}"))?;
            let records = signer
                .recovery_records()
                .await
                .map_err(|err| anyhow!("read recovery records: {err}"))?;
            let local_ready = records
                .iter()
                .any(|r| !r.reference.for_remote && r.complete_signature.is_some());
            let remote_ready = records.iter().any(|r| r.reference.for_remote);
            if local_ready && remote_ready {
                let status: GetChannelSigningStatusResult = rpc
                    .call(
                        Some(&token),
                        "get_channel_signing_status",
                        json!([GetChannelSigningStatusParams {
                            channel_id: channel_id.into(),
                        }]),
                    )
                    .await?;
                if matches!(status.status, ChannelSigningStatus::NoSignatureRequired) {
                    let signed_tx = unsigned_tx
                        .as_advanced_builder()
                        .set_witnesses(vec![Bytes::default()])
                        .build()
                        .data();
                    for submit_attempt in 0..4 {
                        match rpc
                            .call::<SubmitSignedFundingTxResult>(
                                Some(&token),
                                "submit_signed_funding_tx",
                                json!([SubmitSignedFundingTxParams {
                                    channel_id: channel_id.into(),
                                    signed_funding_tx: signed_tx.clone().into(),
                                }]),
                            )
                            .await
                        {
                            Ok(_) => break,
                            Err(err)
                                if submit_attempt < 3
                                    && (err.to_string().contains("simulated dropped")
                                        || err.to_string().contains("timeout")) =>
                            {
                                rpc.control.flush_events(&self.trace);
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    rpc.control.flush_events(&self.trace);
                    record(
                        &self.trace,
                        format_args!(
                            "{} submitted signed funding tx channel={channel_id}",
                            WALLET_HOSTS[self.index]
                        ),
                    );
                    return Ok(channel_id);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        bail!("timed out completing opening commitments for channel {channel_id}")
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_all_channels(
        &self,
        session: &mut HostedSession<MemoryStore>,
        store: &MemoryStore,
        store_file: &Path,
        pending_outbound: &mut BTreeMap<Hash256, Hash256>,
        local_initiated_close: &mut BTreeSet<Hash256>,
        session_file: &Path,
        rpc: &LspRpcClient,
        wallet_lock: &Script,
        sdk_fault: &Cell<Option<SdkSignerFault>>,
    ) -> Result<bool> {
        let res = self
            .poll_all_channels_inner(
                session,
                store,
                store_file,
                pending_outbound,
                local_initiated_close,
                session_file,
                rpc,
                wallet_lock,
                sdk_fault,
            )
            .await;
        rpc.control.flush_events(&self.trace);
        res
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_all_channels_inner(
        &self,
        session: &mut HostedSession<MemoryStore>,
        store: &MemoryStore,
        store_file: &Path,
        pending_outbound: &mut BTreeMap<Hash256, Hash256>,
        local_initiated_close: &mut BTreeSet<Hash256>,
        session_file: &Path,
        rpc: &LspRpcClient,
        wallet_lock: &Script,
        sdk_fault: &Cell<Option<SdkSignerFault>>,
    ) -> Result<bool> {
        let Some(mut token) = session.tenant_token().map(str::to_owned) else {
            return Ok(false);
        };
        let channels = sorted_bindings(session.state());
        for channel_id in channels {
            match self
                .poll_channel(
                    session,
                    store,
                    store_file,
                    pending_outbound,
                    local_initiated_close,
                    session_file,
                    rpc,
                    &token,
                    channel_id,
                    sdk_fault,
                )
                .await
            {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(err) if is_auth_rejection(&err) => {
                    record(
                        &self.trace,
                        format_args!(
                            "{} token rejected, refreshing via lsp_register_tenant",
                            WALLET_HOSTS[self.index]
                        ),
                    );
                    if self
                        .register_tenant(
                            session,
                            store,
                            store_file,
                            pending_outbound,
                            local_initiated_close,
                            session_file,
                            rpc,
                            true,
                        )
                        .await
                        .is_ok()
                    {
                        if let Some(fresh) = session.tenant_token() {
                            token = fresh.to_owned();
                        }
                    }
                }
                Err(err) => {
                    if err.to_string().contains("SDK ") {
                        record(
                            &self.trace,
                            format_args!("{} poll_channel error: {err}", WALLET_HOSTS[self.index]),
                        );
                    }
                }
            }
            if let Err(err) = self
                .poll_watchtower(
                    session,
                    store,
                    store_file,
                    pending_outbound,
                    local_initiated_close,
                    session_file,
                    rpc,
                    &token,
                    channel_id,
                    wallet_lock,
                )
                .await
            {
                if is_auth_rejection(&err) {
                    record(
                        &self.trace,
                        format_args!(
                            "{} watchtower token rejected, refreshing via lsp_register_tenant",
                            WALLET_HOSTS[self.index]
                        ),
                    );
                    let _ = self
                        .register_tenant(
                            session,
                            store,
                            store_file,
                            pending_outbound,
                            local_initiated_close,
                            session_file,
                            rpc,
                            true,
                        )
                        .await;
                    continue;
                }
                let msg = err.to_string();
                if !msg.contains("timeout")
                    && !msg.contains("simulated dropped")
                    && !msg.contains("Connection refused")
                    && !msg.contains("Broken pipe")
                    && !msg.contains("reset by peer")
                {
                    record(
                        &self.trace,
                        format_args!("{} poll_watchtower error: {err}", WALLET_HOSTS[self.index]),
                    );
                }
            }
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_channel(
        &self,
        session: &mut HostedSession<MemoryStore>,
        store: &MemoryStore,
        store_file: &Path,
        pending_outbound: &mut BTreeMap<Hash256, Hash256>,
        local_initiated_close: &BTreeSet<Hash256>,
        session_file: &Path,
        rpc: &LspRpcClient,
        token: &str,
        channel_id: Hash256,
        sdk_fault: &Cell<Option<SdkSignerFault>>,
    ) -> Result<bool> {
        // Query RPC evidence for any pending outbound payments before signing
        // TLC removals (matching production external-signer wallet preimage verification).
        let pending_hashes: Vec<Hash256> = pending_outbound.keys().copied().collect();
        if !pending_hashes.is_empty() {
            let mut updated = false;
            for payment_hash in pending_hashes {
                let Ok(payment) = rpc
                    .call::<GetPaymentCommandResult>(
                        Some(token),
                        "get_payment",
                        json!([GetPaymentCommandParams {
                            payment_hash: payment_hash.into(),
                            tenant_id: None,
                        }]),
                    )
                    .await
                else {
                    continue;
                };
                if let Some(preimage) = payment.payment_preimage {
                    for bound_channel in sorted_bindings(session.state()) {
                        let signer = session
                            .channel_signer(bound_channel)
                            .await
                            .map_err(|err| anyhow!("SDK channel_signer({bound_channel}): {err}"))?;
                        match signer
                            .authorize_fulfillment(payment_hash, preimage.into())
                            .await
                        {
                            Ok(()) => {}
                            Err(err) if err.to_string().contains("unknown payment") => continue,
                            Err(err) => {
                                return Err(anyhow!(
                                    "SDK authorize_fulfillment({payment_hash}): {err}"
                                ))
                            }
                        }
                    }
                    pending_outbound.remove(&payment_hash);
                    updated = true;
                } else if payment.status == JsonPaymentStatus::Failed {
                    for bound_channel in sorted_bindings(session.state()) {
                        let signer = session
                            .channel_signer(bound_channel)
                            .await
                            .map_err(|err| anyhow!("SDK channel_signer({bound_channel}): {err}"))?;
                        match signer.authorize_cancellation(payment_hash).await {
                            Ok(()) => {}
                            Err(err) if err.to_string().contains("unknown payment") => continue,
                            Err(err) => {
                                return Err(anyhow!(
                                    "SDK authorize_cancellation({payment_hash}): {err}"
                                ))
                            }
                        }
                    }
                    pending_outbound.remove(&payment_hash);
                    updated = true;
                }
            }
            if updated {
                persist_wallet_files(
                    store,
                    store_file,
                    session.state(),
                    pending_outbound,
                    local_initiated_close,
                    session_file,
                )?;
            }
        }

        let result: GetChannelSigningStatusResult = rpc
            .call(
                Some(token),
                "get_channel_signing_status",
                json!([GetChannelSigningStatusParams {
                    channel_id: channel_id.into(),
                }]),
            )
            .await?;

        let is_closing = matches!(
            &result.status,
            ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendClosingSigned,
                ..
            }
        );
        if let ChannelSigningStatus::SignatureRequired {
            transition: ChannelSigningTransition::SendClosingSigned,
            content,
            ..
        } = &result.status
        {
            if let Ok(m) = fiber_lsp_sdk::json::musig2_from_rpc(content.clone()) {
                if let fnn::fiber_types::Musig2SignableContent::CooperativeCloseTransaction(tx) =
                    m.content
                {
                    let funding_outpoint = tx
                        .raw()
                        .inputs()
                        .get(0)
                        .context("closing tx missing funding input")?
                        .previous_output();
                    if let Some((funding_cell, _)) = self.chain.live_cell(&funding_outpoint) {
                        let input_cap: u64 = funding_cell.capacity().unpack();
                        let output_cap: u64 = tx
                            .raw()
                            .outputs()
                            .into_iter()
                            .map(|o| Unpack::<u64>::unpack(&o.capacity()))
                            .sum();
                        if let Some(fee) = input_cap.checked_sub(output_cap) {
                            let local_initiated = local_initiated_close.contains(&channel_id);
                            let terms = if local_initiated {
                                CloseAuthorization {
                                    fee,
                                    local_fee_share: fee,
                                    remote_fee_share: 0,
                                }
                            } else {
                                CloseAuthorization {
                                    fee,
                                    local_fee_share: 0,
                                    remote_fee_share: fee,
                                }
                            };
                            let signer = session
                                .channel_signer(channel_id)
                                .await
                                .map_err(|err| anyhow!("SDK channel_signer close: {err}"))?;
                            signer
                                .authorize_close(terms)
                                .await
                                .map_err(|err| anyhow!("SDK authorize_close: {err}"))?;
                            persist_wallet_files(
                                store,
                                store_file,
                                session.state(),
                                pending_outbound,
                                local_initiated_close,
                                session_file,
                            )?;
                        }
                    }
                }
            }
        }

        let now_ms = fnn::now_timestamp_as_millis_u64();
        let outcome = session
            .handle_channel_status(channel_id, result.status, now_ms)
            .await
            .map_err(|err| anyhow!("SDK handle_channel_status({channel_id}): {err}"))?;
        if matches!(
            outcome,
            ProcessOutcome::ReadyToSubmit(SubmitParams::Channel(_))
        ) && sdk_fault.get() == Some(SdkSignerFault::SkipPersistAfterConfirmCrash)
        {
            sdk_fault.set(None);
            self.wallets.borrow_mut()[self.index].sdk_faults_hit += 1;
            record(
                &self.trace,
                format_args!(
                    "{} hit SdkSignerFault::SkipPersistAfterConfirmCrash channel={channel_id}",
                    WALLET_HOSTS[self.index]
                ),
            );
            return Ok(true);
        }
        // Persist SDK nonces & signing state immediately before transmitting any signature.
        persist_wallet_files(
            store,
            store_file,
            session.state(),
            pending_outbound,
            local_initiated_close,
            session_file,
        )?;
        match outcome {
            ProcessOutcome::Idle => Ok(false),
            ProcessOutcome::ReadyToSubmit(SubmitParams::Channel(params)) => {
                if let Some(SdkSignerFault::CrashAfterPersistBeforeSubmit) = sdk_fault.get() {
                    sdk_fault.set(None);
                    self.wallets.borrow_mut()[self.index].sdk_faults_hit += 1;
                    record(
                        &self.trace,
                        format_args!(
                            "{} hit SdkSignerFault::CrashAfterPersistBeforeSubmit channel={channel_id}",
                            WALLET_HOSTS[self.index]
                        ),
                    );
                    return Ok(true);
                }
                self.wallets.borrow_mut()[self.index].last_channel_sig = Some(params.clone());
                let res: SubmitChannelSignatureResult = rpc
                    .call(Some(token), "submit_channel_signature", json!([params]))
                    .await?;
                let mut wallets = self.wallets.borrow_mut();
                match res {
                    SubmitChannelSignatureResult::Applied => {
                        wallets[self.index].channel_sigs_applied += 1;
                        if is_closing {
                            wallets[self.index].closing_sigs_applied += 1;
                        }
                    }
                    SubmitChannelSignatureResult::AlreadyApplied => {
                        wallets[self.index].channel_sigs_already_applied += 1;
                    }
                }
                record(
                    &self.trace,
                    format_args!(
                        "{} submit_channel_signature channel={channel_id} -> {res:?}",
                        WALLET_HOSTS[self.index]
                    ),
                );
                Ok(false)
            }
            ProcessOutcome::NeedConfirmation(pending) => {
                let SubmitParams::Channel(params) = session
                    .confirm(pending, now_ms)
                    .await
                    .map_err(|err| anyhow!("SDK confirm({channel_id}): {err}"))?
                else {
                    bail!("expected channel submit params");
                };
                if let Some(fault) = sdk_fault.take() {
                    self.wallets.borrow_mut()[self.index].sdk_faults_hit += 1;
                    match fault {
                        SdkSignerFault::SkipPersistAfterConfirmCrash => {
                            record(
                                &self.trace,
                                format_args!(
                                    "{} hit SdkSignerFault::SkipPersistAfterConfirmCrash channel={channel_id}",
                                    WALLET_HOSTS[self.index]
                                ),
                            );
                            return Ok(true);
                        }
                        SdkSignerFault::CrashAfterPersistBeforeSubmit => {
                            persist_wallet_files(
                                store,
                                store_file,
                                session.state(),
                                pending_outbound,
                                local_initiated_close,
                                session_file,
                            )?;
                            record(
                                &self.trace,
                                format_args!(
                                    "{} hit SdkSignerFault::CrashAfterPersistBeforeSubmit channel={channel_id}",
                                    WALLET_HOSTS[self.index]
                                ),
                            );
                            return Ok(true);
                        }
                    }
                }
                persist_wallet_files(
                    store,
                    store_file,
                    session.state(),
                    pending_outbound,
                    local_initiated_close,
                    session_file,
                )?;
                self.wallets.borrow_mut()[self.index].last_channel_sig = Some(params.clone());
                let res: SubmitChannelSignatureResult = rpc
                    .call(Some(token), "submit_channel_signature", json!([params]))
                    .await?;
                let mut wallets = self.wallets.borrow_mut();
                match res {
                    SubmitChannelSignatureResult::Applied => {
                        wallets[self.index].channel_sigs_applied += 1;
                        if is_closing {
                            wallets[self.index].closing_sigs_applied += 1;
                        }
                    }
                    SubmitChannelSignatureResult::AlreadyApplied => {
                        wallets[self.index].channel_sigs_already_applied += 1;
                    }
                }
                Ok(false)
            }
            ProcessOutcome::Denied => bail!("SDK denied channel signing request"),
            ProcessOutcome::ReadyToSubmit(SubmitParams::Watchtower(_)) => {
                bail!("unexpected watchtower params for channel status")
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_watchtower(
        &self,
        session: &mut HostedSession<MemoryStore>,
        store: &MemoryStore,
        store_file: &Path,
        pending_outbound: &BTreeMap<Hash256, Hash256>,
        local_initiated_close: &BTreeSet<Hash256>,
        session_file: &Path,
        rpc: &LspRpcClient,
        token: &str,
        channel_id: Hash256,
        wallet_lock: &Script,
    ) -> Result<()> {
        let result: GetWatchtowerSigningStatusResult = match rpc
            .call(
                Some(token),
                "get_watchtower_signing_status",
                json!([GetWatchtowerSigningStatusParams {
                    channel_id: channel_id.into(),
                }]),
            )
            .await
        {
            Ok(result) => result,
            Err(err) if err.to_string().contains("watched channel not found") => return Ok(()),
            Err(err) => return Err(err),
        };
        match &result.status {
            WatchtowerSigningStatus::NoSignatureRequired => return Ok(()),
            WatchtowerSigningStatus::Backoff { .. } => {
                self.wallets.borrow_mut()[self.index].watchtower_backoff_hits += 1;
                return Ok(());
            }
            WatchtowerSigningStatus::ExternalFundingRequired { .. } => {}
        }
        let WatchtowerSigningStatus::ExternalFundingRequired { content, .. } = &result.status
        else {
            return Ok(());
        };
        let template_tx: Transaction = content.transaction.clone().into();
        let Some((funded_tx, fee)) =
            fund_external_funding_template(&template_tx, wallet_lock.clone(), WATCHTOWER_MAX_FEE)?
        else {
            return Ok(());
        };
        let verifier = SimChainVerifier {
            chain: self.chain.clone(),
        };
        let first_outpoint = funded_tx
            .raw()
            .inputs()
            .get(0)
            .context("missing commitment input")?
            .previous_output();
        let signer = session
            .channel_signer(channel_id)
            .await
            .map_err(|err| anyhow!("open channel signer for watchtower: {err}"))?;
        let mut source = None;
        let records = signer
            .recovery_records()
            .await
            .map_err(|err| anyhow!("load recovery records: {err}"))?;
        for record in &records {
            if verifier
                .verify_commitment_lineage(record.reference.tx_hash, &first_outpoint)
                .await
                .is_ok()
            {
                source = Some(record.reference.clone());
                break;
            }
        }
        let source = source.ok_or_else(|| {
            let known: Vec<_> = records
                .iter()
                .map(|r| {
                    format!(
                        "(remote={},v={},tx={})",
                        r.reference.for_remote, r.reference.version, r.reference.tx_hash
                    )
                })
                .collect();
            anyhow!(
                "watchtower spend outpoint={} has no verified commitment ancestor among [{}]",
                Hash256::from(first_outpoint.tx_hash()),
                known.join(", ")
            )
        })?;
        let cell_deps = self.chain.settlement_cell_deps().await;
        let auth = OnchainSpendAuthorization {
            source,
            destination: wallet_lock.clone(),
            fee_sponsor_lock: None,
            fee,
            additional_inputs: Vec::new(),
            cell_deps,
        };
        let now_ms = fnn::now_timestamp_as_millis_u64();
        let mut handler = NoopWatchtowerEventHandler;
        let (_health, outcome) = session
            .handle_watchtower_status_result(
                result,
                now_ms,
                Some((funded_tx.into(), auth)),
                &verifier,
                &mut handler,
            )
            .await
            .map_err(|err| anyhow!("SDK handle_watchtower_status_result: {err}"))?;
        persist_wallet_files(
            store,
            store_file,
            session.state(),
            pending_outbound,
            local_initiated_close,
            session_file,
        )?;
        let params: SubmitWatchtowerSignatureParams = match outcome {
            ProcessOutcome::ReadyToSubmit(SubmitParams::Watchtower(params)) => params,
            ProcessOutcome::NeedConfirmation(pending) => {
                let SubmitParams::Watchtower(params) = session
                    .confirm_external_funding_onchain(pending, &verifier, now_ms)
                    .await
                    .map_err(|err| anyhow!("SDK confirm_external_funding_onchain: {err}"))?
                else {
                    bail!("expected watchtower submit params");
                };
                persist_wallet_files(
                    store,
                    store_file,
                    session.state(),
                    pending_outbound,
                    local_initiated_close,
                    session_file,
                )?;
                params
            }
            ProcessOutcome::Idle => return Ok(()),
            other => bail!("unexpected watchtower signing outcome: {other:?}"),
        };
        let res: SubmitWatchtowerSignatureResult = rpc
            .call(Some(token), "submit_watchtower_signature", json!([params]))
            .await?;
        let mut wallets = self.wallets.borrow_mut();
        match res {
            SubmitWatchtowerSignatureResult::Applied => {
                wallets[self.index].watchtower_sigs_applied += 1;
            }
            SubmitWatchtowerSignatureResult::AlreadyApplied => {
                wallets[self.index].watchtower_sigs_already_applied += 1;
            }
        }
        record(
            &self.trace,
            format_args!(
                "{} submit_watchtower_signature channel={channel_id} -> {res:?}",
                WALLET_HOSTS[self.index]
            ),
        );
        Ok(())
    }
}

fn is_auth_rejection(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("unauthorized")
        || msg.contains("revoked")
        || msg.contains("biscuit")
        || msg.contains("invalid token")
        || msg.contains("forbidden")
        || msg.contains("authentication")
}

fn sorted_bindings(state: &HostedSessionState) -> Vec<Hash256> {
    let mut channels: Vec<_> = state.bindings.keys().copied().collect();
    channels.sort();
    channels
}

fn fund_external_funding_template(
    template: &Transaction,
    destination: Script,
    max_fee: u64,
) -> Result<Option<(Transaction, u64)>> {
    let mut outputs: Vec<_> = template.raw().outputs().into_iter().collect();
    let payout_idx = outputs
        .len()
        .checked_sub(1)
        .context("external-funding template missing payout output")?;
    let payout_capacity: u64 = outputs[payout_idx].capacity().unpack();
    outputs[payout_idx] = outputs[payout_idx]
        .clone()
        .as_builder()
        .lock(destination)
        .build();
    let with_destination = template
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(outputs.clone())
        .build();
    let fee = (with_destination.data().serialized_size_in_block() as u64).max(1_000);
    if fee > max_fee {
        bail!("watchtower fee {fee} exceeds max_fee {max_fee}");
    }
    let Some(adjusted_capacity) = payout_capacity.checked_sub(fee) else {
        return Ok(None);
    };
    let payout_data_len = template
        .raw()
        .outputs_data()
        .get(payout_idx)
        .map(|d| d.raw_data().len())
        .unwrap_or(0);
    let min_capacity = outputs[payout_idx]
        .occupied_capacity(ckb_types::core::Capacity::bytes(payout_data_len)?)?
        .as_u64();
    if adjusted_capacity < min_capacity {
        return Ok(None);
    }
    outputs[payout_idx] = outputs[payout_idx]
        .clone()
        .as_builder()
        .capacity(adjusted_capacity)
        .build();
    let funded = with_destination
        .as_advanced_builder()
        .set_outputs(outputs)
        .build()
        .data();
    Ok(Some((funded, fee)))
}

async fn write_message(socket: &mut TcpStream, value: &str) -> io::Result<()> {
    if value.len() > MAX_RPC_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized LSP RPC message",
        ));
    }
    socket.write_u32(value.len() as u32).await?;
    socket.write_all(value.as_bytes()).await
}

async fn read_message(socket: &mut TcpStream) -> io::Result<String> {
    let length = socket.read_u32().await? as usize;
    if length > MAX_RPC_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized LSP RPC message",
        ));
    }
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).await?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
