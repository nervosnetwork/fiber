use anyhow::anyhow;
use ckb_jsonrpc_types::JsonBytes;
use ckb_sdk::{rpc::ckb_indexer::Cell, tx_builder::TxBuilderError, RpcError};
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

use std::{collections::HashMap, sync::Arc, sync::RwLock};
use tokio::sync::RwLock as TokioRwLock;

use crate::{
    ckb::{
        actor::{GetCellsResponse, GetTxResponse},
        config::{UdtArgInfo, UdtCfgInfos, UdtScript},
        contracts::{get_cell_deps, Contract, ContractsContext, ContractsInfo, ScriptCellDep},
        CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult, FundingError,
    },
    fiber::types::Hash256,
    now_timestamp_as_millis_u64,
};

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

#[derive(Default)]
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
                .dep_type(DepType::Code.into())
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
}

impl TraceTxReplier {
    pub fn new(tx_hash: Hash256) -> Self {
        Self { tx_hash }
    }
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Actor for TraceTxReplier {
    type Msg = CkbTxTracingResult;
    type Arguments = (
        Arc<OutputPort<CkbTxTracingResult>>,
        Duration,
        RpcReplyPort<CkbTxTracingResult>,
    );
    type State = Option<RpcReplyPort<CkbTxTracingResult>>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (notifier, timeout, reply_port): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let tx_hash = self.tx_hash;
        myself.send_after(timeout, move || CkbTxTracingResult {
            tx_hash,
            tx_status: TxStatus::Unknown,
        });

        let tx_hash = self.tx_hash;
        notifier.subscribe(myself, move |notification| {
            if notification.tx_hash == tx_hash {
                Some(notification)
            } else {
                None
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
        if let Some(reply_port) = reply_port.take() {
            let _ = reply_port.send(message);
        }
        myself.stop(Some("handled trace tx result".to_string()));
        Ok(())
    }
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
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

impl Clone for Box<dyn MockChainActorMiddleware> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

pub struct MockChainActorState {
    txs: HashMap<Hash256, GetTxResponse>,
    tx_tracing_tasks: HashMap<Hash256, Vec<ActorRef<CkbTxTracingResult>>>,
    tx_notifications: Arc<OutputPort<CkbTxTracingResult>>,
    cell_status: HashMap<OutPoint, CellStatus>,
    middleware: Option<Box<dyn MockChainActorMiddleware>>,
}

impl Default for MockChainActorState {
    fn default() -> Self {
        Self::new()
    }
}

impl MockChainActorState {
    pub fn new() -> Self {
        Self {
            txs: HashMap::new(),
            tx_tracing_tasks: HashMap::new(),
            tx_notifications: Arc::new(OutputPort::default()),
            cell_status: HashMap::new(),
            middleware: None,
        }
    }

    pub fn with_optional_middleware(middleware: Option<Box<dyn MockChainActorMiddleware>>) -> Self {
        Self {
            txs: HashMap::new(),
            tx_tracing_tasks: HashMap::new(),
            tx_notifications: Arc::new(OutputPort::default()),
            cell_status: HashMap::new(),
            middleware,
        }
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
        tx_hash: Hash256,
        notifier: Arc<OutputPort<CkbTxTracingResult>>,
        timeout: Duration,
        reply_port: RpcReplyPort<CkbTxTracingResult>,
    ) {
        let _ = Actor::spawn_linked(
            None,
            TraceTxReplier::new(tx_hash),
            (notifier, timeout, reply_port),
            myself.get_cell(),
        )
        .await
        .expect("start trace tx replier");
    }
}
#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Actor for MockChainActor {
    type Msg = CkbChainMessage;
    type State = MockChainActorState;
    type Arguments = Option<Box<dyn MockChainActorMiddleware>>;
    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Self::State::with_optional_middleware(args))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
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

                let outputs = match outputs.get(0) {
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
                                .build();

                            let mut outputs_builder = outputs.as_builder();
                            outputs_builder.replace(0, udt_output);
                            outputs_builder.build()
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
                                output
                                    .as_builder()
                                    .capacity((capacity as u64).pack())
                                    .build(),
                            );
                            outputs_builder.build()
                        }
                    }
                    None => [CellOutput::new_builder()
                        .capacity(
                            (request.local_amount as u64 + request.local_reserved_ckb_amount)
                                .pack(),
                        )
                        .lock(request.script.clone())
                        .build()]
                    .pack(),
                };

                let outputs_data = if let Some(ref _udt_script) = request.udt_type_script {
                    let udt_amount = request.local_amount + request.remote_amount;
                    let mut data = BytesMut::with_capacity(16);
                    data.put(&udt_amount.to_le_bytes()[..]);
                    vec![data.freeze().pack()].pack()
                } else {
                    let outputs_data = fulfilled_tx
                        .as_ref()
                        .map(|x| x.outputs_data())
                        .unwrap_or_default();
                    if outputs_data.is_empty() {
                        [Default::default()].pack()
                    } else {
                        outputs_data
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
            AddFundingTx(_) | RemoveFundingTx(_) | CommitFundingTx(..) => {
                // ignore
            }
            GetShutdownTx(.., reply) => {
                let _ = reply.send(Ok(None));
            }
            GetCells(.., reply) => {
                let _ = reply.send(Ok(GetCellsResponse {
                    objects: Vec::<Cell>::new(),
                    last_cursor: JsonBytes::default(),
                }));
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
                const MAX_CYCLES: u64 = 100_000_000;
                let mut f = || {
                    // Mark the inputs as consumed
                    for input in tx.input_pts_iter() {
                        match state.cell_status.entry(input.clone()) {
                            std::collections::hash_map::Entry::Occupied(mut entry) => {
                                if *entry.get() == CellStatus::Consumed {
                                    return (
                                        TxStatus::Rejected("Cell already consumed".to_string()),
                                        Err(ckb_sdk::RpcError::Other(anyhow!(
                                            "Cell {:?} already consumed",
                                            &input
                                        ))),
                                    );
                                }
                                *entry.get_mut() = CellStatus::Consumed;
                            }
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                debug!("Consuming cell {:?}", &input);
                                entry.insert(CellStatus::Consumed);
                            }
                        }
                    }

                    let context = &mut MOCK_CONTEXT.write().unwrap().context;
                    match context.verify_tx(&tx, MAX_CYCLES) {
                        Ok(c) => {
                            debug!("Verified transaction: {:?} with {} CPU cycles", tx, c);
                            // Also save the outputs to the context, so that we can refer to
                            // these out points later.
                            for outpoint in tx.output_pts().into_iter() {
                                let index: u32 = outpoint.index().unpack();
                                let index = index as usize;
                                let cell = tx.outputs().get(index).unwrap();
                                let data = tx.outputs_data().get(index).unwrap();
                                context.create_cell_with_out_point(
                                    outpoint.clone(),
                                    cell,
                                    data.as_bytes(),
                                );
                            }
                            (TxStatus::Committed(0, H256::default(), 0), Ok(()))
                        }
                        Err(e) => (
                            TxStatus::Rejected("Failed to verify transaction".to_string()),
                            Err(ckb_sdk::RpcError::Other(anyhow!(
                                "Failed to verify transaction: {:?}, error: {:?}",
                                tx,
                                e
                            ))),
                        ),
                    }
                };
                let (tx_status, result) = f();
                debug!(
                    "Transaction verification result: tx {:?}, status: {:?}",
                    &tx, &tx_status
                );
                let tx_hash = tx.hash().into();
                state.tx_notifications.send(CkbTxTracingResult {
                    tx_hash,
                    tx_status: tx_status.clone(),
                });
                state.txs.insert(
                    tx_hash,
                    GetTxResponse {
                        transaction: Some(tx),
                        tx_status,
                    },
                );
                if let Err(e) = reply_port.send(result) {
                    error!(
                        "[{}] send reply failed: {:?}",
                        myself.get_name().unwrap_or_default(),
                        e
                    );
                }
            }
            GetTx(tx_hash, reply_port) => {
                let result = Ok(state.txs.get(&tx_hash).cloned().unwrap_or(GetTxResponse {
                    transaction: None,
                    tx_status: TxStatus::Unknown,
                }));
                let _ = reply_port.send(result);
            }
            CreateTxTracer(tracer) => {
                debug!("Tracing transaction: {:?}", &tracer);
                match state.txs.get(&tracer.tx_hash) {
                    Some(tx) => {
                        let _ = tracer.callback.send(CkbTxTracingResult {
                            tx_hash: tracer.tx_hash,
                            tx_status: tx.tx_status.clone(),
                        });
                    }
                    // The transaction is not found in the tx_status, we need to wait for the
                    // tx notification from the mock chain actor.
                    None => {
                        self.start_trace_tx_replier(
                            myself,
                            tracer.tx_hash,
                            state.tx_notifications.clone(),
                            Duration::from_millis(TRACE_TX_WAITING_FOR_NOTIFICATION_MS),
                            tracer.callback,
                        )
                        .await;
                    }
                };
            }
            RemoveTxTracers(tx_hash) => {
                for task in state
                    .tx_tracing_tasks
                    .remove(&tx_hash)
                    .unwrap_or_default()
                    .into_iter()
                {
                    task.stop(Some(format!("remove tracers for tx {}", tx_hash)));
                }
            }
            GetBlockTimestamp(request, rpc_reply_port) => {
                let timestamp = get_block_timestamp(request.block_hash().into()).await;
                let _ = rpc_reply_port.send(Ok(Some(timestamp)));
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
            callback,
        })
    };
    call_t!(mock_actor, message, TRACE_TX_TIMEOUT_MS)
        .expect("chain actor alive")
        .tx_status
}

pub async fn get_tx_from_hash(
    mock_actor: ActorRef<CkbChainMessage>,
    tx_hash: Hash256,
) -> Result<GetTxResponse, RpcError> {
    pub const TIMEOUT: u64 = 1000;
    call_t!(mock_actor, CkbChainMessage::GetTx, TIMEOUT, tx_hash).expect("chain actor alive")
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
    let now = now_timestamp_as_millis_u64();
    set_next_block_timestamp(now).await;
    let timestamp = get_block_timestamp(H256::default()).await;
    assert_eq!(timestamp, now);
}
