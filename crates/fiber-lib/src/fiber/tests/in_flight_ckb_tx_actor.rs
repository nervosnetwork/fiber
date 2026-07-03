use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use ckb_jsonrpc_types::JsonBytes;
use ckb_sdk::rpc::ckb_indexer::{Cell, Order, Pagination, SearchKey};
use ckb_sdk::RpcError;
use ckb_types::core::{tx_pool::TxStatus, TransactionView};
use ckb_types::H256;
use fiber_types::Hash256;
use ractor::{concurrency::Duration, Actor, ActorProcessingErr, ActorRef};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration as TokioDuration};

use crate::ckb::client::{CkbChainClient, GetShutdownTxResponse, GetTxResponse};
use crate::ckb::{
    CkbChainMessage, CkbTxTracingActor, CkbTxTracingArguments, CkbTxTracingMessage,
    CkbTxTracingResult,
};
use crate::fiber::{
    InFlightCkbTxActor, InFlightCkbTxActorArguments, InFlightCkbTxActorMessage, InFlightCkbTxKind,
    NetworkActorEvent, NetworkActorMessage,
};

fn permanent_send_tx_error() -> RpcError {
    RpcError::Other(anyhow::anyhow!(
        "TransactionFailedToResolve: Unknown(OutPoint)"
    ))
}

struct MockChainClient {
    tx_status: TxStatus,
}

#[async_trait::async_trait]
impl CkbChainClient for MockChainClient {
    async fn get_transaction(&self, _hash: H256) -> Result<GetTxResponse, anyhow::Error> {
        Ok(GetTxResponse {
            transaction: None,
            tx_status: self.tx_status.clone(),
        })
    }

    async fn get_cells(
        &self,
        _search_key: SearchKey,
        _order: Order,
        _limit: u32,
        _after: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>, anyhow::Error> {
        Ok(Pagination {
            objects: vec![],
            last_cursor: JsonBytes::from_bytes(ckb_types::bytes::Bytes::new()),
        })
    }

    async fn get_block_timestamp(
        &self,
        _block_hash: Hash256,
    ) -> Result<Option<u64>, anyhow::Error> {
        Ok(None)
    }

    async fn get_shutdown_tx(
        &self,
        _funding_lock_script: ckb_types::packed::Script,
    ) -> Result<Option<GetShutdownTxResponse>, anyhow::Error> {
        Ok(None)
    }
}

struct TestChainState {
    tracing_actor: ActorRef<CkbTxTracingMessage>,
    send_tx_should_fail: bool,
    send_tx_calls: Arc<AtomicUsize>,
    report_send_tx_error_calls: Arc<AtomicUsize>,
}

struct TestChainActor;

#[async_trait::async_trait]
impl Actor for TestChainActor {
    type Msg = CkbChainMessage;
    type State = TestChainState;
    type Arguments = TestChainState;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CkbChainMessage::SendTx(_tx, reply_port) => {
                state.send_tx_calls.fetch_add(1, Ordering::SeqCst);
                let result = if state.send_tx_should_fail {
                    Err(permanent_send_tx_error())
                } else {
                    Ok(())
                };
                let _ = reply_port.send(result);
            }
            CkbChainMessage::CreateTxTracer(tracer) => {
                state
                    .tracing_actor
                    .send_message(CkbTxTracingMessage::CreateTracer(tracer))?;
            }
            CkbChainMessage::ReportSendTxError(tx_hash, err) => {
                state
                    .report_send_tx_error_calls
                    .fetch_add(1, Ordering::SeqCst);
                state
                    .tracing_actor
                    .send_message(CkbTxTracingMessage::ReportSendTxError(tx_hash, err))?;
            }
            _ => {}
        }
        Ok(())
    }
}

struct TestNetworkActor;

#[async_trait::async_trait]
impl Actor for TestNetworkActor {
    type Msg = NetworkActorMessage;
    type State = mpsc::UnboundedSender<NetworkActorEvent>;
    type Arguments = mpsc::UnboundedSender<NetworkActorEvent>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        sender: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(sender)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        sender: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let NetworkActorMessage::Event(event) = message {
            let _ = sender.send(event);
        }
        Ok(())
    }
}

