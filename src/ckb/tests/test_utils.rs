use anyhow::anyhow;
use ckb_jsonrpc_types::TxStatus;
use ckb_testtool::{ckb_hash, context::Context};
use ckb_types::{
    bytes::Bytes,
    core::{Capacity, DepType, TransactionView},
    packed::{CellDep, CellInput, CellOutput, OutPoint, Script, Transaction},
    prelude::{Builder, Entity, IntoTransactionView, Pack, Unpack},
    H256,
};
use once_cell::sync::{Lazy, OnceCell};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::RwLock;

use crate::{
    ckb::{
        config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtScript},
        contracts::{
            get_cell_deps_by_contracts, get_script_by_contract, get_udt_cell_deps, Contract,
            ContractsContext, ContractsInfo,
        },
        TraceTxRequest, TraceTxResponse,
    },
    now_timestamp_as_millis_u64,
};

use crate::ckb::CkbChainMessage;

use ckb_types::packed::Byte32;
use ractor::{
    call_t, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, OutputPort, RpcReplyPort,
    SupervisionEvent,
};
use tracing::{debug, error};

pub const TRACE_TX_WAITING_FOR_NOTIFICATION_MS: u64 = 2 * 1000;
pub const TRACE_TX_TIMEOUT_MS: u64 = 3 * 1000;

