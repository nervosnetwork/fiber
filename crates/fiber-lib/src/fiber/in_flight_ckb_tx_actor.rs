use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::OutPoint,
};
use ractor::{concurrency::Duration, Actor, ActorProcessingErr, ActorRef};
use tentacle::secio::PeerId;

use crate::{
    ckb::{
        CkbChainMessage, CkbTxTracer, CkbTxTracingMask, CkbTxTracingResult,
        GetBlockTimestampRequest,
    },
    fiber::NetworkActorEvent,
};

use super::{types::Hash256, NetworkActorMessage, ASSUME_NETWORK_ACTOR_ALIVE};

// tx index is not returned on older ckb version, using dummy tx index instead.
// Waiting for https://github.com/nervosnetwork/ckb/pull/4583/ to be released.
const DUMMY_FUNDING_TX_INDEX: u32 = 0;

#[derive(Clone)]
pub enum InFlightCkbTxKind {
    /// Funding(channel_id)
    Funding(Hash256),
    /// Closing(peer_id, channel_id, force_closing)
    Closing(PeerId, Hash256, bool),
}

pub struct InFlightCkbTxActor {
    pub chain_actor: ActorRef<CkbChainMessage>,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub tx_hash: Hash256,
    pub tx_kind: InFlightCkbTxKind,
    /// Number of blocks to wait to confirm the tx status
    pub confirmations: u64,
}

pub struct InFlightCkbTxActorArguments {
    pub transaction: Option<TransactionView>,
}

pub struct InFlightCkbTxActorState {
    transaction: Option<TransactionView>,
    result: Option<CkbTxTracingResult>,
}

#[derive(Clone, Debug)]
pub enum InternalMessage {
    Start,
    /// Send the transaction to CKB and retry on failures.
    SendTx,
    ReportTracingResult(CkbTxTracingResult),
}

#[derive(Clone, Debug)]
pub enum InFlightCkbTxActorMessage {
    /// Send the transaction. The actor may start with only tx tracer first.
    SendTx(TransactionView),
    /// Message for the actor itself
    Internal(InternalMessage),
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Actor for InFlightCkbTxActor {
    type Msg = InFlightCkbTxActorMessage;
    type State = InFlightCkbTxActorState;
    type Arguments = InFlightCkbTxActorArguments;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(InFlightCkbTxActorState {
            transaction: args.transaction,
            result: None,
        })
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        myself
            .send_message(InFlightCkbTxActorMessage::Internal(InternalMessage::Start))
            .map_err(Into::into)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            InFlightCkbTxActorMessage::Internal(InternalMessage::Start) => {
                self.start(myself, state).await
            }
            InFlightCkbTxActorMessage::Internal(InternalMessage::SendTx) => {
                self.send_tx(myself, state).await
            }
            InFlightCkbTxActorMessage::Internal(InternalMessage::ReportTracingResult(result)) => {
                state.result = Some(result.clone());
                self.report_tracing_result(myself, result).await
            }
            InFlightCkbTxActorMessage::SendTx(tx) => {
                let tx_hash: Hash256 = tx.hash().into();
                // ignore if the tx has does not match
                if tx_hash == self.tx_hash {
                    let should_send_tx = state.transaction.is_none();
                    state.transaction = Some(tx);
                    if should_send_tx {
                        self.send_tx_interval(myself).await?;
                    }
                } else {
                    tracing::error!("expected tx hash {}, got {}", self.tx_hash, tx_hash);
                }
                Ok(())
            }
        }
    }
}

impl InFlightCkbTxActor {
    /// Computes interval to send tx based on the confirmation
    fn interval(&self) -> Duration {
        Duration::from_secs(self.confirmations.max(1) * 2 * 8)
    }

    /// Get timeout to call chain RPC
    fn timeout_ms(&self) -> u64 {
        8000
    }

    async fn start(
        &self,
        myself: ActorRef<InFlightCkbTxActorMessage>,
        state: &mut InFlightCkbTxActorState,
    ) -> Result<(), ActorProcessingErr> {
        // start tx tracer
        let tracing_request = |tx| {
            CkbChainMessage::CreateTxTracer(CkbTxTracer {
                tx_hash: self.tx_hash,
                confirmations: self.confirmations,
                mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
                callback: tx,
            })
        };

        self.chain_actor.call_and_forward(
            tracing_request,
            &myself,
            |tracing_result| {
                InFlightCkbTxActorMessage::Internal(InternalMessage::ReportTracingResult(
                    tracing_result,
                ))
            },
            None,
        )?;

        if state.transaction.is_some() {
            self.send_tx_interval(myself).await?;
        }

        Ok(())
    }

