use std::collections::HashMap;

use bitmask_enum::bitmask;
use ckb_sdk::RpcError;
use ckb_types::{core::tx_pool::TxStatus, packed::OutPoint, prelude::*};
use ractor::{concurrency::Duration, Actor, ActorProcessingErr, ActorRef, RpcReplyPort};

use crate::ckb::config::new_ckb_rpc_async_client;
use fiber_types::Hash256;

use super::jsonrpc_types_convert::tx_status_from_json;

const SYNTHETIC_REJECT_REASON: &str = "Transaction rejected when submitting";
const FUNDING_INPUT_SPENT_REJECT_REASON: &str =
    "Funding input spent by another committed transaction";

/// CKB's `PoolRejectedRBF`: another transaction in the pool spends an input of this one.
pub(crate) fn is_pool_conflict_code(code: i64) -> bool {
    code == -1111
}

/// Whether `err` says another transaction in the pool spends an input of this one.
pub(crate) fn is_pool_conflict(err: &RpcError) -> bool {
    matches!(err, RpcError::Rpc(e) if is_pool_conflict_code(e.code.code()))
}

/// Permanent SendTx RPC errors that indicate the transaction cannot be accepted into the pool.
/// A pool conflict counts: who reports it decides for which transactions (a funding, not a
/// closing, `InFlightCkbTxActor`).
pub(crate) fn is_permanent_send_tx_error(err: &RpcError) -> bool {
    match err {
        RpcError::Rpc(e) => {
            if e.code.code() == -301 || is_pool_conflict_code(e.code.code()) {
                return true;
            }
            e.message.contains("TransactionFailedToResolve")
        }
        RpcError::Other(e) => e.to_string().contains("TransactionFailedToResolve"),
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub struct CkbTxTracingResult {
    pub tx_hash: Hash256,
    pub tx_status: TxStatus,
    pub watched_input_spent: bool,
}

impl CkbTxTracingResult {
    pub fn new(tx_hash: Hash256, tx_status: TxStatus) -> Self {
        Self {
            tx_hash,
            tx_status,
            watched_input_spent: false,
        }
    }

    pub fn unknown(tx_hash: Hash256) -> Self {
        Self::new(tx_hash, TxStatus::Unknown)
    }

    pub fn watched_input_spent(tx_hash: Hash256) -> Self {
        Self {
            tx_hash,
            tx_status: TxStatus::Rejected(FUNDING_INPUT_SPENT_REJECT_REASON.to_string()),
            watched_input_spent: true,
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
    pub watched_inputs: Vec<OutPoint>,
    pub callback: RpcReplyPort<CkbTxTracingResult>,
}

#[derive(Debug)]
struct CkbTxTracersForTheSameTx {
    last_status: TxStatus,
    last_watched_input_spent: bool,
    last_block: u64,
    tracers: Vec<CkbTxTracer>,
    watched_inputs: Vec<OutPoint>,
    send_tx_error: Option<RpcError>,
}

impl Default for CkbTxTracersForTheSameTx {
    fn default() -> Self {
        Self {
            last_status: TxStatus::Unknown,
            last_watched_input_spent: false,
            last_block: 0,
            tracers: Default::default(),
            watched_inputs: Default::default(),
            send_tx_error: None,
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
    ReportSendTxError(Hash256, RpcError),

    Internal(InternalMessage),
}

impl CkbTxTracingMessage {
    pub(crate) fn run_tracers() -> Self {
        Self::Internal(InternalMessage::RunTracers)
    }

    pub(crate) fn report_tracing_result(result: CkbTxTracingResult, tip_block_number: u64) -> Self {
        Self::Internal(InternalMessage::ReportTracingResult(
            result,
            tip_block_number,
        ))
    }
}

#[async_trait::async_trait]
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
        use CkbTxTracingMessage::{CreateTracer, Internal, RemoveTracers, ReportSendTxError};

        match message {
            CreateTracer(arguments) => state.create_tracer(myself, arguments).await,
            RemoveTracers(arguments) => state.remove_tracers(arguments),
            ReportSendTxError(tx_hash, err) => state.report_send_tx_error(tx_hash, err),
            Internal(InternalMessage::RunTracers) => state.run_tracers(myself, None).await,
            Internal(InternalMessage::ReportTracingResult(result, tip_block_number)) => {
                state.report_tracing_result(result, Some(tip_block_number))
            }
        }
    }
}

struct TracingTask {
    actor: ActorRef<CkbTxTracingMessage>,
    rpc_url: String,
    targets: Vec<(Hash256, Vec<OutPoint>)>,
}

impl CkbTxTracingState {
    async fn create_tracer(
        &mut self,
        myself: ActorRef<CkbTxTracingMessage>,
        tracer: CkbTxTracer,
    ) -> Result<(), ActorProcessingErr> {
        let tx_hash = tracer.tx_hash;
        let tx_tracers = self.tracers.entry(tx_hash).or_default();
        let mut added_watched_input = false;
        for input in &tracer.watched_inputs {
            if !tx_tracers.watched_inputs.contains(input) {
                tx_tracers.watched_inputs.push(input.clone());
                added_watched_input = true;
            }
        }
        tx_tracers.tracers.push(tracer);

        if tx_tracers.tracers.len() == 1 || added_watched_input {
            // If there's no existing tracer for the tx (or new watched inputs were registered),
            // trigger the tracing task now.
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
        let targets: Vec<(Hash256, Vec<OutPoint>)> = match tx_hashes {
            Some(tx_hashes) => tx_hashes
                .into_iter()
                .filter_map(|tx_hash| {
                    self.tracers
                        .get(&tx_hash)
                        .map(|entry| (tx_hash, entry.watched_inputs.clone()))
                })
                .collect(),
            None => self
                .tracers
                .iter()
                .map(|(tx_hash, entry)| (*tx_hash, entry.watched_inputs.clone()))
                .collect(),
        };
        if !targets.is_empty() {
            let task = TracingTask {
                actor: myself,
                rpc_url: self.rpc_url.clone(),
                targets,
            };
            task.spawn();
        }
        Ok(())
    }

    fn report_send_tx_error(
        &mut self,
        tx_hash: Hash256,
        err: RpcError,
    ) -> Result<(), ActorProcessingErr> {
        if !is_permanent_send_tx_error(&err) {
            return Ok(());
        }

        let Some(tx_tracers) = self.tracers.get_mut(&tx_hash) else {
            return Ok(());
        };

        tx_tracers.send_tx_error = Some(err);

        Ok(())
    }

    fn report_tracing_result(
        &mut self,
        result: CkbTxTracingResult,
        tip_block_number: Option<u64>,
    ) -> Result<(), ActorProcessingErr> {
        let tx_tracers = if let Some(tx_tracers) = self.tracers.get_mut(&result.tx_hash) {
            tx_tracers
        } else {
            return Ok(());
        };

        let polled_status = result.tx_status.clone();

        if !matches!(polled_status, TxStatus::Unknown) {
            tx_tracers.send_tx_error = None;
        }

        let synthesized_rejection = !result.watched_input_spent
            && matches!(polled_status, TxStatus::Unknown)
            && tx_tracers
                .send_tx_error
                .as_ref()
                .is_some_and(is_permanent_send_tx_error);

        let (tx_status, watched_input_spent) = if synthesized_rejection {
            (
                TxStatus::Rejected(SYNTHETIC_REJECT_REASON.to_string()),
                false,
            )
        } else {
            (polled_status.clone(), result.watched_input_spent)
        };

        let effective_result = CkbTxTracingResult {
            tx_hash: result.tx_hash,
            tx_status: tx_status.clone(),
            watched_input_spent,
        };

        if let Some(tip_block_number) = tip_block_number {
            match tx_status {
                TxStatus::Committed(block_number, ..) => {
                    // Always use the latest committed block number
                    tx_tracers.last_block = block_number;
                }
                _ => {
                    // Otherwise, use tip block number when first entered this status
                    if tx_tracers.last_block == 0
                        || tx_status != tx_tracers.last_status
                        || watched_input_spent != tx_tracers.last_watched_input_spent
                    {
                        tx_tracers.last_block = tip_block_number
                    }
                }
            }
        }
        tx_tracers.last_status = tx_status.clone();
        tx_tracers.last_watched_input_spent = watched_input_spent;

        let last_block = tx_tracers.last_block;
        let has_enough_confirmations = |tracer: &CkbTxTracer| {
            tip_block_number
                .map(|tip_block_number| {
                    tip_block_number.saturating_sub(last_block) >= tracer.confirmations
                })
                .unwrap_or(false)
        };

        // Leave only tracers not fired
        for i in (0..tx_tracers.tracers.len()).rev() {
            let tracer = &tx_tracers.tracers[i];
            if tracer.mask.contains((&tx_status).into()) && has_enough_confirmations(tracer) {
                let tracer = tx_tracers.tracers.swap_remove(i);
                // ignore the error if the receiver is dropped
                let _ = tracer.callback.send(effective_result.clone());
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
        ractor::concurrency::spawn(async move { self.run().await });
    }

    async fn run(self) {
        if let Err(err) = self.run_inner().await {
            tracing::error!("Failed to run CKB tx tracing task: {:?}", err);
        }
    }

    async fn run_inner(self) -> Result<(), Box<dyn std::error::Error>> {
        let ckb_client = new_ckb_rpc_async_client(&self.rpc_url);
        let tip_block_number: u64 = ckb_client.get_tip_block_number().await?.into();

        for (tx_hash, watched_inputs) in self.targets {
            match ckb_client.get_transaction(tx_hash.into()).await {
                Ok(response_opt) => {
                    let mut result = response_opt
                        .map(|response| {
                            CkbTxTracingResult::new(
                                tx_hash,
                                tx_status_from_json(response.tx_status),
                            )
                        })
                        .unwrap_or_else(|| CkbTxTracingResult::unknown(tx_hash));

                    if matches!(result.tx_status, TxStatus::Unknown) && !watched_inputs.is_empty() {
                        match Self::any_watched_input_spent_by_other_tx(
                            &ckb_client,
                            tx_hash,
                            &watched_inputs,
                        )
                        .await
                        {
                            Ok(true) => {
                                result = CkbTxTracingResult::watched_input_spent(tx_hash);
                            }
                            Ok(false) => {}
                            Err(_) => {
                                // Skip reporting this poll so transient RPC failures do not
                                // reset `last_status` / `last_block` confirmation progress.
                                continue;
                            }
                        }
                    }

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

    async fn any_watched_input_spent_by_other_tx(
        ckb_client: &ckb_sdk::CkbRpcAsyncClient,
        tx_hash: Hash256,
        watched_inputs: &[OutPoint],
    ) -> Result<bool, RpcError> {
        let mut transient_err: Option<RpcError> = None;
        for input in watched_inputs {
            let live_cell = match ckb_client.get_live_cell(input.clone().into(), false).await {
                Ok(cell) => cell,
                Err(err) => {
                    tracing::warn!(
                        "Failed to query live cell {:?} for traced tx {}: {:?}",
                        input,
                        tx_hash,
                        err
                    );
                    transient_err.get_or_insert(err);
                    continue;
                }
            };
            if live_cell.cell.is_some() || live_cell.status == "live" {
                continue;
            }

            // Confirm the parent transaction creating this input is actually committed
            // on-chain (rather than still unconfirmed or rolled back), so a missing live
            // cell proves it was consumed by a committed transaction.
            let prev_tx_hash: ckb_types::H256 = input.tx_hash().unpack();
            let prev_committed = match ckb_client.get_transaction(prev_tx_hash).await {
                Ok(Some(prev_tx)) => {
                    matches!(
                        tx_status_from_json(prev_tx.tx_status),
                        TxStatus::Committed(..)
                    )
                }
                Ok(None) => false,
                Err(err) => {
                    tracing::warn!(
                        "Failed to query input parent tx {:x} for traced tx {}: {:?}",
                        input.tx_hash(),
                        tx_hash,
                        err
                    );
                    transient_err.get_or_insert(err);
                    continue;
                }
            };
            if !prev_committed {
                continue;
            }

            // Re-check the traced transaction itself to avoid racing with the funding
            // transaction being the very transaction that just consumed `input`.
            match ckb_client.get_transaction(tx_hash.into()).await {
                Ok(Some(response)) => {
                    if !matches!(tx_status_from_json(response.tx_status), TxStatus::Unknown) {
                        return Ok(false);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        "Failed to re-verify traced tx {} after observing spent input {:?}: {:?}",
                        tx_hash,
                        input,
                        err
                    );
                    return Err(err);
                }
            }

            return Ok(true);
        }
        if let Some(err) = transient_err {
            return Err(err);
        }
        Ok(false)
    }
}
