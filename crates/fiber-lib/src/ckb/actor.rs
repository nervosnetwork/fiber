use ckb_sdk::{CkbRpcAsyncClient, RpcError};
use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::{self, Transaction},
    prelude::IntoTransactionView as _,
};
use ractor::{concurrency::Duration, Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use tracing::debug;

use crate::{
    ckb::contracts::{get_script_by_contract, Contract},
    fiber::types::Hash256,
};

use super::{
    funding::{FundingContext, LiveCellsExclusionMap},
    jsonrpc_types_convert::{transaction_view_from_json, tx_status_from_json},
    tx_tracing_actor::{
        CkbTxTracer, CkbTxTracingActor, CkbTxTracingArguments, CkbTxTracingMessage,
    },
    CkbConfig, FundingError, FundingRequest, FundingTx,
};

pub struct CkbChainActor {}

#[derive(Clone, Debug)]
pub struct CkbChainState {
    config: CkbConfig,
    ckb_tx_tracing_actor: ActorRef<CkbTxTracingMessage>,
    secret_key: secp256k1::SecretKey,
    funding_source_lock_script: packed::Script,
    live_cells_exclusion_map: LiveCellsExclusionMap,
}

#[derive(Debug, Clone)]
pub struct GetBlockTimestampRequest {
    block_hash: Hash256,
}

impl GetBlockTimestampRequest {
    pub fn from_block_hash(block_hash: Hash256) -> Self {
        Self { block_hash }
    }

    pub fn block_hash(&self) -> Hash256 {
        self.block_hash
    }
}

pub type GetBlockTimestampResponse = u64;

#[derive(Debug, Clone)]
pub struct GetTxResponse {
    /// The transaction.
    pub transaction: Option<TransactionView>,
    pub tx_status: TxStatus,
}

impl Default for GetTxResponse {
    fn default() -> Self {
        Self {
            transaction: None,
            tx_status: TxStatus::Unknown,
        }
    }
}

impl From<Option<ckb_jsonrpc_types::TransactionWithStatusResponse>> for GetTxResponse {
    fn from(value: Option<ckb_jsonrpc_types::TransactionWithStatusResponse>) -> Self {
        match value {
            Some(response) => Self {
                transaction: response.transaction.map(|tx| match tx.inner {
                    ckb_jsonrpc_types::Either::Left(json) => transaction_view_from_json(json),
                    ckb_jsonrpc_types::Either::Right(_) => {
                        panic!("bytes response format not used");
                    }
                }),
                tx_status: tx_status_from_json(response.tx_status),
            },
            None => Self::default(),
        }
    }
}

#[derive(Debug)]
pub enum CkbChainMessage {
    Fund(
        FundingTx,
        FundingRequest,
        RpcReplyPort<Result<FundingTx, FundingError>>,
    ),
    VerifyFundingTx {
        local_tx: Transaction,
        remote_tx: Transaction,
        reply: RpcReplyPort<Result<(), FundingError>>,
    },
    /// Add funding tx. This is used to reestablish a channel that is not ready yet.
    /// Adding a funding tx will add its used input cells to the exclusion list.
    AddFundingTx(FundingTx),
    /// Remove a funding tx to release the used live cells.
    /// RemoveFundingTx(tx_hash)
    RemoveFundingTx(Hash256),
    /// Notify that the funding tx has been committed and the used live cells will become dead soon.
    /// CommitFundingTx(tx_hash, commit_block_number),
    CommitFundingTx(Hash256, u64),
    Sign(FundingTx, RpcReplyPort<Result<FundingTx, FundingError>>),
    SendTx(TransactionView, RpcReplyPort<Result<(), RpcError>>),
    GetTx(Hash256, RpcReplyPort<Result<GetTxResponse, RpcError>>),
    CreateTxTracer(CkbTxTracer),
    RemoveTxTracers(Hash256),
    GetBlockTimestamp(
        GetBlockTimestampRequest,
        RpcReplyPort<Result<Option<GetBlockTimestampResponse>, RpcError>>,
    ),
    Stop,
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
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
        let ckb_tx_tracing_actor = Actor::spawn_linked(
            Some(format!(
                "{}/ckb-tx-tracing",
                myself.get_name().as_deref().unwrap_or_default()
            )),
            CkbTxTracingActor::new(),
            CkbTxTracingArguments {
                rpc_url: config.rpc_url.clone(),
                polling_interval: Duration::from_millis(config.tx_tracing_polling_interval_ms),
            },
            myself.into(),
        )
        .await?
        .0;
        Ok(CkbChainState {
            config,
            secret_key,
            funding_source_lock_script,
            ckb_tx_tracing_actor,
            live_cells_exclusion_map: Default::default(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CkbChainMessage::Fund(tx, request, reply_port) => {
                if !reply_port.is_closed() {
                    let context = state.build_funding_context(&request);
                    let exclusion = &mut state.live_cells_exclusion_map;
                    let result = tx.fulfill(request, context, exclusion).await;
                    if !reply_port.is_closed() {
                        // ignore error
                        let _ = reply_port.send(result);
                    }
                }
            }
            CkbChainMessage::VerifyFundingTx {
                local_tx,
                remote_tx,
                reply,
            } => {
                let mut funding_tx: FundingTx = local_tx.into();
                let result = funding_tx.update_for_peer(remote_tx.into_view());
                let _ = reply.send(result);
            }
            CkbChainMessage::AddFundingTx(tx) => {
                state.live_cells_exclusion_map.add_funding_tx(&tx);
            }
            CkbChainMessage::RemoveFundingTx(tx_hash) => {
                state.live_cells_exclusion_map.remove(&tx_hash.into());
            }
            CkbChainMessage::CommitFundingTx(tx_hash, commit_block_number) => {
                state
                    .live_cells_exclusion_map
                    .commit(&tx_hash.into(), commit_block_number);
            }
            CkbChainMessage::Sign(tx, reply_port) => {
                if !reply_port.is_closed() {
                    let secret_key = state.secret_key;
                    let rpc_url = state.config.rpc_url.clone();
                    let result = tx.sign(secret_key, rpc_url).await;
                    if !reply_port.is_closed() {
                        // ignore error
                        let _ = reply_port.send(result);
                    }
                }
            }
            CkbChainMessage::SendTx(tx, reply_port) => {
                let rpc_url = state.config.rpc_url.clone();
                let ckb_client = CkbRpcAsyncClient::new(&rpc_url);
                let result = match ckb_client.send_transaction(tx.data().into(), None).await {
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
            }
            CkbChainMessage::GetTx(tx_hash, reply_port) => {
                let rpc_url = state.config.rpc_url.clone();
                let ckb_client = CkbRpcAsyncClient::new(&rpc_url);
                let result = ckb_client.get_transaction(tx_hash.into()).await;
                if !reply_port.is_closed() {
                    // ignore error
                    let _ = reply_port.send(result.map(Into::into));
                }
            }
            CkbChainMessage::CreateTxTracer(tracer) => {
                debug!(
                    "[{}] trace transaction {} with {} confs",
                    myself.get_name().unwrap_or_default(),
                    tracer.tx_hash,
                    tracer.confirmations
                );
                state
                    .ckb_tx_tracing_actor
                    .send_message(CkbTxTracingMessage::CreateTracer(tracer))?;
            }
            CkbChainMessage::RemoveTxTracers(tx_hash) => {
                state
                    .ckb_tx_tracing_actor
                    .send_message(CkbTxTracingMessage::RemoveTracers(tx_hash))?;
            }
            CkbChainMessage::GetBlockTimestamp(
                GetBlockTimestampRequest { block_hash },
                reply_port,
            ) => {
                let rpc_url = state.config.rpc_url.clone();
                let ckb_client = CkbRpcAsyncClient::new(&rpc_url);
                let _ = reply_port.send(
                    ckb_client
                        .get_header(block_hash.into())
                        .await
                        .map(|x| x.map(|x| x.inner.timestamp.into())),
                );
            }
            CkbChainMessage::Stop => {
                myself.stop(Some("stop received".to_string()));
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
