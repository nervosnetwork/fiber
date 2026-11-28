use anyhow::anyhow;

use ckb_sdk::tx_builder::TxBuilderError;
use ckb_testtool::context::Context;
use ckb_types::bytes::BufMut;
use ckb_types::{
    bytes::Bytes,
    core::{tx_pool::TxStatus, Capacity, DepType, TransactionView},
    packed::{self, CellDep, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Pack, PackVec, Unpack},
    H256,
};
use molecule::bytes::BytesMut;
use once_cell::sync::{Lazy, OnceCell};

use crate::ckb::client::CkbChainClient;
use std::{collections::HashMap, sync::Arc, sync::RwLock};
use tokio::sync::RwLock as TokioRwLock;

use crate::{
    ckb::{
        contracts::{get_cell_deps, Contract, ContractsContext, ContractsInfo, ScriptCellDep},
        CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult, FundingError, GetTxResponse,
    },
    now_timestamp_as_millis_u64,
};
use fiber_types::{Hash256, UdtCfgInfos};
use fiber_types::{UdtArgInfo, UdtScript};

use crate::ckb::CkbChainMessage;

use ractor::{
    call_t, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, OutputPort, RpcReplyPort,
    SupervisionEvent,
};
use tracing::{debug, error};

pub const TRACE_TX_WAITING_FOR_NOTIFICATION_MS: u64 = 2 * 1000;
pub const TRACE_TX_TIMEOUT_MS: u64 = 3 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    // This cell has been consumed. If any transaction
    // tries to consume the same cell, it should be rejected.
    Consumed,
}

// The problem of channel announcement is that each nodes will query the block timestamp
// and use it as the channel announcement timestamp.
// Guaranteeing the block timestamp is the same across all nodes is important
// because if a node A has a greater channel announcement timestamp than node B, then when
// A tries to get broadcast messages after this channel announcement timestamp, B will return
// the channel announcement. But for A, it is not a later broadcast message. This process will
// cause an infinite loop.
// So here we create an static block timestamp context which is shared across all nodes,
// and we use this context to guarantee that the block timestamp is the same across all nodes.
pub static BLOCK_TIMESTAMP_CONTEXT: OnceCell<TokioRwLock<BlockTimestampContext>> = OnceCell::new();

#[derive(Default, Clone, Debug)]
pub struct BlockTimestampContext {
    pub timestamps: HashMap<H256, u64>,
    // If non empty, we will use this as the timestamp for the next block.
    // This is normally used to mock an ancient block.
    pub next_timestamp: Option<u64>,
}

pub async fn get_block_timestamp(block: H256) -> u64 {
    let context = BLOCK_TIMESTAMP_CONTEXT.get_or_init(|| TokioRwLock::new(Default::default()));
    let mut context = context.write().await;
    context.get_block_timestamp(block)
}

pub async fn set_next_block_timestamp(next_timestamp: u64) {
    let context = BLOCK_TIMESTAMP_CONTEXT.get_or_init(|| TokioRwLock::new(Default::default()));
    let mut context = context.write().await;
    context.set_next_block_timestamp(next_timestamp);
}

impl BlockTimestampContext {
    fn get_block_timestamp(&mut self, block: H256) -> u64 {
        if let Some(timestamp) = self.timestamps.get(&block) {
            return *timestamp;
        }
        let timestamp = self
            .next_timestamp
            .take()
            .unwrap_or(now_timestamp_as_millis_u64());
        self.timestamps.insert(block, timestamp);
        return timestamp;
    }

    fn set_next_block_timestamp(&mut self, next_timestamp: u64) {
        self.next_timestamp = Some(next_timestamp);
    }
}

pub static MOCK_CONTEXT: Lazy<RwLock<MockContext>> = Lazy::new(|| RwLock::new(MockContext::new()));

pub struct MockContext {
    pub context: Context,
    pub contracts_context: ContractsContext,
}

impl Default for MockContext {
    fn default() -> Self {
        Self::new()
    }
}

