use ckb_sdk::{CkbRpcClient, RpcError};
use ckb_types::{
    core::TransactionView,
    packed::{self, Script},
    prelude::*,
};
use ractor::{
    concurrency::{sleep, Duration},
    Actor, ActorProcessingErr, ActorRef, RpcReplyPort,
};

use crate::ckb_chain::contracts::{get_script_by_contract, Contract};

use super::{funding::FundingContext, CkbChainConfig, FundingError, FundingRequest, FundingTx};

pub struct CkbChainActor {}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct CkbChainState {
    config: CkbChainConfig,
    secret_key: secp256k1::SecretKey,
    funding_source_lock_script: packed::Script,
}

#[derive(Debug, Clone)]
pub struct TraceTxRequest {
    pub tx_hash: packed::Byte32,
    // How many confirmations required to consider the transaction committed.
    pub confirmations: u64,
}

#[derive(Debug)]
pub enum CkbChainMessage {
    Fund(
        FundingTx,
        FundingRequest,
        RpcReplyPort<Result<(FundingTx, Script), FundingError>>,
    ),
    Sign(FundingTx, RpcReplyPort<Result<FundingTx, FundingError>>),
    SendTx(TransactionView, RpcReplyPort<Result<(), RpcError>>),
    TraceTx(TraceTxRequest, RpcReplyPort<ckb_jsonrpc_types::Status>),
}

#[ractor::async_trait]
impl Actor for CkbChainActor {
    type Msg = CkbChainMessage;
    type State = CkbChainState;
    type Arguments = CkbChainConfig;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        config: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let secret_key = config.read_secret_key()?;
        let secp = secp256k1::Secp256k1::new();
        let pub_key = secret_key.public_key(&secp);
        let pub_key_hash = ckb_hash::blake2b_256(pub_key.serialize());
        let funding_source_lock_script =
            get_script_by_contract(Contract::Secp256k1Lock, &pub_key_hash[0..20]);
        tracing::info!(
            "[{}] funding lock args: {}",
            myself.get_name().unwrap_or_default(),
            funding_source_lock_script.args()
        );

        Ok(CkbChainState {
            config,
            secret_key,
            funding_source_lock_script,
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        use CkbChainMessage::{Fund, SendTx, Sign, TraceTx};
        match message {
            Fund(tx, request, reply_port) => {
                let context = state.build_funding_context(&request);
                if !reply_port.is_closed() {
                    tokio::task::block_in_place(move || {
                        let result = tx.fulfill(request, context);
                        if !reply_port.is_closed() {
                            reply_port.send(result).expect("reply ok");
                        }
                    });
                }
            }
            Sign(tx, reply_port) => {
                if !reply_port.is_closed() {
                    let secret_key = state.secret_key;
                    let rpc_url = state.config.rpc_url.clone();
                    tokio::task::block_in_place(move || {
                        let result = tx.sign(secret_key, rpc_url);
                        if !reply_port.is_closed() {
                            reply_port.send(result).expect("reply ok");
                        }
                    });
                }
            }
            SendTx(tx, reply_port) => {
                let rpc_url = state.config.rpc_url.clone();
                tokio::task::block_in_place(move || {
                    let ckb_client = CkbRpcClient::new(&rpc_url);
                    let result = match ckb_client.send_transaction(tx.data().into(), None) {
                        Ok(_) => Ok(()),
                        Err(err) => {
                            //FIXME(yukang): RBF or duplicated transaction handling
                            match err {
                                RpcError::Rpc(e)
                                    if (e.code.code() == -1107 || e.code.code() == -1111) =>
                                {
                                    tracing::warn!(
                                        "[{}] transaction { } already in pool",
                                        myself.get_name().unwrap_or_default(),
                                        tx.hash(),
                                    );
                                    Ok(())
                                }
                                _ => {
                                    tracing::error!(
                                        "[{}] send transaction {} failed: {:?}",
                                        myself.get_name().unwrap_or_default(),
                                        tx.hash(),
                                        err
                                    );
                                    Err(err)
                                }
                            }
                        }
                    };
                    if !reply_port.is_closed() {
                        reply_port.send(result).expect("reply ok");
                    }
                });
            }
            TraceTx(
                TraceTxRequest {
                    tx_hash,
                    confirmations,
                },
                reply_port,
            ) => {
                tracing::info!(
                    "[{}] trace transaction {} with {} confs",
                    myself.get_name().unwrap_or_default(),
                    tx_hash,
                    confirmations
                );
                // TODO: Need a better way to trace the transaction.
                while !reply_port.is_closed() {
                    let actor_name = myself.get_name().unwrap_or_default();
                    let rpc_url = state.config.rpc_url.clone();
                    let tx_hash = tx_hash.clone();
                    let status = tokio::task::block_in_place(move || {
                        let ckb_client = CkbRpcClient::new(&rpc_url);
                        match ckb_client.get_transaction_status(tx_hash.unpack()) {
                            Ok(resp) => match resp.tx_status.status {
                                ckb_jsonrpc_types::Status::Committed => {
                                    match ckb_client.get_tip_block_number() {
                                        Ok(tip_number) => {
                                            let tip_number: u64 = tip_number.into();
                                            let commit_number: u64 = resp
                                                .tx_status
                                                .block_number
                                                .unwrap_or_default()
                                                .into();
                                            (tip_number >= commit_number + confirmations)
                                                .then_some(ckb_jsonrpc_types::Status::Committed)
                                        }
                                        Err(err) => {
                                            tracing::error!(
                                                "[{}] get tip block number failed: {:?}",
                                                actor_name,
                                                err
                                            );
                                            None
                                        }
                                    }
                                }
                                ckb_jsonrpc_types::Status::Rejected => {
                                    Some(ckb_jsonrpc_types::Status::Rejected)
                                }
                                _ => None,
                            },
                            Err(err) => {
                                tracing::error!(
                                    "[{}] get transaction status failed: {:?}",
                                    actor_name,
                                    err
                                );
                                None
                            }
                        }
                    });
                    match status {
                        Some(status) => {
                            if !reply_port.is_closed() {
                                reply_port.send(status).expect("reply ok");
                            }
                            return Ok(());
                        }
                        None => sleep(Duration::from_secs(5)).await,
                    }
                }
            }
        }
        Ok(())
    }
}

impl CkbChainState {
    fn build_funding_context(&self, request: &FundingRequest) -> FundingContext {
        FundingContext {
            secret_key: self.secret_key,
            rpc_url: self.config.rpc_url.clone(),
            funding_source_lock_script: self.funding_source_lock_script.clone(),
            funding_cell_lock_script: request.script.clone(),
        }
    }
}

#[cfg(test)]
pub use test_utils::{submit_tx, trace_tx, MockChainActor};

#[cfg(test)]
mod test_utils {
    use std::collections::HashMap;

