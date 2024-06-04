use ckb_sdk::{CkbRpcClient, RpcError};
use ckb_types::{core::TransactionView, packed, prelude::*};
use ractor::{
    concurrency::{sleep, Duration},
    Actor, ActorProcessingErr, ActorRef, RpcReplyPort,
};

use crate::ckb::chain::CommitmentLockContext;

use super::{funding::FundingContext, CkbChainConfig, FundingError, FundingRequest, FundingTx};

pub struct CkbChainActor {}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct CkbChainState {
    config: CkbChainConfig,
    secret_key: secp256k1::SecretKey,
    funding_source_lock_script: packed::Script,
    ctx: CommitmentLockContext,
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
        RpcReplyPort<Result<FundingTx, FundingError>>,
    ),
    Sign(FundingTx, RpcReplyPort<Result<FundingTx, FundingError>>),
    SendTx(TransactionView),
    TraceTx(TraceTxRequest, RpcReplyPort<ckb_jsonrpc_types::Status>),
}

#[ractor::async_trait]
impl Actor for CkbChainActor {
    type Msg = CkbChainMessage;
    type State = CkbChainState;
    type Arguments = (CkbChainConfig, CommitmentLockContext);

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (config, ctx): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let secret_key = config.read_secret_key()?;

        let secp = secp256k1::Secp256k1::new();
        let pub_key = secret_key.public_key(&secp);
        let pub_key_hash = ckb_hash::blake2b_256(pub_key.serialize());
        let funding_source_lock_script = ctx.get_secp256k1_lock_script(&pub_key_hash[0..20]);
        log::info!(
            "[{}] funding lock args: {}",
            myself.get_name().unwrap_or_default(),
            funding_source_lock_script.args()
        );

        Ok(CkbChainState {
            config,
            secret_key,
            funding_source_lock_script,
            ctx,
        })
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
                    let secret_key = state.secret_key.clone();
                    let rpc_url = state.config.rpc_url.clone();
                    tokio::task::block_in_place(move || {
                        let result = tx.sign(secret_key, rpc_url);
                        if !reply_port.is_closed() {
                            reply_port.send(result).expect("reply ok");
                        }
                    });
                }
            }
            SendTx(tx) => {
                let rpc_url = state.config.rpc_url.clone();
                tokio::task::block_in_place(move || {
                    let ckb_client = CkbRpcClient::new(&rpc_url);
                    if let Err(err) = ckb_client.send_transaction(tx.data().into(), None) {
                        //FIXME(yukang): RBF or duplicated transaction handling
                        match err {
                            RpcError::Rpc(e)
                                if (e.code.code() == -1107 || e.code.code() == -1111) =>
                            {
                                log::warn!(
                                    "[{}] transaction already in pool",
                                    myself.get_name().unwrap_or_default()
                                );
                            }
                            _ => {
                                log::error!(
                                    "[{}] send transaction failed: {:?}",
                                    myself.get_name().unwrap_or_default(),
                                    err
                                );
                            }
                        }
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
                log::info!(
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
                                            log::error!(
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
                                log::error!(
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
            secret_key: self.secret_key.clone(),
            rpc_url: self.config.rpc_url.clone(),
            funding_source_lock_script: self.funding_source_lock_script.clone(),
            funding_cell_lock_script: request.script.clone(),
        }
    }
}

#[cfg(test)]
pub use test_utils::MockChainActor;

#[cfg(test)]
mod test_utils {
    use std::collections::HashMap;

    use ckb_types::{
        packed::CellOutput,
        prelude::{Builder, Entity, Pack, PackVec, Unpack},
    };

    use super::CkbChainMessage;
    use crate::ckb::chain::MockContext;

    use ckb_types::packed::Byte32;
    use log::{debug, error};
    use ractor::{Actor, ActorProcessingErr, ActorRef};

    pub struct MockChainActorState {
        ctx: MockContext,
        committed_tx_status: HashMap<Byte32, ckb_jsonrpc_types::Status>,
    }

    impl MockChainActorState {
        pub fn new() -> Self {
            Self {
                ctx: MockContext::new(),
                committed_tx_status: HashMap::new(),
            }
        }
    }

    pub struct MockChainActor {}

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
            ctx: &mut Self::State,
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
                            let capacity = request.local_amount + current_capacity;
                            let mut outputs_builder = outputs.as_builder();

                            outputs_builder
                                .replace(0, output.as_builder().capacity(capacity.pack()).build());
                            outputs_builder.build()
                        }
                        None => [CellOutput::new_builder()
                            .capacity(request.local_amount.pack())
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
                    // TODO: Fill in transaction from request.
                    let signed_tx = tx.clone();
                    debug!(
                        "signing transaction: original tx: {:?}, signed tx: {:?}",
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
                SendTx(tx) => {
                    debug!("Sending transaction: {:?}", tx);
                    // TODO: verify the transaction and set the relevant status.
                    let status = ckb_jsonrpc_types::Status::Committed;
                    debug!("Verified transaction: {:?}, status: {:?}", tx, status);
                    ctx.committed_tx_status.insert(tx.hash(), status);
                }
                TraceTx(tx, reply_port) => {
                    let status = ctx
                        .committed_tx_status
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
}