impl MockContext {
    pub fn new() -> Self {
        let binaries = [
            (
                Contract::CkbAuth,
                Bytes::from_static(include_bytes!("../../../../../tests/deploy/contracts/auth")),
            ),
            (
                Contract::FundingLock,
                Bytes::from_static(include_bytes!(
                    "../../../../../tests/deploy/contracts/funding-lock"
                )),
            ),
            (
                Contract::CommitmentLock,
                Bytes::from_static(include_bytes!(
                    "../../../../../tests/deploy/contracts/commitment-lock"
                )),
            ),
            // mock secp256k1 lock script
            (
                Contract::Secp256k1Lock,
                Bytes::from_static(include_bytes!(
                    "../../../../../tests/deploy/contracts/always_success"
                )),
            ),
            (
                Contract::SimpleUDT,
                Bytes::from_static(include_bytes!(
                    "../../../../../tests/deploy/contracts/simple_udt"
                )),
            ),
        ];
        let mut context = Context::new_with_deterministic_rng();
        let mut contract_default_scripts: HashMap<Contract, Script> = HashMap::new();
        let mut script_cell_deps: HashMap<Contract, Vec<ScriptCellDep>> = HashMap::new();

        for (contract, binary) in binaries.into_iter() {
            let out_point = context.deploy_cell(binary);
            let script = context
                .build_script(&out_point, Default::default())
                .expect("valid script");
            contract_default_scripts.insert(contract, script);
            let cell_dep = CellDep::new_builder()
                .out_point(out_point)
                .dep_type(DepType::Code)
                .build();

            let cell_deps = if matches!(contract, Contract::FundingLock)
                || matches!(contract, Contract::CommitmentLock)
            {
                // FundingLock and CommitmentLock depend on CkbAuth
                vec![
                    cell_dep.into(),
                    script_cell_deps
                        .get(&Contract::CkbAuth)
                        .unwrap()
                        .clone()
                        .first()
                        .unwrap()
                        .clone(),
                ]
            } else {
                vec![cell_dep.into()]
            };
            script_cell_deps.insert(contract, cell_deps);
        }

        let mock_udt_infos = UdtCfgInfos(vec![UdtArgInfo {
            name: "Mock UDT".to_string(),
            script: UdtScript::default(),
            auto_accept_amount: None,
            cell_deps: vec![],
        }]);

        let contracts = ContractsInfo {
            contract_default_scripts,
            script_cell_deps,
            udt_whitelist: mock_udt_infos,
        };
        let contracts_context = ContractsContext {
            contracts,
            type_id_resolver: None,
        };
        MockContext {
            context,
            contracts_context,
        }
    }
}

// A simple actor to wait for the tx notifications from mock chain actor,
// Sometimes we are sending SendTx to the mock chain actor after a TraceTx request.
// In this case, we don't want to prematurely tell the caller that the transaction
// does not exists. So we use this actor to wait for the tx notifications from the
// mock chain actor.
struct TraceTxReplier {
    tx_hash: Hash256,
    mask: CkbTxTracingMask,
    watched_inputs: Vec<OutPoint>,
}

impl TraceTxReplier {
    pub fn new(tx_hash: Hash256, mask: CkbTxTracingMask, watched_inputs: Vec<OutPoint>) -> Self {
        Self {
            tx_hash,
            mask,
            watched_inputs,
        }
    }
}

#[derive(Clone, Debug)]
pub enum MockTxNotification {
    Tx(CkbTxTracingResult),
    ConsumedInputs {
        spending_tx_hash: Hash256,
        inputs: Vec<OutPoint>,
    },
}

#[async_trait::async_trait]
impl Actor for TraceTxReplier {
    type Msg = CkbTxTracingResult;
    type Arguments = (
        Arc<OutputPort<MockTxNotification>>,
        Option<Duration>,
        RpcReplyPort<CkbTxTracingResult>,
    );
    type State = Option<RpcReplyPort<CkbTxTracingResult>>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (notifier, timeout, reply_port): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        if let Some(timeout) = timeout {
            let tx_hash = self.tx_hash;
            myself.send_after(timeout, move || CkbTxTracingResult::unknown(tx_hash));
        }

        let tx_hash = self.tx_hash;
        let mask = self.mask;
        let watched_inputs = self.watched_inputs.clone();
        notifier.subscribe(myself, move |notification| match notification {
            MockTxNotification::Tx(tx_notification) => {
                if tx_notification.tx_hash == tx_hash
                    && mask.contains((&tx_notification.tx_status).into())
                {
                    Some(tx_notification)
                } else {
                    None
                }
            }
            MockTxNotification::ConsumedInputs {
                spending_tx_hash,
                inputs,
            } => {
                if spending_tx_hash != tx_hash
                    && (mask.contains(CkbTxTracingMask::Rejected)
                        || mask.contains(CkbTxTracingMask::WatchedInputSpent))
                    && watched_inputs.iter().any(|w| inputs.contains(w))
                {
                    Some(CkbTxTracingResult::watched_input_spent(tx_hash))
                } else {
                    None
                }
            }
        });
        Ok(Some(reply_port))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        reply_port: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        crate::simulation::before_actor_message(myself.get_id()).await;
        if let Some(reply_port) = reply_port.take() {
            let _ = reply_port.send(message);
        }
        myself.stop(Some("handled trace tx result".to_string()));
        Ok(())
    }
}

