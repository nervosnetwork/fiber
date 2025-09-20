use anyhow::anyhow;
use ckb_sdk::{tx_builder::TxBuilderError, RpcError};
use ckb_testtool::context::Context;
use ckb_types::{
    bytes::Bytes,
    core::{tx_pool::TxStatus, Capacity, DepType, TransactionView},
    packed::{CellDep, CellInput, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Pack, Unpack},
    H256,
};
use once_cell::sync::{Lazy, OnceCell};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};
use tokio::sync::RwLock as TokioRwLock;

use crate::{
    ckb::{
        actor::GetTxResponse,
        config::{UdtArgInfo, UdtCfgInfos, UdtDep, UdtScript},
        contracts::{
            get_cell_deps, get_cell_deps_by_contracts, get_script_by_contract, get_udt_cell_deps,
            Contract, ContractsContext, ContractsInfo, ScriptCellDep,
        },
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

pub static MOCK_CONTEXT: Lazy<MockContext> = Lazy::new(MockContext::new);

pub struct MockContext {
    pub context: RwLock<Context>,
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
            (
                Contract::AlwaysSuccess,
                Bytes::from_static(include_bytes!(
                    "../../../../../tests/deploy/contracts/always_success"
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

        let udt_whitelist = [Contract::SimpleUDT, Contract::AlwaysSuccess]
            .into_iter()
            .map(|contract| {
                let script = contract_default_scripts.get(&contract).unwrap();
                let cell_deps: Vec<UdtDep> = script_cell_deps
                    .get(&contract)
                    .map(|x| x.iter().map(Into::into).collect())
                    .unwrap_or_default();
                UdtArgInfo {
                    name: format!("{:?}", contract),
                    script: allow_all_for_script(script),
                    auto_accept_amount: None,
                    cell_deps,
                }
            })
            .collect();

        let contracts = ContractsInfo {
            contract_default_scripts,
            script_cell_deps,
            udt_whitelist: UdtCfgInfos(udt_whitelist),
        };
        let contracts_context = ContractsContext {
            contracts,
            type_id_resolver: None,
        };
        MockContext {
            context: RwLock::new(context),
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
            Fund(mut tx, request, reply_port) => {
                // We only know how to build the funding transaction for SimpleUDT and AlwaysSuccess.
                let contract = request.udt_type_script.as_ref().map(|script| {
                    let supported_contracts = [Contract::SimpleUDT, Contract::AlwaysSuccess];
                    let current_args: Vec<u8> = script.args().unpack();
                    supported_contracts
                        .into_iter()
                        .find(|contract| {
                            script == &get_script_by_contract(*contract, current_args.as_slice())
                        })
                        .expect("Script should be one of the supported contracts")
                });

                let is_udt = request.udt_type_script.is_some();

                let (first_output, first_output_data) = match tx
                    .as_ref()
                    .and_then(|x| x.output_with_data(0))
                {
                    Some((output, output_data)) => {
                        if output.lock() != request.script {
                            error!(
                                "funding request script ({:?}) does not match the first output lock script ({:?})", request.script, output.lock()
                            );
                            return Ok(());
                        }
                        if output.type_() != request.udt_type_script.pack() {
                            error!(
                                "funding request udt type script ({:?}) does not match the first output type script ({:?})", request.udt_type_script, output.type_()
                            );
                            return Ok(());
                        }
                        if is_udt && output_data.len() < 16 {
                            error!(
                                "funding request output data is too short: {:?}, expected at least 16 bytes", output_data
                            );
                            return Ok(());
                        }
                        (output, output_data)
                    }
                    // Create a dummy output and output_data so that we can "update" existing output and output_data.
                    _ => (
                        CellOutput::new_builder()
                            .capacity(Capacity::shannons(0).pack())
                            .lock(request.script.clone())
                            .type_(request.udt_type_script.clone().pack())
                            .build(),
                        if is_udt {
                            [0u8; 16].to_vec().into()
                        } else {
                            Default::default()
                        },
                    ),
                };

                let (output, output_data) = if is_udt {
                    let mut data = [0u8; 16];
                    data.copy_from_slice(&first_output_data.as_ref()[..16]);
                    let udt_amount = request.local_amount + u128::from_le_bytes(data);
                    let current_capacity: u64 = first_output.capacity().unpack();
                    let ckb_amount = request.local_reserved_ckb_amount + current_capacity;
                    (
                        first_output
                            .as_builder()
                            .capacity(ckb_amount.pack())
                            .build(),
                        udt_amount.to_le_bytes().pack(),
                    )
                } else {
                    let current_capacity: u64 = first_output.capacity().unpack();
                    let ckb_amount = request.local_amount
                        + (request.local_reserved_ckb_amount as u128)
                        + (current_capacity as u128);
                    if ckb_amount > u64::MAX as u128 {
                        let _ = reply_port.send(Err(FundingError::CkbTxBuilderError(
                            TxBuilderError::Other(anyhow!("capacity overflow")),
                        )));
                        return Ok(());
                    }
                    let ckb_amount = ckb_amount as u64;

                    (
                        first_output
                            .as_builder()
                            .capacity(ckb_amount.pack())
                            .build(),
                        first_output_data.pack(),
                    )
                };

                let outputs = tx.as_ref().map(|x| x.outputs()).unwrap_or_default();
                let outputs = if outputs.is_empty() {
                    outputs.as_builder().push(output).build()
                } else {
                    let mut builder = outputs.as_builder();
                    builder.replace(0, output);
                    builder.build()
                };

                let outputs_data = tx.as_ref().map(|x| x.outputs_data()).unwrap_or_default();
                let outputs_data = if outputs_data.is_empty() {
                    outputs_data.as_builder().push(output_data).build()
                } else {
                    let mut builder = outputs_data.as_builder();
                    builder.replace(0, output_data);
                    builder.build()
                };

                let cell_deps = [
                    tx.as_ref().map(|x| x.cell_deps()).unwrap_or_default(),
                    match request.udt_type_script.as_ref() {
                        Some(script) => get_udt_cell_deps(script).await.ok().unwrap_or_default(),
                        None => Default::default(),
                    },
                    // AlwaysSuccess is needed to unlock the input cells
                    get_cell_deps_by_contracts(vec![Contract::AlwaysSuccess])
                        .await
                        .expect("get always success cell deps"),
                ]
                .into_iter()
                .flatten()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();

                let new_tx = tx
                    .as_ref()
                    .map(|x| x.as_advanced_builder())
                    .unwrap_or_default()
                    .set_cell_deps(cell_deps)
                    .set_outputs(outputs.into_iter().collect())
                    .set_outputs_data(outputs_data.into_iter().collect())
                    .build();

                let new_tx = match contract {
                    Some(Contract::SimpleUDT) if new_tx.inputs().is_empty() => {
                        let context = &mut MOCK_CONTEXT.context.write().expect("get mock context");
                        let outpoint =
                            create_deterministic_outpoint_from_seed(new_tx.hash().as_slice());
                        context.create_cell_with_out_point(
                            outpoint.clone(),
                            CellOutput::new_builder()
                                .lock(get_script_by_contract(Contract::AlwaysSuccess, &[]))
                                .type_(request.udt_type_script.clone().pack())
                                .build(),
                            u128::MAX.to_le_bytes().to_vec().into(),
                        );
                        new_tx
                            .as_advanced_builder()
                            .set_inputs(vec![CellInput::new_builder()
                                .previous_output(outpoint)
                                .build()])
                            .build()
                    }
                    _ => new_tx,
                };

                debug!(
                    "Fulfilling funding request: request: {:?}, original tx: {:?}, fulfilled tx: {:?}",
                    request, &tx, &new_tx
                );

                tx.update_for_self(new_tx);

                if let Err(e) = reply_port.send(Ok(tx)) {
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
                let (tx_status, result) = if let Some(resp) = state.txs.get(&tx.hash().into()) {
                    // Like a real CKB node, this mock should allow sending
                    // duplicate transactions. This is required because the
                    // `Fund(..)` branch may add inputs for UDT channels, and
                    // processing the same UDT funding transaction multiple
                    // times would otherwise lead to consumed cell errors.
                    (
                        resp.tx_status.clone(),
                        match &resp.tx_status {
                            TxStatus::Committed(..) => Ok(()),
                            TxStatus::Rejected(reason) => {
                                Err(ckb_sdk::RpcError::Other(anyhow!("{}", reason)))
                            }
                            _ => {
                                unreachable!();
                            }
                        },
                    )
                } else {
                    match tx.input_pts_iter().find(|input| {
                        state
                            .cell_status
                            .get(input)
                            .is_some_and(|status| *status == CellStatus::Consumed)
                    }) {
                        Some(input) => (
                            TxStatus::Rejected("Cell already consumed".to_string()),
                            Err(ckb_sdk::RpcError::Other(anyhow!(
                                "Cell {:?} already consumed",
                                &input
                            ))),
                        ),
                        None => {
                            // Mark the inputs as consumed
                            for input in tx.input_pts_iter() {
                                state.cell_status.insert(input, CellStatus::Consumed);
                            }
                            let verify_context =
                                &mut MOCK_CONTEXT.context.write().expect("get mock context");
                            match verify_context.verify_tx(&tx, MAX_CYCLES) {
                                Ok(c) => {
                                    debug!("Verified transaction: {:?} with {} CPU cycles", tx, c);
                                    // Also save the outputs to the context, so that we can refer to
                                    // these out points later.
                                    for outpoint in tx.output_pts().into_iter() {
                                        let index: u32 = outpoint.index().unpack();
                                        let index = index as usize;
                                        let cell = tx.outputs().get(index).unwrap();
                                        let data = tx.outputs_data().get(index).unwrap();
                                        verify_context.create_cell_with_out_point(
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
                        }
                    }
                };
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
        let reject_reason = format!("submit tx failed: {:?}", error);
        error!("{}", reject_reason);
        return TxStatus::Rejected(reject_reason);
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

pub fn create_deterministic_outpoint_from_seed<S: AsRef<[u8]>>(seed: S) -> OutPoint {
    let hash = ckb_hash::blake2b_256(seed.as_ref());
    OutPoint::new_builder().tx_hash(hash.pack()).build()
}

pub fn get_simple_udt_script() -> Script {
    let args =
        hex::decode("32e555f3ff8e135cece1351a6a2971518392c1e30375c1e006ad0ce8eac07947").unwrap();
    get_script_by_contract(Contract::SimpleUDT, &args)
}

pub fn get_always_success_script() -> Script {
    get_script_by_contract(Contract::AlwaysSuccess, &[])
}

fn allow_all_for_script(script: &Script) -> UdtScript {
    UdtScript {
        code_hash: H256(script.code_hash().as_slice().try_into().expect("32 bytes")),
        hash_type: script.hash_type().try_into().expect("valid hash type"),
        args: "0x.*".to_string(),
    }
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_set_and_get_block_timestamp() {
    let now = now_timestamp_as_millis_u64();
    set_next_block_timestamp(now).await;
    let timestamp = get_block_timestamp(H256::default()).await;
    assert_eq!(timestamp, now);
}
