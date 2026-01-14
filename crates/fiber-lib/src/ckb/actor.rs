use ckb_sdk::RpcError;
use ckb_types::{core::TransactionView, packed, prelude::IntoTransactionView as _};
use ractor::{concurrency::Duration, Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use strum::AsRefStr;
use tracing::debug;

use crate::{
    ckb::contracts::{get_script_by_contract, Contract},
    fiber::{serde_utils::EntityHex, types::Hash256},
    utils::actor::ActorHandleLogGuard,
};

use super::{
    funding::{FundingContext, LiveCellsExclusionMap},
    tx_tracing_actor::{
        CkbTxTracer, CkbTxTracingActor, CkbTxTracingArguments, CkbTxTracingMessage,
    },
    CkbConfig, FundingError, FundingRequest, FundingTx,
};

pub struct CkbChainActor {}

const ACTOR_HANDLE_WARN_THRESHOLD_MS: u64 = 15_000;

#[derive(Clone, Debug)]
pub struct CkbChainState {
    config: CkbConfig,
    ckb_tx_tracing_actor: ActorRef<CkbTxTracingMessage>,
    secret_key: secp256k1::SecretKey,
    funding_source_lock_script: packed::Script,
    live_cells_exclusion_map: LiveCellsExclusionMap,
}

#[derive(Debug, AsRefStr)]
pub enum CkbChainMessage {
    Fund(
        FundingTx,
        FundingRequest,
        RpcReplyPort<Result<FundingTx, FundingError>>,
    ),
    VerifyFundingTx {
        local_tx: packed::Transaction,
        remote_tx: packed::Transaction,
        funding_cell_lock_script: packed::Script,
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
    CreateTxTracer(CkbTxTracer),
    RemoveTxTracers(Hash256),

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
        let _handle_log_guard = ActorHandleLogGuard::new(
            "CkbChainActor",
            message.as_ref().to_string(),
            "fiber.ckb_chain_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            CkbChainMessage::Fund(tx, request, reply_port) => {
                let context = state.build_funding_context(request.script.clone());
                let result = match state.config.funding_tx_shell_builder_as_deref() {
                    None => {
                        tx.fulfill(request, context, &mut state.live_cells_exclusion_map)
                            .await
                    }
                    Some(shell_script) => fund_via_shell(shell_script, tx, request, context).await,
                };
                let _ = reply_port.send(result);
            }
            CkbChainMessage::VerifyFundingTx {
                local_tx,
                remote_tx,
                funding_cell_lock_script,
                reply,
            } => {
                let mut funding_tx: FundingTx = local_tx.into();
                let context = state.build_funding_context(funding_cell_lock_script);
                let result = funding_tx
                    .update_for_peer(remote_tx.into_view(), context)
                    .await;
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
                let ckb_client = state.config.ckb_rpc_client();
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

            CkbChainMessage::Stop => {
                myself.stop(Some("stop received".to_string()));
            }
        }

        Ok(())
    }
}

impl CkbChainState {
    fn build_funding_context(&self, funding_cell_lock_script: packed::Script) -> FundingContext {
        FundingContext {
            secret_key: self.secret_key,
            rpc_url: self.config.rpc_url.clone(),
            funding_source_lock_script: self.funding_source_lock_script.clone(),
            funding_cell_lock_script,
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FundingTxShellBuilderInput {
    tx: ckb_jsonrpc_types::Transaction,
    request: FundingRequest,
    rpc_url: String,
    #[serde_as(as = "EntityHex")]
    funding_source_lock_script: packed::Script,
}

#[cfg(not(target_arch = "wasm32"))]
async fn fund_via_shell(
    shell_script: &str,
    mut tx: FundingTx,
    request: FundingRequest,
    context: FundingContext,
) -> Result<FundingTx, FundingError> {
    use std::process::Stdio;
    use tokio::{io::AsyncWriteExt, process::Command};
    let (executable, arg) = if cfg!(target_os = "windows") {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut child = Command::new(executable)
        .arg(arg)
        .arg(shell_script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let input = FundingTxShellBuilderInput {
        tx: tx.take().map(|tx| tx.data().into()).unwrap_or_default(),
        request,
        rpc_url: context.rpc_url,
        funding_source_lock_script: context.funding_source_lock_script,
    };
    let mut stdin = child.stdin.take().expect("failed to get stdin");
    let input_json = serde_json::to_string(&input)?;
    stdin.write_all(input_json.as_bytes()).await?;

    let output = child.wait_with_output().await?;
    if output.status.success() {
        let out_tx_json = String::from_utf8(output.stdout)?;
        let tx: ckb_jsonrpc_types::Transaction = serde_json::from_str(&out_tx_json)?;
        let tx: packed::Transaction = tx.into();
        let tx: FundingTx = tx.into_view().into();
        Ok(tx)
    } else {
        let err = String::from_utf8(output.stderr)?;
        Err(FundingError::CkbTxBuilderError(
            ckb_sdk::tx_builder::TxBuilderError::Other(anyhow::anyhow!(err)),
        ))
    }
}

#[cfg(target_arch = "wasm32")]
async fn fund_via_shell(
    _shell_script: &str,
    _tx: FundingTx,
    _request: FundingRequest,
    _context: FundingContext,
) -> Result<FundingTx, FundingError> {
    // Never called in WASM
    unreachable!();
}