#[async_trait::async_trait]
pub trait MockChainActorMiddleware: Send + std::fmt::Debug {
    /// Returns Ok(None) if the message is handled by the middleware, otherwise the message
    /// will be forwarded to the underlying MockChainActor.
    async fn handle(
        &mut self,
        inner_self: ActorRef<CkbChainMessage>,
        message: CkbChainMessage,
        state: &mut MockChainActorState,
    ) -> Result<Option<CkbChainMessage>, ActorProcessingErr>;

    // Trick to make `Box<dyn MockChainActorMiddleware>` cloneable
    fn clone_box(&self) -> Box<dyn MockChainActorMiddleware>;
}

/// Minimal simulation chain driver seam injected by `fiber-dst` into `MockChainState`.
#[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
pub trait SimulatedChainDriver: Send + Sync + std::fmt::Debug {
    /// Whether settlement clock validation and simulated block headers are active.
    fn is_active(&self) -> bool;
    /// Validate inputs, relative/absolute `since` maturity, and capacity conservation.
    fn validate_spend(&self, state: &MockChainState, tx: &TransactionView) -> anyhow::Result<()>;
    /// Current simulated block header derived from the virtual clock.
    fn current_tip(&self, state: &MockChainState) -> ckb_types::core::HeaderView;
    /// Query live cells from `MockChainState` using CKB indexer semantics.
    fn query_cells(
        &self,
        state: &MockChainState,
        key: &ckb_sdk::rpc::ckb_indexer::SearchKey,
        order: ckb_sdk::rpc::ckb_indexer::Order,
        limit: u32,
        after: Option<ckb_jsonrpc_types::JsonBytes>,
    ) -> ckb_sdk::rpc::ckb_indexer::Pagination<ckb_sdk::rpc::ckb_indexer::Cell>;
}

#[derive(Clone, Debug)]
pub struct MockChainState {
    pub txs: HashMap<Hash256, GetTxResponse>,
    pub tx_tracing_tasks: HashMap<Hash256, Vec<ActorRef<CkbTxTracingResult>>>,
    pub tx_notifications: Arc<OutputPort<MockTxNotification>>,
    pub cell_status: HashMap<OutPoint, CellStatus>,
    pub indexer_cells: Vec<ckb_sdk::rpc::ckb_indexer::Cell>,
    #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
    simulation_timestamps: BlockTimestampContext,
    #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
    pub driver: Option<Arc<dyn SimulatedChainDriver>>,
}

impl Default for MockChainState {
    fn default() -> Self {
        Self::new()
    }
}

impl MockChainState {
    pub fn new() -> Self {
        Self {
            txs: HashMap::new(),
            tx_tracing_tasks: HashMap::new(),
            tx_notifications: Arc::new(OutputPort::default()),
            cell_status: HashMap::new(),
            indexer_cells: Vec::new(),
            #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
            simulation_timestamps: BlockTimestampContext::default(),
            #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
            driver: None,
        }
    }

    pub fn submit_transaction(&mut self, tx: TransactionView) -> Result<(), ckb_sdk::RpcError> {
        const MAX_CYCLES: u64 = 100_000_000;
        let hash = tx.hash().into();
        if self
            .txs
            .get(&hash)
            .is_some_and(|record| matches!(record.tx_status, TxStatus::Committed(..)))
        {
            return Ok(());
        }

        let inputs: Vec<_> = tx.input_pts_iter().collect();
        let result: anyhow::Result<()> = (|| {
            let mut unique = std::collections::HashSet::new();
            for input in &inputs {
                anyhow::ensure!(
                    unique.insert(input)
                        && self.cell_status.get(input) != Some(&CellStatus::Consumed),
                    "Cell {input:?} already consumed",
                );
            }
            #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
            if let Some(driver) = self.driver.as_ref().filter(|driver| driver.is_active()) {
                driver.validate_spend(self, &tx)?;
            }
            let context = &mut MOCK_CONTEXT.write().unwrap().context;
            if !inputs.is_empty() {
                context
                    .verify_tx(&tx, MAX_CYCLES)
                    .map_err(|err| anyhow!("Failed to verify transaction: {err:?}"))?;
            }
            for (outpoint, (output, data)) in tx
                .output_pts()
                .into_iter()
                .zip(tx.outputs().into_iter().zip(tx.outputs_data()))
            {
                context.create_cell_with_out_point(outpoint, output, data.as_bytes());
            }
            for input in &inputs {
                self.cell_status.insert(input.clone(), CellStatus::Consumed);
            }
            Ok(())
        })();
        let status = match &result {
            Ok(()) => {
                #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
                if let Some(driver) = self.driver.as_ref().filter(|driver| driver.is_active()) {
                    let header = driver.current_tip(self);
                    let hash = H256::from(header.hash());
                    self.simulation_timestamps
                        .timestamps
                        .insert(hash.clone(), header.timestamp());
                    TxStatus::Committed(header.number(), hash, 0)
                } else {
                    TxStatus::Committed(0, H256::default(), 0)
                }
                #[cfg(not(all(feature = "simulation", not(target_arch = "wasm32"))))]
                TxStatus::Committed(0, H256::default(), 0)
            }
            Err(error) => TxStatus::Rejected(error.to_string()),
        };
        self.txs.insert(
            hash,
            GetTxResponse {
                transaction: Some(tx),
                tx_status: status.clone(),
            },
        );
        self.tx_notifications
            .send(MockTxNotification::Tx(CkbTxTracingResult::new(
                hash, status,
            )));
        if result.is_ok() && !inputs.is_empty() {
            self.tx_notifications
                .send(MockTxNotification::ConsumedInputs {
                    spending_tx_hash: hash,
                    inputs,
                });
        }
        result.map_err(ckb_sdk::RpcError::Other)
    }