    async fn send_tx_interval(
        &self,
        myself: ActorRef<InFlightCkbTxActorMessage>,
    ) -> Result<(), ActorProcessingErr> {
        tracing::debug!("Executing send_tx_interval...");
        let message = InFlightCkbTxActorMessage::Internal(InternalMessage::SendTx);

        // send once immediately
        myself.send_message(message.clone())?;
        myself.send_interval(self.interval(), move || message.clone());

        Ok(())
    }

    async fn send_tx(
        &self,
        myself: ActorRef<InFlightCkbTxActorMessage>,
        state: &mut InFlightCkbTxActorState,
    ) -> Result<(), ActorProcessingErr> {
        if let Some(result) = state.result.as_ref().cloned() {
            // Already got the result, retry
            return self.report_tracing_result(myself, result).await;
        }

        let tx = match state.transaction.as_ref().cloned() {
            Some(tx) => tx,
            None => return Ok(()),
        };
        tracing::debug!("Executing send_tx...");
        match ractor::call_t!(
            self.chain_actor,
            CkbChainMessage::SendTx,
            self.timeout_ms(),
            tx
        ) {
            Ok(Ok(_)) => {
                // repeat sending the tx on success to let the CKB node broadcasts it
            }
            Ok(Err(err)) => {
                tracing::error!(
                    "failed to send tx {} because of rpc error: {}",
                    self.tx_hash,
                    err
                )
            }
            Err(err) => {
                tracing::error!(
                    "failed to send tx {} because of ractor error: {}",
                    self.tx_hash,
                    err
                )
            }
        }

        Ok(())
    }

    async fn report_tracing_result(
        &self,
        myself: ActorRef<InFlightCkbTxActorMessage>,
        result: CkbTxTracingResult,
    ) -> Result<(), ActorProcessingErr> {
        let message = match (self.tx_kind.clone(), result.tx_status) {
            (
                InFlightCkbTxKind::Funding(..),
                TxStatus::Committed(block_number, block_hash, tx_index),
            ) => {
                let _ = self
                    .chain_actor
                    .send_message(CkbChainMessage::CommitFundingTx(self.tx_hash, block_number));
                if let Ok(Ok(Some(timestamp))) = ractor::call_t!(
                    self.chain_actor,
                    |tx| CkbChainMessage::GetBlockTimestamp(
                        GetBlockTimestampRequest::from_block_hash(block_hash.clone().into()),
                        tx
                    ),
                    self.timeout_ms()
                ) {
                    NetworkActorEvent::FundingTransactionConfirmed(
                        OutPoint::new(self.tx_hash.into(), DUMMY_FUNDING_TX_INDEX),
                        block_hash.clone(),
                        tx_index,
                        timestamp,
                    )
                } else {
                    // ignore the error and let it retry later on the next SendTx message.
                    return Ok(());
                }
            }
            (InFlightCkbTxKind::Funding(_channel_id), status) => {
                tracing::error!(
                    "Funding transaction {:?} failed to be confirmed with final status {:?}",
                    self.tx_hash,
                    status
                );
                NetworkActorEvent::FundingTransactionFailed(OutPoint::new(
                    self.tx_hash.into(),
                    DUMMY_FUNDING_TX_INDEX,
                ))
            }
            (
                InFlightCkbTxKind::Closing(peer_id, channel_id, force_closing),
                TxStatus::Committed(..),
            ) => {
                tracing::info!("Closing transaction {:?} confirmed", self.tx_hash);
                NetworkActorEvent::ClosingTransactionConfirmed(
                    peer_id,
                    channel_id,
                    self.tx_hash.into(),
                    force_closing,
                )
            }
            (InFlightCkbTxKind::Closing(peer_id, channel_id, _force_closing), status) => {
                tracing::error!(
                    "Closing transaction {:?} failed to be confirmed with final status {:?}",
                    self.tx_hash,
                    status
                );
                NetworkActorEvent::ClosingTransactionFailed(
                    peer_id,
                    channel_id,
                    self.tx_hash.into(),
                )
            }
        };

        self.network_actor
            .send_message(NetworkActorMessage::new_event(message))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        // The actor has done its job
        myself.stop(Some("tx tracing result already fired".to_string()));
        Ok(())
    }
}