    use anyhow::anyhow;
    use ckb_types::{
        core::TransactionView,
        packed::{CellOutput, OutPoint},
        prelude::{Builder, Entity, Pack, PackVec, Unpack},
    };

    use crate::ckb_chain::TraceTxRequest;

    use super::super::contracts::MockContext;
    use super::CkbChainMessage;

    use ckb_types::packed::Byte32;
    use ractor::{call_t, Actor, ActorProcessingErr, ActorRef};
    use tracing::{debug, error};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CellStatus {
        // This cell has been consumed. If any transaction
        // tries to consume the same cell, it should be rejected.
        Consumed,
    }

    pub struct MockChainActorState {
        ctx: MockContext,
        tx_status: HashMap<Byte32, ckb_jsonrpc_types::Status>,
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
                            .capacity(
                                (request.local_amount + request.local_reserved_ckb_amount).pack(),
                            )
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

                    let funding_source_lock_script = request.script;

                    if let Err(e) = reply_port.send(Ok((fulfilled_tx, funding_source_lock_script)))
                    {
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
                    let mut context = state.ctx.write();
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
                    state.tx_status.insert(tx.hash(), status);
                    if let Err(e) = reply_port.send(result) {
                        error!(
                            "[{}] send reply failed: {:?}",
                            myself.get_name().unwrap_or_default(),
                            e
                        );
                    }
                }
                TraceTx(tx, reply_port) => {
                    let status = state
                        .tx_status
                        .get(&tx.tx_hash)
                        .cloned()
                        .unwrap_or(ckb_jsonrpc_types::Status::Unknown);
                    debug!(
                        "Tracing transaction: {:?}, status: {:?}",
                        &tx.tx_hash, &status
                    );
                    if let Err(e) = reply_port.send(status) {
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
        pub const TIMEOUT: u64 = 1000;
        let tx_hash = tx.hash();
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
    }
}

#[cfg(test)]
mod test {
    use ckb_jsonrpc_types::Status;
    use ckb_types::core::TransactionView;
    use ckb_types::packed::{CellInput, CellOutput};
    use ckb_types::prelude::{Builder, Pack};
    use molecule::prelude::Entity;
    use ractor::{Actor, ActorRef};

    use super::super::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};
    use super::test_utils::submit_tx;
    use super::CkbChainMessage;

    async fn create_mock_chain_actor() -> ActorRef<CkbChainMessage> {
        use super::test_utils::MockChainActor;
        Actor::spawn(None, MockChainActor::new(), ())
            .await
            .expect("start mock chain actor")
            .0
    }

    #[tokio::test]
    async fn test_submit_empty_tx() {
        let actor = create_mock_chain_actor().await;
        assert_eq!(
            submit_tx(actor, TransactionView::new_advanced_builder().build()).await,
            Status::Committed
        );
    }

