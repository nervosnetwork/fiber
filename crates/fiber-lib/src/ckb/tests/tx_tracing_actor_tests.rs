use ckb_sdk::RpcError;
use ckb_types::core::tx_pool::TxStatus;
use fiber_types::Hash256;
use ractor::{concurrency::Duration, Actor, RpcReplyPort};
use tokio::{
    sync::oneshot,
    time::{timeout, Duration as TokioDuration},
};

use crate::ckb::{
    CkbTxTracer, CkbTxTracingActor, CkbTxTracingArguments, CkbTxTracingMask, CkbTxTracingMessage,
    CkbTxTracingResult,
};

fn permanent_send_tx_error() -> RpcError {
    RpcError::Other(anyhow::anyhow!(
        "TransactionFailedToResolve: Unknown(OutPoint)"
    ))
}

#[tokio::test]
async fn report_send_tx_error_does_not_fire_rejected_callback_immediately() {
    let (actor, _handle) = Actor::spawn(
        None,
        CkbTxTracingActor::new(),
        CkbTxTracingArguments {
            rpc_url: "http://127.0.0.1:0".to_string(),
            polling_interval: Duration::from_secs(3600),
        },
    )
    .await
    .expect("spawn tx tracing actor");

    let tx_hash = Hash256::from([42; 32]);
    let (send, recv) = oneshot::channel();
    actor
        .send_message(CkbTxTracingMessage::CreateTracer(CkbTxTracer {
            tx_hash,
            confirmations: 4,
            mask: CkbTxTracingMask::Rejected,
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    actor
        .send_message(CkbTxTracingMessage::ReportSendTxError(
            tx_hash,
            permanent_send_tx_error(),
        ))
        .expect("report send tx error");

    let recv_result = timeout(TokioDuration::from_millis(50), recv).await;
    assert!(
        recv_result.is_err(),
        "ReportSendTxError alone should not fire rejected callback"
    );

    actor.stop(None);
}

#[tokio::test]
async fn synthetic_rejected_waits_for_confirmations_before_callback() {
    let (actor, _handle) = Actor::spawn(
        None,
        CkbTxTracingActor::new(),
        CkbTxTracingArguments {
            rpc_url: "http://127.0.0.1:0".to_string(),
            polling_interval: Duration::from_secs(3600),
        },
    )
    .await
    .expect("spawn tx tracing actor");

    let tx_hash = Hash256::from([45; 32]);
    let (send, mut recv) = oneshot::channel();
    actor
        .send_message(CkbTxTracingMessage::CreateTracer(CkbTxTracer {
            tx_hash,
            confirmations: 4,
            mask: CkbTxTracingMask::Rejected,
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    actor
        .send_message(CkbTxTracingMessage::ReportSendTxError(
            tx_hash,
            permanent_send_tx_error(),
        ))
        .expect("report send tx error");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::unknown(tx_hash),
            100,
        ))
        .expect("report unknown at tip 100");

    assert!(
        recv.try_recv().is_err(),
        "synthetic rejected should wait for enough confirmations"
    );

    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::unknown(tx_hash),
            104,
        ))
        .expect("report unknown at tip 104");

    let result = timeout(TokioDuration::from_millis(50), recv)
        .await
        .expect("synthetic rejected callback should fire after enough confirmations")
        .expect("receive tracing result");
    assert!(matches!(result.tx_status, TxStatus::Rejected(_)));

    actor.stop(None);
}

#[tokio::test]
async fn unknown_poll_with_permanent_send_tx_error_reports_rejected() {
    let (actor, _handle) = Actor::spawn(
        None,
        CkbTxTracingActor::new(),
        CkbTxTracingArguments {
            rpc_url: "http://127.0.0.1:0".to_string(),
            polling_interval: Duration::from_secs(3600),
        },
    )
    .await
    .expect("spawn tx tracing actor");

    let tx_hash = Hash256::from([43; 32]);
    let (send, mut recv) = oneshot::channel();
    actor
        .send_message(CkbTxTracingMessage::CreateTracer(CkbTxTracer {
            tx_hash,
            confirmations: 4,
            mask: CkbTxTracingMask::Rejected,
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult {
                tx_hash,
                tx_status: TxStatus::Pending,
            },
            100,
        ))
        .expect("report pending");
    actor
        .send_message(CkbTxTracingMessage::ReportSendTxError(
            tx_hash,
            permanent_send_tx_error(),
        ))
        .expect("report send tx error");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::unknown(tx_hash),
            101,
        ))
        .expect("report unknown at tip 101");

    assert!(
        recv.try_recv().is_err(),
        "unknown poll with stored permanent error should wait for confirmations"
    );

    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::unknown(tx_hash),
            105,
        ))
        .expect("report unknown at tip 105");

    let result = timeout(TokioDuration::from_millis(50), recv)
        .await
        .expect("unknown poll with stored permanent error should reject after confirmations")
        .expect("receive tracing result");
    assert!(matches!(result.tx_status, TxStatus::Rejected(_)));

    actor.stop(None);
}

#[tokio::test]
async fn non_unknown_poll_clears_send_tx_error() {
    let (actor, _handle) = Actor::spawn(
        None,
        CkbTxTracingActor::new(),
        CkbTxTracingArguments {
            rpc_url: "http://127.0.0.1:0".to_string(),
            polling_interval: Duration::from_secs(3600),
        },
    )
    .await
    .expect("spawn tx tracing actor");

    let tx_hash = Hash256::from([44; 32]);
    let (send, mut recv) = oneshot::channel();
    actor
        .send_message(CkbTxTracingMessage::CreateTracer(CkbTxTracer {
            tx_hash,
            confirmations: 4,
            mask: CkbTxTracingMask::Rejected,
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult {
                tx_hash,
                tx_status: TxStatus::Pending,
            },
            100,
        ))
        .expect("report pending");
    actor
        .send_message(CkbTxTracingMessage::ReportSendTxError(
            tx_hash,
            permanent_send_tx_error(),
        ))
        .expect("report send tx error");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult {
                tx_hash,
                tx_status: TxStatus::Pending,
            },
            101,
        ))
        .expect("report pending again");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::unknown(tx_hash),
            102,
        ))
        .expect("report unknown at tip 102");

    assert!(
        recv.try_recv().is_err(),
        "cleared send tx error should not synthesize rejection on later unknown poll"
    );

    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::unknown(tx_hash),
            106,
        ))
        .expect("report unknown at tip 106");

    let recv_result = timeout(TokioDuration::from_millis(50), recv).await;
    assert!(
        recv_result.is_err(),
        "cleared send tx error should not report rejected even after enough confirmations"
    );

    actor.stop(None);
}