    pub fn is_input_spent_by_other_committed_tx(
        &self,
        tx_hash: Hash256,
        watched_inputs: &[OutPoint],
    ) -> bool {
        if watched_inputs.is_empty() {
            return false;
        }
        self.txs.iter().any(|(other_hash, other_resp)| {
            *other_hash != tx_hash
                && matches!(other_resp.tx_status, TxStatus::Committed(..))
                && other_resp.transaction.as_ref().is_some_and(|tx| {
                    tx.input_pts_iter()
                        .any(|input| watched_inputs.contains(&input))
                })
        })
    }
}

pub struct MockChainActorState {
    pub shared: Arc<RwLock<MockChainState>>,
    pub middleware: Option<Box<dyn MockChainActorMiddleware>>,
}

impl Clone for Box<dyn MockChainActorMiddleware> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl Default for MockChainActorState {
    fn default() -> Self {
        Self::new(Arc::new(RwLock::new(MockChainState::new())), None)
    }
}

impl MockChainActorState {
    pub fn new(
        shared: Arc<RwLock<MockChainState>>,
        middleware: Option<Box<dyn MockChainActorMiddleware>>,
    ) -> Self {
        Self { shared, middleware }
    }
}

pub struct MockChainActor {}

impl Default for MockChainActor {
    fn default() -> Self {
        Self::new()
    }
}