type TxNotification = (
    Byte32,
    ckb_jsonrpc_types::TransactionView,
    ckb_jsonrpc_types::Status,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    // This cell has been consumed. If any transaction
    // tries to consume the same cell, it should be rejected.
    Consumed,
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
                Bytes::from_static(include_bytes!("../../../tests/deploy/contracts/auth")),
            ),
            (
                Contract::FundingLock,
                Bytes::from_static(include_bytes!(
                    "../../../tests/deploy/contracts/funding-lock"
                )),
            ),
            (
                Contract::CommitmentLock,
                Bytes::from_static(include_bytes!(
                    "../../../tests/deploy/contracts/commitment-lock"
                )),
            ),
            // mock secp256k1 lock script
            (
                Contract::Secp256k1Lock,
                Bytes::from_static(include_bytes!(
                    "../../../tests/deploy/contracts/always_success"
                )),
            ),
            (
                Contract::SimpleUDT,
                Bytes::from_static(include_bytes!("../../../tests/deploy/contracts/simple_udt")),
            ),
            (
                Contract::AlwaysSuccess,
                Bytes::from_static(include_bytes!(
                    "../../../tests/deploy/contracts/always_success"
                )),
            ),
        ];
        let mut context = Context::new_with_deterministic_rng();
        let mut contract_default_scripts: HashMap<Contract, Script> = HashMap::new();
        let mut script_cell_deps: HashMap<Contract, Vec<CellDep>> = HashMap::new();

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
                    cell_dep,
                    script_cell_deps
                        .get(&Contract::CkbAuth)
                        .unwrap()
                        .clone()
                        .first()
                        .unwrap()
                        .clone(),
                ]
            } else {
                vec![cell_dep]
            };
            script_cell_deps.insert(contract, cell_deps);
        }

        let udt_whitelist = [Contract::SimpleUDT, Contract::AlwaysSuccess]
            .into_iter()
            .map(|contract| {
                let script = contract_default_scripts.get(&contract).unwrap();
                let cell_deps: Vec<UdtCellDep> = script_cell_deps
                    .get(&contract)
                    .map(|x| x.iter().map(Into::into).collect())
                    .unwrap_or_default();
                UdtArgInfo {
                    name: format!("{:?}", contract),
                    script: UdtScript::allow_all_for_script(script),
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
        let contracts_context = ContractsContext { contracts };
        MockContext {
            context: RwLock::new(context),
            contracts_context,
        }
    }
}

enum TraceTxResult {
    Found(TxNotification),
    Timeout(),
}

// A simple actor to wait for the tx notifications from mock chain actor,
// Sometimes we are sending SendTx to the mock chain actor after a TraceTx request.
// In this case, we don't want to prematurely tell the caller that the transaction
// does not exists. So we use this actor to wait for the tx notifications from the
// mock chain actor.
struct TraceTxReplier {
    tx_hash: Byte32,
}

impl TraceTxReplier {
    pub fn new(tx_hash: Byte32) -> Self {
        Self { tx_hash }
    }
}

#[ractor::async_trait]
impl Actor for TraceTxReplier {
    type Msg = TraceTxResult;
    type Arguments = (
        Arc<OutputPort<TxNotification>>,
        Duration,
        RpcReplyPort<TraceTxResponse>,
    );
    type State = Option<RpcReplyPort<TraceTxResponse>>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (notifier, timeout, reply_port): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_after(timeout, TraceTxResult::Timeout);
        let hash = self.tx_hash.clone();
        notifier.subscribe(myself, move |notification| {
            if notification.0 == hash {
                Some(TraceTxResult::Found(notification))
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
        let (tx, status) = match message {
            TraceTxResult::Found((_hash, tx, status)) => (Some(tx), status),
            TraceTxResult::Timeout() => {
                debug!("Timeout waiting for tx notification: {:?}", self.tx_hash);
                (None, ckb_jsonrpc_types::Status::Unknown)
            }
        };

        reply_trace_tx(
            tx,
            status,
            reply_port
                .take()
                .expect("state is initialized, and handle function will only be called once"),
        );

        myself.stop(Some("handled trace tx result".to_string()));
        Ok(())
    }
}

fn reply_trace_tx(
    tx: Option<ckb_jsonrpc_types::TransactionView>,
    status: ckb_jsonrpc_types::Status,
    reply_port: RpcReplyPort<TraceTxResponse>,
) {
    let block_hash = tx.as_ref().map(|tx| tx.hash.clone());
    let status = TxStatus {
        status,
        // Some tests may require the block hash and block number to be set.
        block_number: Some(Default::default()),
        block_hash: block_hash.clone(),
        tx_index: None,
        reason: None,
    };
    let response = TraceTxResponse { tx, status };

    if let Err(e) = reply_port.send(response) {
        error!(
            "Sending trace tx result of {:?} failed: {:?}",
            block_hash, e
        );
    };
}

pub struct MockChainActorState {
    tx_status: HashMap<
        Byte32,
        (
            ckb_jsonrpc_types::TransactionView,
            ckb_jsonrpc_types::Status,
        ),
    >,
    tx_notifications: Arc<OutputPort<TxNotification>>,
    cell_status: HashMap<OutPoint, CellStatus>,
}

impl Default for MockChainActorState {
    fn default() -> Self {
        Self::new()
    }
}

impl MockChainActorState {
    pub fn new() -> Self {
        Self {
            tx_status: HashMap::new(),
            tx_notifications: Arc::new(OutputPort::default()),
            cell_status: HashMap::new(),
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
        tx_hash: Byte32,
        notifier: Arc<OutputPort<TxNotification>>,
        timeout: Duration,
        reply_port: RpcReplyPort<TraceTxResponse>,
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

#[ractor::async_trait]
impl Actor for MockChainActor {
    type Msg = CkbChainMessage;
    type State = MockChainActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        _: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Self::State::new())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
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
                    let ckb_amount = request.local_amount as u64
                        + request.local_reserved_ckb_amount
                        + current_capacity;
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
                    request
                        .udt_type_script
                        .as_ref()
                        .and_then(get_udt_cell_deps)
                        .unwrap_or_default(),
                    // AlwaysSuccess is needed to unlock the input cells
                    get_cell_deps_by_contracts(vec![Contract::AlwaysSuccess]),
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
                        let context = &mut MOCK_CONTEXT.context.write().await;
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

                tx.update_for_self(new_tx).expect("update tx");

                if let Err(e) = reply_port.send(Ok(tx)) {
                    error!(
                        "[{}] send reply failed: {:?}",
                        myself.get_name().unwrap_or_default(),
                        e
                    );
                }
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
                let (status, result) = {
                    match tx.input_pts_iter().find(|input| {
                        state
                            .cell_status
                            .get(input)
                            .map_or(false, |status| *status == CellStatus::Consumed)
                    }) {
                        Some(input) => (
                            ckb_jsonrpc_types::Status::Rejected,
                            Err(ckb_sdk::RpcError::Other(anyhow!(
                                "Transaction {:?} contains consumed input {:?}",
                                &tx,
                                &input
                            ))),
                        ),
                        None => {
                            // Mark the inputs as consumed
                            for input in tx.input_pts_iter() {
                                state.cell_status.insert(input, CellStatus::Consumed);
                            }
                            let verify_context = &mut MOCK_CONTEXT.context.write().await;
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
                                    (ckb_jsonrpc_types::Status::Committed, Ok(()))
                                }
                                Err(e) => (
                                    ckb_jsonrpc_types::Status::Rejected,
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
                    "Transaction verfication result: tx {:?}, status: {:?}",
                    &tx, &status
                );
                state
                    .tx_notifications
                    .send((tx.hash(), tx.clone().into(), status.clone()));
                state.tx_status.insert(tx.hash(), (tx.into(), status));
                if let Err(e) = reply_port.send(result) {
                    error!(
                        "[{}] send reply failed: {:?}",
                        myself.get_name().unwrap_or_default(),
                        e
                    );
                }
            }
            TraceTx(tx, reply_port) => {
                debug!("Tracing transaction: {:?}", &tx);
                match state.tx_status.get(&tx.tx_hash).cloned() {
                    Some((tx_view, status)) => {
                        reply_trace_tx(Some(tx_view), status, reply_port);
                    }
                    // The transaction is not found in the tx_status, we need to wait for the
                    // tx notification from the mock chain actor.
                    None => {
                        self.start_trace_tx_replier(
                            myself,
                            tx.tx_hash,
                            state.tx_notifications.clone(),
                            Duration::from_millis(TRACE_TX_WAITING_FOR_NOTIFICATION_MS),
                            reply_port,
                        )
                        .await;
                    }
                };
            }
            GetBlockTimestamp(request, rpc_reply_port) => {
                // The problem of channel announcement is that each nodes will query the block timestamp
                // and use it as the channel announcement timestamp.
                // Guaranteeing the block timestamp is the same across all nodes is important
                // because if a node A has a greater channel announcement timestamp than node B, then when
                // A tries to get broadcast messages after this channel announcement timestamp, B will return
                // the channel announcement. But for A, it is not a later broadcast message. This process will
                // cause an infinite loop.
                // So here we create an static lock which is shared across all nodes, and we use this lock to
                // guarantee that the block timestamp is the same across all nodes.
                static BLOCK_TIMESTAMP: OnceCell<RwLock<HashMap<H256, u64>>> = OnceCell::new();
                BLOCK_TIMESTAMP.get_or_init(|| RwLock::new(HashMap::new()));
                let timestamp = *BLOCK_TIMESTAMP
                    .get()
                    .unwrap()
                    .write()
                    .await
                    .entry(request.block_hash())
                    .or_insert(now_timestamp_as_millis_u64());

                let _ = rpc_reply_port.send(Ok(Some(timestamp)));
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

pub async fn submit_tx(
    mock_actor: ActorRef<CkbChainMessage>,
    tx: TransactionView,
) -> ckb_jsonrpc_types::Status {
    pub const TIMEOUT: u64 = 1000;
    debug!("Calling chain actor to submit tx: {:?}", &tx);
    if let Err(error) = call_t!(mock_actor, CkbChainMessage::SendTx, TIMEOUT, tx.clone())
        .expect("chain actor alive")
    {
        error!("submit tx failed: {:?}", error);
        return ckb_jsonrpc_types::Status::Rejected;
    }
    trace_tx(mock_actor, tx).await
}

pub async fn trace_tx(
    mock_actor: ActorRef<CkbChainMessage>,
    tx: TransactionView,
) -> ckb_jsonrpc_types::Status {
    trace_tx_hash(mock_actor, tx.hash()).await
}

pub async fn trace_tx_hash(
    mock_actor: ActorRef<CkbChainMessage>,
    tx_hash: Byte32,
) -> ckb_jsonrpc_types::Status {
    let request = TraceTxRequest {
        tx_hash,
        confirmations: 1,
    };
    call_t!(
        mock_actor,
        CkbChainMessage::TraceTx,
        TRACE_TX_TIMEOUT_MS,
        request
    )
    .expect("chain actor alive")
    .status
    .status
}

pub async fn get_tx_from_hash(
    mock_actor: ActorRef<CkbChainMessage>,
    tx_hash: Byte32,
) -> Result<TransactionView, anyhow::Error> {
    pub const TIMEOUT: u64 = 1000;
    let request = TraceTxRequest {
        tx_hash,
        confirmations: 1,
    };
    call_t!(
        mock_actor,
        CkbChainMessage::TraceTx,
        TIMEOUT,
        request.clone()
    )?
    .tx
    .map(|tx| Transaction::from(tx.inner).into_view())
    .ok_or(anyhow!("tx not found in trace tx response"))
}

pub fn create_deterministic_outpoint_from_seed<S: AsRef<[u8]>>(seed: S) -> OutPoint {
    let hash = ckb_hash::blake2b_256(seed.as_ref());
    OutPoint::new_builder().tx_hash(hash.pack()).build()
}
