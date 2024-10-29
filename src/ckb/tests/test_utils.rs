use anyhow::anyhow;
use ckb_jsonrpc_types::TxStatus;
use ckb_testtool::context::Context;
use ckb_types::{
    bytes::Bytes,
    core::{DepType, TransactionView},
    packed::{CellDep, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Pack, PackVec, Unpack},
};
use std::collections::HashMap;

use crate::ckb::{
    config::UdtCfgInfos,
    contracts::{Contract, ContractsContext, ContractsInfo, CONTRACTS_CONTEXT_INSTANCE},
    TraceTxRequest, TraceTxResponse,
};

use crate::ckb::CkbChainMessage;

use ckb_types::packed::Byte32;
use ractor::{call_t, Actor, ActorProcessingErr, ActorRef};
use tracing::{debug, error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    // This cell has been consumed. If any transaction
    // tries to consume the same cell, it should be rejected.
    Consumed,
}

pub struct MockContext {
    pub context: Context,
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
        let _ = CONTRACTS_CONTEXT_INSTANCE.set(contracts_context);

        MockContext { context }
    }
}

pub struct MockChainActorState {
    ctx: MockContext,
    tx_status: HashMap<
        Byte32,
        (
            ckb_jsonrpc_types::TransactionView,
            ckb_jsonrpc_types::Status,
        ),
    >,
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
            ctx: MockContext::new(),
            tx_status: HashMap::new(),
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
            GetCurrentBlockNumber(_, reply) => {
                let _ = reply.send(Ok(0));
            }
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
                    let context = &mut state.ctx.context;
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
                let (tx_view, status) = match state.tx_status.get(&tx.tx_hash).cloned() {
                    Some((tx_view, status)) => (Some(tx_view), status),
                    None => (None, ckb_jsonrpc_types::Status::Unknown),
                };

                debug!(
                    "Tracing transaction: {:?}, status: {:?}",
                    &tx.tx_hash, &status
                );
                let status = TxStatus {
                    status,
                    // Some tests may require the block hash and block number to be set.
                    block_number: Some(Default::default()),
                    block_hash: Some(ckb_types::H256(
                        tx.tx_hash.clone().as_slice().try_into().unwrap(),
                    )),
                    tx_index: None,
                    reason: None,
                };
                let response = TraceTxResponse {
                    tx: tx_view,
                    status,
                };

                if let Err(e) = reply_port.send(response) {
                    error!(
                        "[{}] send reply failed: {:?}",
                        myself.get_name().unwrap_or_default(),
                        e
                    );
                }
            }
        }
        Ok(())
    }
}

pub async fn submit_tx(
    mock_actor: ActorRef<CkbChainMessage>,
    tx: TransactionView,
) -> ckb_jsonrpc_types::Status {
    pub const TIMEOUT: u64 = 1000;
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
    )
    .expect("chain actor alive")
    .status
    .status
}