impl MockChainActor {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn start_trace_tx_replier(
        &self,
        myself: ActorRef<CkbChainMessage>,
        tracer: CkbTxTracer,
        notifier: Arc<OutputPort<MockTxNotification>>,
        timeout: Option<Duration>,
    ) -> ActorRef<CkbTxTracingResult> {
        let tx_hash = tracer.tx_hash;
        let replier = TraceTxReplier::new(tx_hash, tracer.mask, tracer.watched_inputs);
        let (actor, _) = Actor::spawn_linked(
            None,
            replier,
            (notifier, timeout, tracer.callback),
            myself.get_cell(),
        )
        .await
        .expect("start trace tx replier");
        actor
    }
}
#[async_trait::async_trait]
impl Actor for MockChainActor {
    type Msg = CkbChainMessage;
    type State = MockChainActorState;
    type Arguments = (
        Option<Box<dyn MockChainActorMiddleware>>,
        Arc<RwLock<MockChainState>>,
    );
    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        (middleware, state): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Self::State::new(state, middleware))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        crate::simulation::before_actor_message(myself.get_id()).await;
        let message = if let Some(mut middleware) = state.middleware.take() {
            let handled = middleware.handle(myself.clone(), message, state).await;
            state.middleware = Some(middleware);
            match handled? {
                Some(message) => message,
                None => return Ok(()),
            }
        } else {
            message
        };
        use CkbChainMessage::*;
        match message {
            Fund(tx, request, reply_port) => {
                let mut fulfilled_tx = tx.clone();
                let outputs = fulfilled_tx
                    .as_ref()
                    .map(|x| x.outputs())
                    .unwrap_or_default();

                let mut ckb_amount = request.local_reserved_ckb_amount;

                let mut capacity =
                    request.local_amount + (request.local_reserved_ckb_amount as u128);
                if capacity > u64::MAX as u128 {
                    let _ = reply_port.send(Err(FundingError::CkbTxBuilderError(
                        TxBuilderError::Other(anyhow!("capacity overflow")),
                    )));
                    return Ok(());
                }

                let (outputs, outputs_data): (packed::CellOutputVec, packed::BytesVec) =
                    match outputs.get(0) {
                        Some(output) => {
                            if output.lock() != request.script {
                                error!(
                                    "funding request script ({:?}) does not match the first output lock script ({:?})", request.script, output.lock()
                                );
                                return Ok(());
                            }
                            ckb_amount = ckb_amount
                                .checked_add(request.remote_reserved_ckb_amount)
                                .expect("valid ckb amount");

                            if let Some(ref udt_script) = request.udt_type_script {
                                let udt_output = packed::CellOutput::new_builder()
                                    .capacity(Capacity::shannons(ckb_amount).pack())
                                    .type_(Some(udt_script.clone()).pack())
                                    .lock(request.script.clone())
                                    .build();

                                let mut outputs_builder = outputs.as_builder();
                                outputs_builder.replace(0, udt_output);
                                let outputs: packed::CellOutputVec = outputs_builder.build();

                                let udt_amount = request.local_amount + request.remote_amount;
                                let mut data = BytesMut::with_capacity(16);
                                data.put(&udt_amount.to_le_bytes()[..]);
                                let outputs_data: packed::BytesVec =
                                    vec![data.freeze().pack()].pack();

                                (outputs, outputs_data)
                            } else {
                                let current_capacity: u64 = output.capacity().unpack();
                                capacity += current_capacity as u128;
                                if capacity > u64::MAX as u128 {
                                    let _ = reply_port.send(Err(FundingError::CkbTxBuilderError(
                                        TxBuilderError::Other(anyhow!("capacity overflow")),
                                    )));
                                    return Ok(());
                                }

                                let mut outputs_builder = outputs.as_builder();
                                outputs_builder.replace(
                                    0,
                                    output.as_builder().capacity(capacity as u64).build(),
                                );
                                let outputs: packed::CellOutputVec = outputs_builder.build();

                                let outputs_data = fulfilled_tx
                                    .as_ref()
                                    .map(|x| x.outputs_data())
                                    .unwrap_or_default();
                                let outputs_data: packed::BytesVec = if outputs_data.is_empty() {
                                    [Default::default()].pack()
                                } else {
                                    outputs_data
                                };

                                (outputs, outputs_data)
                            }
                        }
                        None => {
                            if let Some(ref udt_script) = request.udt_type_script {
                                let outputs: packed::CellOutputVec = [CellOutput::new_builder()
                                    .capacity(
                                        Capacity::shannons(request.local_reserved_ckb_amount)
                                            .pack(),
                                    )
                                    .lock(request.script.clone())
                                    .type_(Some(udt_script.clone()).pack())
                                    .build()]
                                .pack();

                                let udt_amount = request.local_amount;
                                let mut data = BytesMut::with_capacity(16);
                                data.put(&udt_amount.to_le_bytes()[..]);
                                let outputs_data: packed::BytesVec =
                                    vec![data.freeze().pack()].pack();

                                (outputs, outputs_data)
                            } else {
                                let outputs: packed::CellOutputVec = [CellOutput::new_builder()
                                    .capacity(
                                        request.local_amount as u64
                                            + request.local_reserved_ckb_amount,
                                    )
                                    .lock(request.script.clone())
                                    .build()]
                                .pack();

                                let outputs_data: packed::BytesVec = [Default::default()].pack();

                                (outputs, outputs_data)
                            }
                        }
                    };

                let tx_builder = fulfilled_tx
                    .take()
                    .map(|x| x.as_advanced_builder())
                    .unwrap_or_default();

                fulfilled_tx.update_for_self(
                    tx_builder
                        .set_outputs(outputs.into_iter().collect())
                        .set_outputs_data(outputs_data.into_iter().collect())
                        .build(),
                );

                debug!(
                    "Fulfilling funding request: request: {:?}, original tx: {:?}, fulfilled tx: {:?}",
                    request, &tx, &fulfilled_tx
                );

                if let Err(e) = reply_port.send(Ok(fulfilled_tx)) {
                    error!(
                        "[{}] send reply failed: {:?}",
                        myself.get_name().unwrap_or_default(),
                        e
                    );
                }
            }
            VerifyFundingTx { reply, .. } => {
                let _ = reply.send(Ok(()));
            }
            GetFundingSourceLockScript(reply) => {
                let _ = reply.send(None);
            }
            AddFundingTx(_) | RemoveFundingTx(_) | CommitFundingTx(..) => {
                // ignore
            }

            Sign(tx, reply_port) => {
                // We don't need to sign the funding transaction in mock chain actor,
                // as any funding transaction is considered correct if we can successfully
                // run the scripts of transaction inputs, and we don't have inputs in the
                // funding transaction.
                let signed_tx = tx.clone();
                debug!(
                    "Signing transaction: original tx: {:?}, signed tx: {:?}",
                    &tx, &signed_tx
                );
                if let Err(e) = reply_port.send(Ok(signed_tx)) {
                    error!(
                        "[{}] send reply failed: {:?}",
                        myself.get_name().unwrap_or_default(),
                        e
                    );
                }
            }
            SendTx(tx, reply_port) => {
                let result = state.shared.write().unwrap().submit_transaction(tx);
                let _ = reply_port.send(result);
            }

            CreateTxTracer(tracer) => {
                debug!("Tracing transaction: {:?}", &tracer);
                let (maybe_tx, input_already_spent, notifications) = {
                    let guard = state.shared.read().unwrap();
                    let maybe_tx = guard.txs.get(&tracer.tx_hash).cloned();
                    let accepts_input_spent = tracer.mask.contains(CkbTxTracingMask::Rejected)
                        || tracer.mask.contains(CkbTxTracingMask::WatchedInputSpent);
                    let input_already_spent = accepts_input_spent
                        && maybe_tx.as_ref().is_none_or(|tx| {
                            matches!(tx.tx_status, TxStatus::Unknown | TxStatus::Rejected(_))
                        })
                        && guard.is_input_spent_by_other_committed_tx(
                            tracer.tx_hash,
                            &tracer.watched_inputs,
                        );
                    (
                        maybe_tx,
                        input_already_spent,
                        guard.tx_notifications.clone(),
                    )
                };

                match maybe_tx {
                    _ if input_already_spent => {
                        let _ = tracer
                            .callback
                            .send(CkbTxTracingResult::watched_input_spent(tracer.tx_hash));
                    }
                    Some(tx) if tracer.mask.contains((&tx.tx_status).into()) => {
                        let _ = tracer.callback.send(CkbTxTracingResult::new(
                            tracer.tx_hash,
                            tx.tx_status.clone(),
                        ));
                    }
                    // Either the transaction is not found yet, or its current status does not
                    // match the tracer mask (e.g. Rejected when waiting for Committed | WatchedInputSpent).
                    _ => {
                        let timeout = tracer
                            .mask
                            .contains(CkbTxTracingMask::Unknown)
                            .then_some(Duration::from_millis(TRACE_TX_WAITING_FOR_NOTIFICATION_MS));
                        let tx_hash = tracer.tx_hash;
                        let actor = self
                            .start_trace_tx_replier(myself, tracer, notifications, timeout)
                            .await;
                        state
                            .shared
                            .write()
                            .unwrap()
                            .tx_tracing_tasks
                            .entry(tx_hash)
                            .or_default()
                            .push(actor);
                    }
                };
            }
            RemoveTxTracers(tx_hash) => {
                let mut state_guard = state.shared.write().unwrap();
                for task in state_guard
                    .tx_tracing_tasks
                    .remove(&tx_hash)
                    .unwrap_or_default()
                    .into_iter()
                {
                    task.stop(Some(format!("remove tracers for tx {}", tx_hash)));
                }
            }

            BuildUnsignedFundingTx {
                funding_tx,
                request,
                funding_source_lock_script: _,
                funding_source_lock_script_cell_deps: _,
                funding_cell_lock_script,
                reply,
            } => {
                // Mock implementation: build a simple unsigned funding tx similar to Fund handler.
                let mut fulfilled_tx = funding_tx.clone();

                let (outputs, outputs_data) = if let Some(ref udt_script) = request.udt_type_script
                {
                    let ckb_amount = match request
                        .local_reserved_ckb_amount
                        .checked_add(request.remote_reserved_ckb_amount)
                    {
                        Some(value) => value,
                        None => {
                            let _ = reply.send(Err(FundingError::OverflowError));
                            return Ok(());
                        }
                    };
                    let udt_amount = match request.local_amount.checked_add(request.remote_amount) {
                        Some(value) => value,
                        None => {
                            let _ = reply.send(Err(FundingError::OverflowError));
                            return Ok(());
                        }
                    };
                    let mut data = BytesMut::with_capacity(16);
                    data.put(&udt_amount.to_le_bytes()[..]);
                    let output = CellOutput::new_builder()
                        .capacity(Capacity::shannons(ckb_amount).pack())
                        .type_(Some(udt_script.clone()).pack())
                        .lock(funding_cell_lock_script)
                        .build();
                    (vec![output], vec![data.freeze().pack()])
                } else {
                    let local_amount = match u64::try_from(request.local_amount) {
                        Ok(value) => value,
                        Err(_) => {
                            let _ = reply.send(Err(FundingError::OverflowError));
                            return Ok(());
                        }
                    };
                    let remote_amount = match u64::try_from(request.remote_amount) {
                        Ok(value) => value,
                        Err(_) => {
                            let _ = reply.send(Err(FundingError::OverflowError));
                            return Ok(());
                        }
                    };
                    let total = match local_amount
                        .checked_add(request.local_reserved_ckb_amount)
                        .and_then(|value| value.checked_add(remote_amount))
                        .and_then(|value| value.checked_add(request.remote_reserved_ckb_amount))
                    {
                        Some(value) => value,
                        None => {
                            let _ = reply.send(Err(FundingError::OverflowError));
                            return Ok(());
                        }
                    };
                    let output = CellOutput::new_builder()
                        .capacity(Capacity::shannons(total).pack())
                        .lock(funding_cell_lock_script)
                        .build();
                    (vec![output], vec![packed::Bytes::default()])
                };

                let tx_builder = fulfilled_tx
                    .take()
                    .map(|x| x.as_advanced_builder())
                    .unwrap_or_default();

                fulfilled_tx.update_for_self(
                    tx_builder
                        .set_outputs(outputs)
                        .set_outputs_data(outputs_data)
                        .build(),
                );

                debug!(
                    "Built unsigned funding tx for external funding: {:?}",
                    &fulfilled_tx
                );

                if let Err(e) = reply.send(Ok(fulfilled_tx)) {
                    error!(
                        "[{}] send reply failed: {:?}",
                        myself.get_name().unwrap_or_default(),
                        e
                    );
                }
            }

            ReportSendTxError(_, _) => {
                // ignore
            }

            Stop => {
                myself.stop(Some("stop received".to_string()));
            }
        }
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        _message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        Ok(())
    }
}

