use anyhow::anyhow;
use ckb_jsonrpc_types::TxStatus;
use ckb_testtool::context::Context;
use ckb_types::{
    bytes::Bytes,
    core::{DepType, TransactionView},
    packed::{CellDep, CellOutput, OutPoint, Script, Transaction},
    prelude::{Builder, Entity, IntoTransactionView, Pack, PackVec, Unpack},
    H256,
};
use once_cell::sync::{Lazy, OnceCell};
use std::{collections::HashMap, sync::Arc, sync::RwLock};
use tokio::sync::RwLock as TokioRwLock;

use crate::{
    ckb::{
        config::UdtCfgInfos,
        contracts::{Contract, ContractsContext, ContractsInfo},
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

pub static MOCK_CONTEXT: Lazy<RwLock<MockContext>> = Lazy::new(|| RwLock::new(MockContext::new()));

pub struct MockContext {
    pub context: Context,
    pub contracts_context: ContractsContext,
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
                        .get(0)
                        .unwrap()
                        .clone(),
                ]
            } else {
                vec![cell_dep]
            };
            script_cell_deps.insert(contract, cell_deps);
        }

        let contracts = ContractsInfo {
            contract_default_scripts,
            script_cell_deps,
            udt_whitelist: UdtCfgInfos::default(),
        };
        let contracts_context = ContractsContext { contracts };
        MockContext {
            context,
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
        let _ = myself.send_after(timeout, || TraceTxResult::Timeout());
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
        debug!("MockChainActor received message: {:?}", message);
        use CkbChainMessage::*;
        match message {
            Fund(tx, request, reply_port) => {
                let mut fulfilled_tx = tx.clone();
                let outputs = fulfilled_tx
                    .as_ref()
                    .map(|x| x.outputs())
                    .unwrap_or_default();
                let outputs = match outputs.get(0) {
                    Some(output) => {
                        if output.lock() != request.script {
                            error!(
                                    "funding request script ({:?}) does not match the first output lock script ({:?})", request.script, output.lock()
                                );
                            return Ok(());
                        }
                        let current_capacity: u64 = output.capacity().unpack();
                        let capacity = request.local_amount
                            + request.local_reserved_ckb_amount
                            + current_capacity;
                        let mut outputs_builder = outputs.as_builder();

                        outputs_builder
                            .replace(0, output.as_builder().capacity(capacity.pack()).build());
                        outputs_builder.build()
                    }
                    None => [CellOutput::new_builder()
                        .capacity((request.local_amount + request.local_reserved_ckb_amount).pack())
                        .lock(request.script.clone())
                        .build()]
                    .pack(),
                };

                let outputs_data = fulfilled_tx
                    .as_ref()
                    .map(|x| x.outputs_data())
                    .unwrap_or_default();
                let outputs_data = if outputs_data.is_empty() {
                    [Default::default()].pack()
                } else {
                    outputs_data
                };

                let tx_builder = fulfilled_tx
                    .take()
                    .map(|x| x.as_advanced_builder())
                    .unwrap_or_default();

                fulfilled_tx
                    .update_for_self(
                        tx_builder
                            .set_outputs(outputs.into_iter().collect())
                            .set_outputs_data(outputs_data.into_iter().collect())
                            .build(),
                    )
                    .expect("update tx");

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
                                        ckb_jsonrpc_types::Status::Rejected,
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
                                debug!(
                                    "Creating cell with outpoint: {:?}, cell: {:?}, data: {:?}",
                                    outpoint, cell, data
                                );
                                context.create_cell_with_out_point(
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
                };
                let (status, result) = f();
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
                static BLOCK_TIMESTAMP: OnceCell<TokioRwLock<HashMap<H256, u64>>> = OnceCell::new();
                BLOCK_TIMESTAMP.get_or_init(|| TokioRwLock::new(HashMap::new()));
                let timestamp = *BLOCK_TIMESTAMP
                    .get()
                    .unwrap()
                    .write()
                    .await
                    .entry(request.block_hash())
                    .or_insert(now_timestamp_as_millis_u64());

                debug!(
                    "Get block timestamp: block_hash: {:?}, timestamp: {}",
                    request.block_hash(),
                    timestamp
                );
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
