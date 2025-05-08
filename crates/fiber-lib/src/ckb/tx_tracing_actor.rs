use std::collections::HashMap;

use bitmask_enum::bitmask;
use ckb_sdk::CkbRpcClient;
use ckb_types::core::tx_pool::TxStatus;
use ractor::{concurrency::Duration, Actor, ActorProcessingErr, ActorRef, RpcReplyPort};

use crate::fiber::types::Hash256;

use super::jsonrpc_types_convert::tx_status_from_json;

#[derive(Debug, Clone)]
pub struct CkbTxTracingResult {
    pub tx_hash: Hash256,
    pub tx_status: TxStatus,
}

impl CkbTxTracingResult {
    pub fn unknown(tx_hash: Hash256) -> Self {
        Self {
            tx_hash,
            tx_status: TxStatus::Unknown,
        }
    }
}

pub struct CkbTxTracingActor {}

impl CkbTxTracingActor {
    pub fn new() -> Self {
        Self {}
    }
}

pub struct CkbTxTracingArguments {
    pub rpc_url: String,
    /// Tracer uses polling now. It can be improved later to use a pushing mechanism.
    pub polling_interval: Duration,
}

#[bitmask(u8)]
pub enum CkbTxTracingMask {
    Pending = 1,
    Proposed = 2,
    Committed = 4,
    Unknown = 8,
    Rejected = 16,
}

impl From<&TxStatus> for CkbTxTracingMask {
    fn from(status: &TxStatus) -> Self {
        match status {
            TxStatus::Pending => CkbTxTracingMask::Pending,
            TxStatus::Proposed => CkbTxTracingMask::Proposed,
            TxStatus::Committed(..) => CkbTxTracingMask::Committed,
            TxStatus::Unknown => CkbTxTracingMask::Unknown,
            TxStatus::Rejected(_) => CkbTxTracingMask::Rejected,
        }
    }
}

#[derive(Debug)]
pub struct CkbTxTracer {
    pub tx_hash: Hash256,
    pub confirmations: u64,
    pub mask: CkbTxTracingMask,
    pub callback: RpcReplyPort<CkbTxTracingResult>,
}

#[derive(Debug)]
struct CkbTxTracersForTheSameTx {
    last_status: TxStatus,
    last_block: u64,
    tracers: Vec<CkbTxTracer>,
}

impl Default for CkbTxTracersForTheSameTx {
    fn default() -> Self {
        Self {
            last_status: TxStatus::Unknown,
            last_block: 0,
            tracers: Default::default(),
        }
    }
}

#[derive(Debug)]
pub struct CkbTxTracingState {
    rpc_url: String,
    tracers: HashMap<Hash256, CkbTxTracersForTheSameTx>,
}

#[derive(Debug)]
pub enum InternalMessage {
    RunTracers,
    ReportTracingResult(CkbTxTracingResult, u64),
}

#[derive(Debug)]
pub enum CkbTxTracingMessage {
    CreateTracer(CkbTxTracer),
    RemoveTracers(Hash256),

    Internal(InternalMessage),
}

impl CkbTxTracingMessage {
    fn run_tracers() -> Self {
        Self::Internal(InternalMessage::RunTracers)
    }

    fn report_tracing_result(result: CkbTxTracingResult, tip_block_number: u64) -> Self {
        Self::Internal(InternalMessage::ReportTracingResult(
            result,
            tip_block_number,
        ))
    }
}