pub async fn submit_tx(mock_actor: ActorRef<CkbChainMessage>, tx: TransactionView) -> TxStatus {
    pub const TIMEOUT: u64 = 1000;
    debug!("Calling chain actor to submit tx: {:?}", &tx);
    if let Err(error) = call_t!(mock_actor, CkbChainMessage::SendTx, TIMEOUT, tx.clone())
        .expect("chain actor alive")
    {
        error!("submit tx failed: {:?}", error);
        return TxStatus::Rejected("submit tx failed".to_string());
    }
    trace_tx(mock_actor, tx.hash().into()).await
}

pub async fn trace_tx(mock_actor: ActorRef<CkbChainMessage>, tx_hash: Hash256) -> TxStatus {
    // ($actor:expr, $msg:expr, $timeout_ms:expr) => {{
    let message = |callback| {
        CkbChainMessage::CreateTxTracer(CkbTxTracer {
            tx_hash,
            confirmations: 1,
            mask: CkbTxTracingMask::Committed,
            watched_inputs: Vec::new(),
            callback,
        })
    };
    call_t!(mock_actor, message, TRACE_TX_TIMEOUT_MS)
        .expect("chain actor alive")
        .tx_status
}

pub fn complete_commitment_tx(commitment_tx: &TransactionView) -> TransactionView {
    let cell_deps = futures::executor::block_on(get_cell_deps(
        vec![Contract::FundingLock],
        &commitment_tx.outputs().get(0).unwrap().type_().to_opt(),
    ))
    .expect("get cell deps should be ok");
    commitment_tx
        .as_advanced_builder()
        .cell_deps(cell_deps)
        .build()
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_set_and_get_block_timestamp() {
    // A block of its own: the mock chain commits every transaction in H256::default(), so another
    // test may have cached that block's timestamp already.
    let block = H256::from(rand::random::<[u8; 32]>());
    let now = now_timestamp_as_millis_u64();
    // One lock for both steps, or a parallel test reading a new block takes the next timestamp.
    let context = BLOCK_TIMESTAMP_CONTEXT.get_or_init(|| TokioRwLock::new(Default::default()));
    let mut context = context.write().await;
    context.set_next_block_timestamp(now);
    assert_eq!(context.get_block_timestamp(block), now);
}

#[derive(Clone, Debug)]
pub struct MockCkbChainClient {
    pub state: Arc<RwLock<MockChainState>>,
    #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
    pub simulation_hook: Option<Arc<dyn crate::simulation::SimulatedChainClientHook>>,
}

impl MockCkbChainClient {
    pub fn new(state: Arc<RwLock<MockChainState>>) -> Self {
        Self {
            state,
            #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
            simulation_hook: None,
        }
    }
}

#[async_trait::async_trait]
impl CkbChainClient for MockCkbChainClient {
    async fn get_transaction(&self, hash: ckb_types::H256) -> Result<GetTxResponse, anyhow::Error> {
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        if let Some(hook) = &self.simulation_hook {
            hook.before_client_request("get_transaction").await?;
        }
        let hash: Hash256 = hash.into();
        let state = self.state.read().unwrap();
        Ok(state
            .txs
            .get(&hash)
            .map(|response| {
                let tx_status = match &response.tx_status {
                    TxStatus::Pending => ckb_jsonrpc_types::TxStatus::pending(),
                    TxStatus::Proposed => ckb_jsonrpc_types::TxStatus::proposed(),
                    TxStatus::Committed(number, hash, index) => {
                        ckb_jsonrpc_types::TxStatus::committed(
                            (*number).into(),
                            hash.clone(),
                            (*index).into(),
                        )
                    }
                    TxStatus::Rejected(reason) => {
                        ckb_jsonrpc_types::TxStatus::rejected(reason.clone())
                    }
                    TxStatus::Unknown => ckb_jsonrpc_types::TxStatus::unknown(),
                };
                ckb_jsonrpc_types::TransactionWithStatusResponse {
                    transaction: response
                        .transaction
                        .clone()
                        .map(ckb_jsonrpc_types::TransactionView::from)
                        .map(ckb_jsonrpc_types::ResponseFormat::json),
                    tx_status,
                    cycles: None,
                    time_added_to_pool: None,
                    min_replace_fee: None,
                    fee: None,
                }
            })
            .into())
    }

    async fn get_cells(
        &self,
        search_key: ckb_sdk::rpc::ckb_indexer::SearchKey,
        order: ckb_sdk::rpc::ckb_indexer::Order,
        limit: u32,
        after: Option<ckb_jsonrpc_types::JsonBytes>,
    ) -> Result<ckb_sdk::rpc::ckb_indexer::Pagination<ckb_sdk::rpc::ckb_indexer::Cell>, anyhow::Error>
    {
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        if let Some(hook) = &self.simulation_hook {
            hook.before_client_request("get_cells").await?;
        }
        let state = self.state.read().unwrap();
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        if let Some(driver) = state.driver.as_ref().filter(|driver| driver.is_active()) {
            return Ok(driver.query_cells(&state, &search_key, order, limit, after));
        }
        let _ = (search_key, order, limit, after);
        Ok(ckb_sdk::rpc::ckb_indexer::Pagination {
            objects: state.indexer_cells.clone(),
            last_cursor: ckb_jsonrpc_types::JsonBytes::default(),
        })
    }

    async fn get_block_timestamp(&self, block_hash: Hash256) -> Result<Option<u64>, anyhow::Error> {
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        if let Some(hook) = &self.simulation_hook {
            hook.before_client_request("get_block_timestamp").await?;
        }
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        if crate::simulation::is_running() {
            let mut state = self.state.write().expect("shared mock chain");
            return Ok(Some(
                state
                    .simulation_timestamps
                    .get_block_timestamp(block_hash.into()),
            ));
        }
        let timestamp = get_block_timestamp(block_hash.into()).await;
        Ok(Some(timestamp))
    }

    async fn get_shutdown_tx(
        &self,
        _funding_lock_script: ckb_types::packed::Script,
    ) -> Result<Option<crate::ckb::client::GetShutdownTxResponse>, anyhow::Error> {
        #[cfg(all(feature = "simulation", not(target_arch = "wasm32")))]
        if let Some(hook) = &self.simulation_hook {
            hook.before_client_request("get_shutdown_tx").await?;
            let state = self.state.read().expect("shared mock chain");
            // Discover a confirmed spend of the actual funding output, as the
            // production indexer would. Unconfirmed spends are not shutdown evidence.
            return Ok(state
                .txs
                .iter()
                .filter(|(_, response)| matches!(response.tx_status, TxStatus::Committed(..)))
                .filter(|(_, response)| {
                    response.transaction.as_ref().is_some_and(|tx| {
                        tx.input_pts_iter().any(|input| {
                            let output_index: u32 = input.index().unpack();
                            state
                                .txs
                                .get(&Hash256::from(input.tx_hash()))
                                .and_then(|funding| funding.transaction.as_ref())
                                .and_then(|funding| funding.outputs().get(output_index as usize))
                                .is_some_and(|output| output.lock() == _funding_lock_script)
                        })
                    })
                })
                .min_by_key(|(hash, _)| *hash)
                .map(|(_, response)| crate::ckb::client::GetShutdownTxResponse {
                    transaction: response.transaction.clone(),
                    tx_status: response.tx_status.clone(),
                }));
        }
        Ok(None)
    }
}