    #[tokio::test]
    async fn test_submit_one_output_tx() {
        let actor = create_mock_chain_actor().await;
        assert_eq!(
            submit_tx(
                actor,
                TransactionView::new_advanced_builder()
                    .output(CellOutput::default())
                    .output_data(Default::default())
                    .build()
            )
            .await,
            Status::Committed
        );
    }

    #[tokio::test]
    async fn test_submit_always_success_tx() {
        let actor = create_mock_chain_actor().await;
        let capacity = 100u64;
        let output = CellOutput::new_builder()
            .capacity(capacity.pack())
            .lock(get_script_by_contract(
                Contract::AlwaysSuccess,
                &b"whatever1"[..],
            ))
            .build();
        let tx = TransactionView::new_advanced_builder()
            .output(output)
            .output_data(Default::default())
            .build();
        assert_eq!(
            submit_tx(actor.clone(), tx.clone()).await,
            Status::Committed
        );
        let out_point = tx.output_pts_iter().next().unwrap();
        let tx = TransactionView::new_advanced_builder()
            .cell_deps(get_cell_deps_by_contracts(vec![Contract::AlwaysSuccess]))
            .input(
                CellInput::new_builder()
                    .previous_output(out_point.clone())
                    .build(),
            )
            .output(
                CellOutput::new_builder()
                    .capacity(capacity.pack())
                    .lock(get_script_by_contract(
                        Contract::FundingLock,
                        &b"whatever2"[..],
                    ))
                    .build(),
            )
            .output_data(Default::default())
            .build();
        assert_eq!(submit_tx(actor, tx).await, Status::Committed);
    }

    #[tokio::test]
    async fn test_repeatedly_consume_the_same_cell() {
        let actor = create_mock_chain_actor().await;
        let capacity = 100u64;
        let output = CellOutput::new_builder()
            .capacity(capacity.pack())
            .lock(get_script_by_contract(
                Contract::AlwaysSuccess,
                &b"whatever1"[..],
            ))
            .build();
        let tx = TransactionView::new_advanced_builder()
            .output(output)
            .output_data(Default::default())
            .build();
        assert_eq!(
            submit_tx(actor.clone(), tx.clone()).await,
            Status::Committed
        );
        let out_point = tx.output_pts_iter().next().unwrap();
        let tx = TransactionView::new_advanced_builder()
            .cell_deps(get_cell_deps_by_contracts(vec![Contract::AlwaysSuccess]))
            .input(
                CellInput::new_builder()
                    .previous_output(out_point.clone())
                    .build(),
            )
            .output(
                CellOutput::new_builder()
                    .capacity(capacity.pack())
                    .lock(get_script_by_contract(
                        Contract::FundingLock,
                        &b"whatever2"[..],
                    ))
                    .build(),
            )
            .output_data(Default::default())
            .build();
        assert_eq!(submit_tx(actor.clone(), tx).await, Status::Committed);
        let tx = TransactionView::new_advanced_builder()
            .cell_deps(get_cell_deps_by_contracts(vec![Contract::AlwaysSuccess]))
            .input(
                CellInput::new_builder()
                    .previous_output(out_point.clone())
                    .build(),
            )
            .output(
                CellOutput::new_builder()
                    .capacity(capacity.pack())
                    .lock(get_script_by_contract(
                        Contract::FundingLock,
                        &b"whatever3"[..],
                    ))
                    .build(),
            )
            .output_data(Default::default())
            .build();
        assert_eq!(submit_tx(actor, tx).await, Status::Rejected);
    }

    #[tokio::test]
    async fn test_submit_malformed_commitment_tx() {
        let actor = create_mock_chain_actor().await;
        let capacity = 100u64;
        let output = CellOutput::new_builder()
            .capacity(capacity.pack())
            .lock(get_script_by_contract(
                Contract::FundingLock,
                &b"whatever1"[..],
            ))
            .build();
        let tx = TransactionView::new_advanced_builder()
            .output(output)
            .output_data(Default::default())
            .build();
        assert_eq!(
            submit_tx(actor.clone(), tx.clone()).await,
            Status::Committed
        );
        let out_point = tx.output_pts_iter().next().unwrap();
        let tx = TransactionView::new_advanced_builder()
            .cell_deps(get_cell_deps_by_contracts(vec![Contract::FundingLock]))
            .input(
                CellInput::new_builder()
                    .previous_output(out_point.clone())
                    .build(),
            )
            .output(
                CellOutput::new_builder()
                    .capacity(capacity.pack())
                    .lock(get_script_by_contract(
                        Contract::CommitmentLock,
                        &b"whatever2"[..],
                    ))
                    .build(),
            )
            .output_data(Default::default())
            .build();
        assert_eq!(submit_tx(actor, tx).await, Status::Rejected);
    }
}