#[ractor::async_trait]
impl Actor for CkbTxTracingActor {
    type Msg = CkbTxTracingMessage;
    type State = CkbTxTracingState;
    type Arguments = CkbTxTracingArguments;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        arguments: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        myself.send_interval(arguments.polling_interval, CkbTxTracingMessage::run_tracers);
        Ok(Self::State {
            rpc_url: arguments.rpc_url,
            tracers: Default::default(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        use CkbTxTracingMessage::{CreateTracer, Internal, RemoveTracers};

        match message {
            CreateTracer(arguments) => state.create_tracer(myself, arguments).await,
            RemoveTracers(arguments) => state.remove_tracers(arguments),
            Internal(InternalMessage::RunTracers) => state.run_tracers(myself, None).await,
            Internal(InternalMessage::ReportTracingResult(result, tip_block_number)) => {
                state
                    .report_tracing_result(myself, result, tip_block_number)
                    .await
            }
        }
    }
}

struct TracingTask {
    actor: ActorRef<CkbTxTracingMessage>,
    rpc_url: String,
    tx_hashes: Vec<Hash256>,
}

impl CkbTxTracingState {
    async fn create_tracer(
        &mut self,
        myself: ActorRef<CkbTxTracingMessage>,
        tracer: CkbTxTracer,
    ) -> Result<(), ActorProcessingErr> {
        let tx_hash = tracer.tx_hash;
        let tx_tracers = self.tracers.entry(tx_hash).or_default();
        tx_tracers.tracers.push(tracer);

        if tx_tracers.tracers.len() > 1 {
            // If there's no existing tracer for the tx, trigger the tracing task now
            self.run_tracers(myself, Some(vec![tx_hash])).await?
        }

        Ok(())
    }

    fn remove_tracers(&mut self, tx_hash: Hash256) -> Result<(), ActorProcessingErr> {
        self.tracers.remove(&tx_hash);
        Ok(())
    }

    /// Run the tracers to check the txs statuses.
    ///
    /// When `tx_hashes` is None, all tracers are checked. Otherwise only the specified tx hashes are checked.
    async fn run_tracers(
        &mut self,
        myself: ActorRef<CkbTxTracingMessage>,
        tx_hashes: Option<Vec<Hash256>>,
    ) -> Result<(), ActorProcessingErr> {
        let tx_hashes = tx_hashes.unwrap_or_else(|| self.tracers.keys().cloned().collect());
        if !tx_hashes.is_empty() {
            let task = TracingTask {
                actor: myself,
                rpc_url: self.rpc_url.clone(),
                tx_hashes,
            };
            task.spawn();
        }
        Ok(())
    }

    async fn report_tracing_result(
        &mut self,
        _myself: ActorRef<CkbTxTracingMessage>,
        result: CkbTxTracingResult,
        tip_block_number: u64,
    ) -> Result<(), ActorProcessingErr> {
        let tx_tracers = if let Some(tx_tracers) = self.tracers.get_mut(&result.tx_hash) {
            tx_tracers
        } else {
            return Ok(());
        };

        let tx_status = result.tx_status.clone();

        match tx_status {
            TxStatus::Committed(block_number, ..) => {
                // Always use the latest committed block number
                tx_tracers.last_block = block_number;
            }
            _ => {
                // Otherwise, use tip block number when first entered this status
                if tx_tracers.last_block == 0 || tx_status != tx_tracers.last_status {
                    tx_tracers.last_block = tip_block_number
                }
            }
        }
        tx_tracers.last_status = tx_status.clone();

        // Leave only tracers not fired
        for i in (0..tx_tracers.tracers.len()).rev() {
            let tracer = &tx_tracers.tracers[i];
            if tracer.mask.contains((&tx_status).into())
                && tip_block_number.saturating_sub(tx_tracers.last_block) >= tracer.confirmations
            {
                let tracer = tx_tracers.tracers.swap_remove(i);
                // ignore the error if the receiver is dropped
                let _ = tracer.callback.send(result.clone());
            }
        }

        // Remove the empty entry for the tx
        if tx_tracers.tracers.is_empty() {
            self.tracers.remove(&result.tx_hash);
        }

        Ok(())
    }
}

impl TracingTask {
    fn spawn(self) {
        tokio::task::spawn_blocking(move || self.run());
    }

    fn run(self) {
        if let Err(err) = self.run_inner() {
            tracing::error!("Failed to run CKB tx tracing task: {:?}", err);
        }
    }

    fn run_inner(self) -> Result<(), Box<dyn std::error::Error>> {
        let ckb_client = CkbRpcClient::new(&self.rpc_url);
        let tip_block_number: u64 = ckb_client.get_tip_block_number()?.into();

        for tx_hash in self.tx_hashes {
            match ckb_client.get_transaction(tx_hash.into()) {
                Ok(response_opt) => {
                    let result = response_opt
                        .map(|response| CkbTxTracingResult {
                            tx_hash,
                            tx_status: tx_status_from_json(response.tx_status),
                        })
                        .unwrap_or_else(|| CkbTxTracingResult::unknown(tx_hash));
                    let _ = self
                        .actor
                        .send_message(CkbTxTracingMessage::report_tracing_result(
                            result,
                            tip_block_number,
                        ));
                }
                Err(err) => {
                    tracing::warn!("Failed to get CKB tx {}: {:?}", tx_hash, err);
                }
            }
        }

        Ok(())
    }
}
