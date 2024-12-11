use ckb_sdk::{rpc::ResponseFormatGetter, CkbRpcClient, RpcError};
use ckb_types::{core::TransactionView, packed, prelude::*, H256};
use ractor::{
    concurrency::{sleep, Duration},
    Actor, ActorProcessingErr, ActorRef, RpcReplyPort,
};

use crate::ckb::contracts::{get_script_by_contract, Contract};

use super::{funding::FundingContext, CkbConfig, FundingError, FundingRequest, FundingTx};

pub struct CkbChainActor {}

#[derive(Clone, Debug)]
pub struct CkbChainState {
    config: CkbConfig,
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
pub struct TraceTxResponse {
    pub tx: Option<ckb_jsonrpc_types::TransactionView>,
    pub status: ckb_jsonrpc_types::TxStatus,
}

impl TraceTxResponse {
    pub fn new(
        tx: Option<ckb_jsonrpc_types::TransactionView>,
        status: ckb_jsonrpc_types::TxStatus,
    ) -> Self {
        Self { tx, status }
    }
}

#[derive(Debug, Clone)]
pub struct GetBlockTimestampRequest {
    block_hash: H256,
}

impl GetBlockTimestampRequest {
    pub fn from_block_hash(block_hash: H256) -> Self {
        Self { block_hash }
    }

    pub fn block_hash(&self) -> H256 {
        self.block_hash.clone()
    }
}

pub type GetBlockTimestampResponse = u64;

#[derive(Debug)]
pub enum CkbChainMessage {
    Fund(
        FundingTx,
        FundingRequest,
        RpcReplyPort<Result<FundingTx, FundingError>>,
    ),
    Sign(FundingTx, RpcReplyPort<Result<FundingTx, FundingError>>),
    SendTx(TransactionView, RpcReplyPort<Result<(), RpcError>>),
    TraceTx(TraceTxRequest, RpcReplyPort<TraceTxResponse>),
    GetBlockTimestamp(
        GetBlockTimestampRequest,
        RpcReplyPort<Result<Option<GetBlockTimestampResponse>, RpcError>>,
    ),
}

#[ractor::async_trait]
impl Actor for CkbChainActor {
    type Msg = CkbChainMessage;
    type State = CkbChainState;
    type Arguments = CkbConfig;

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
                            // ignore error
                            let _ = reply_port.send(result);
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
                            // ignore error
                            let _ = reply_port.send(result);
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
                        // ignore error
                        let _ = reply_port.send(result);
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
                        // FIXME: `get_transaction_status` is only called with verbosity = 1 in sdk now
                        match ckb_client.get_only_committed_transaction(tx_hash.unpack()) {
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
                                            let transaction = match resp
                                                .transaction
                                                .map(|x| x.get_value())
                                                .transpose()
                                            {
                                                Ok(Some(tx)) => Some(tx),
                                                Ok(None) => None,
                                                Err(err) => {
                                                    tracing::error!(
                                                        "[{}] get transaction failed: {:?}",
                                                        actor_name,
                                                        err
                                                    );
                                                    None
                                                }
                                            };
                                            (tip_number >= commit_number + confirmations).then_some(
                                                TraceTxResponse::new(transaction, resp.tx_status),
                                            )
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
                                    Some(TraceTxResponse::new(None, resp.tx_status))
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
                                // ignore error
                                let _ = reply_port.send(status);
                            }
                            return Ok(());
                        }
                        None => sleep(Duration::from_secs(5)).await,
                    }
                }
            }
            CkbChainMessage::GetBlockTimestamp(
                GetBlockTimestampRequest { block_hash },
                reply_port,
            ) => {
                let rpc_url = state.config.rpc_url.clone();
                tokio::task::block_in_place(move || {
                    let ckb_client = CkbRpcClient::new(&rpc_url);
                    let _ = reply_port.send(
                        ckb_client
                            .get_block(block_hash)
                            .map(|x| x.map(|x| x.header.inner.timestamp.into())),
                    );
                });
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