async fn spawn_test_actors(
    tx_status: TxStatus,
    send_tx_should_fail: bool,
) -> (
    ActorRef<InFlightCkbTxActorMessage>,
    mpsc::UnboundedReceiver<NetworkActorEvent>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    ActorRef<CkbTxTracingMessage>,
    Hash256,
) {
    let (tracing_actor, _) = Actor::spawn(
        None,
        CkbTxTracingActor::new(),
        CkbTxTracingArguments {
            rpc_url: "http://127.0.0.1:0".to_string(),
            polling_interval: Duration::from_secs(3600),
        },
    )
    .await
    .expect("spawn tracing actor");

    let tracing_actor_for_test = tracing_actor.clone();
    let send_tx_calls = Arc::new(AtomicUsize::new(0));
    let report_send_tx_error_calls = Arc::new(AtomicUsize::new(0));
    let (chain_actor, _) = Actor::spawn(
        None,
        TestChainActor,
        TestChainState {
            tracing_actor,
            send_tx_should_fail,
            send_tx_calls: send_tx_calls.clone(),
            report_send_tx_error_calls: report_send_tx_error_calls.clone(),
        },
    )
    .await
    .expect("spawn chain actor");

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (network_actor, _) = Actor::spawn(None, TestNetworkActor, event_tx)
        .await
        .expect("spawn network actor");

    let tx = TransactionView::new_advanced_builder().build();
    let tx_hash: Hash256 = tx.hash().into();
    let channel_id = Hash256::from([1; 32]);
    let (in_flight_actor, _) = Actor::spawn(
        None,
        InFlightCkbTxActor {
            chain_actor,
            chain_client: MockChainClient {
                tx_status: tx_status.clone(),
            },
            network_actor,
            tx_hash,
            tx_kind: InFlightCkbTxKind::Funding(channel_id),
            confirmations: 1,
        },
        InFlightCkbTxActorArguments {
            transaction: Some(tx),
        },
    )
    .await
    .expect("spawn in flight actor");

    (
        in_flight_actor,
        event_rx,
        send_tx_calls,
        report_send_tx_error_calls,
        tracing_actor_for_test,
        tx_hash,
    )
}

async fn poll_unknown_with_tip(
    tracing_actor: &ActorRef<CkbTxTracingMessage>,
    tx_hash: Hash256,
    tip_block_number: u64,
) {
    tracing_actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::unknown(tx_hash),
            tip_block_number,
        ))
        .expect("report unknown poll");
}

#[tokio::test]
async fn permanent_send_tx_error_on_unknown_tx_emits_funding_transaction_failed() {
    let (_in_flight_actor, mut event_rx, send_tx_calls, report_calls, tracing_actor, tx_hash) =
        spawn_test_actors(TxStatus::Unknown, true).await;

    tokio::time::sleep(TokioDuration::from_millis(100)).await;
    assert!(report_calls.load(Ordering::SeqCst) >= 1);

    poll_unknown_with_tip(&tracing_actor, tx_hash, 100).await;
    poll_unknown_with_tip(&tracing_actor, tx_hash, 101).await;

    let event = timeout(TokioDuration::from_secs(1), event_rx.recv())
        .await
        .expect("should receive network event within timeout")
        .expect("network event channel open");
    assert!(matches!(
        event,
        NetworkActorEvent::FundingTransactionFailed(_)
    ));
    assert_eq!(send_tx_calls.load(Ordering::SeqCst), 1);
    assert!(report_calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn committed_preflight_skips_send_tx_and_error_reporting() {
    let (_in_flight_actor, mut event_rx, send_tx_calls, report_calls, ..) =
        spawn_test_actors(TxStatus::Committed(0, H256::default(), 0), true).await;

    let recv_result = timeout(TokioDuration::from_millis(200), event_rx.recv()).await;
    assert!(
        recv_result.is_err(),
        "committed preflight should not emit funding failure"
    );
    assert_eq!(send_tx_calls.load(Ordering::SeqCst), 0);
    assert_eq!(report_calls.load(Ordering::SeqCst), 0);
}
