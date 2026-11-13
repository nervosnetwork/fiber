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
            watched_inputs: Vec::new(),
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
            watched_inputs: Vec::new(),
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
            watched_inputs: Vec::new(),
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::new(tx_hash, TxStatus::Pending),
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
            watched_inputs: Vec::new(),
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    actor
        .send_message(CkbTxTracingMessage::report_tracing_result(
            CkbTxTracingResult::new(tx_hash, TxStatus::Pending),
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
            CkbTxTracingResult::new(tx_hash, TxStatus::Pending),
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

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn watched_input_spent_by_committed_tx_reports_rejected_after_confirmations() {
    use ckb_types::packed::OutPoint;
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let parent_tx_hash = Hash256::from([0x11; 32]);
    let funding_tx_hash = Hash256::from([0x22; 32]);
    let input = OutPoint::new(parent_tx_hash.into(), 0);

    let tip = Arc::new(AtomicU64::new(100));
    let cell_spent = Arc::new(AtomicBool::new(false));
    let fail_live_cell_rpc = Arc::new(AtomicBool::new(false));
    let funding_committed = Arc::new(AtomicBool::new(false));
    let rpc_requests = Arc::new(AtomicU64::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let tip_srv = tip.clone();
    let cell_spent_srv = cell_spent.clone();
    let fail_live_cell_rpc_srv = fail_live_cell_rpc.clone();
    let funding_committed_srv = funding_committed.clone();
    let rpc_requests_srv = rpc_requests.clone();
    let parent_hex = format!("{:#x}", ckb_types::H256::from(parent_tx_hash));
    let funding_hex = format!("{:#x}", ckb_types::H256::from(funding_tx_hash));

    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let tip_srv = tip_srv.clone();
            let cell_spent_srv = cell_spent_srv.clone();
            let fail_live_cell_rpc_srv = fail_live_cell_rpc_srv.clone();
            let funding_committed_srv = funding_committed_srv.clone();
            let rpc_requests_srv = rpc_requests_srv.clone();
            let parent_hex = parent_hex.clone();
            let funding_hex = funding_hex.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = match socket.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                let json: serde_json::Value =
                    serde_json::from_str(&req[body_start..]).unwrap_or_default();
                let id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let method = json.get("method").and_then(|m| m.as_str()).unwrap_or("");
                if method == "get_live_cell" && fail_live_cell_rpc_srv.load(Ordering::SeqCst) {
                    let resp_body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32603, "message": "transient rpc failure" }
                    })
                    .to_string();
                    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n";
                    let http_resp = format!(
                        "{}Content-Length: {}\r\n\r\n{}",
                        header,
                        resp_body.len(),
                        resp_body
                    );
                    let _ = socket.write_all(http_resp.as_bytes()).await;
                    rpc_requests_srv.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                let result = match method {
                    "get_tip_block_number" => {
                        serde_json::json!(format!("{:#x}", tip_srv.load(Ordering::SeqCst)))
                    }
                    "get_live_cell" => {
                        if cell_spent_srv.load(Ordering::SeqCst) {
                            serde_json::json!({ "cell": null, "status": "unknown" })
                        } else {
                            serde_json::json!({
                                "cell": {
                                    "data": null,
                                    "output": {
                                        "capacity": "0x2540be400",
                                        "lock": {
                                            "code_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                                            "hash_type": "data",
                                            "args": "0x"
                                        },
                                        "type": null
                                    }
                                },
                                "status": "live"
                            })
                        }
                    }
                    "get_transaction" => {
                        let hash = json
                            .get("params")
                            .and_then(|p| p.get(0))
                            .and_then(|h| h.as_str())
                            .unwrap_or("");
                        if hash.eq_ignore_ascii_case(&parent_hex) {
                            serde_json::json!({
                                "transaction": null,
                                "cycles": null,
                                "tx_status": {
                                    "status": "committed",
                                    "block_number": "0x50",
                                    "block_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                                    "tx_index": "0x0"
                                }
                            })
                        } else if hash.eq_ignore_ascii_case(&funding_hex)
                            && funding_committed_srv.load(Ordering::SeqCst)
                        {
                            serde_json::json!({
                                "transaction": null,
                                "cycles": null,
                                "tx_status": {
                                    "status": "committed",
                                    "block_number": "0x65",
                                    "block_hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
                                    "tx_index": "0x0"
                                }
                            })
                        } else {
                            serde_json::json!({
                                "transaction": null,
                                "cycles": null,
                                "tx_status": {
                                    "status": "unknown"
                                }
                            })
                        }
                    }
                    _ => serde_json::Value::Null,
                };
                let resp_body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                })
                .to_string();
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                let _ = socket.write_all(http_resp.as_bytes()).await;
                rpc_requests_srv.fetch_add(1, Ordering::SeqCst);
            });
        }
    });

    let wait_for_rpcs = |target: u64| {
        let rpc_requests = rpc_requests.clone();
        async move {
            timeout(TokioDuration::from_secs(2), async move {
                while rpc_requests.load(Ordering::SeqCst) < target {
                    tokio::time::sleep(TokioDuration::from_millis(5)).await;
                }
            })
            .await
            .expect("mock CKB server should receive expected RPC calls");
            tokio::time::sleep(TokioDuration::from_millis(20)).await;
        }
    };

    let (actor, _handle) = Actor::spawn(
        None,
        CkbTxTracingActor::new(),
        CkbTxTracingArguments {
            rpc_url: format!("http://{}", addr),
            polling_interval: Duration::from_secs(3600),
        },
    )
    .await
    .expect("spawn tx tracing actor");

    let (send, mut recv) = oneshot::channel();
    actor
        .send_message(CkbTxTracingMessage::CreateTracer(CkbTxTracer {
            tx_hash: funding_tx_hash,
            confirmations: 4,
            mask: CkbTxTracingMask::Committed | CkbTxTracingMask::Rejected,
            watched_inputs: vec![input],
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    // Initial CreateTracer triggers 1 poll: tip + get_tx(funding) + get_live_cell = 3 RPCs.
    wait_for_rpcs(3).await;

    // Input is still live: advancing tip must not reject the unknown funding tx.
    tip.store(110, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 3).await;
    assert!(
        recv.try_recv().is_err(),
        "live input must keep unknown funding tx pending"
    );

    // Spend the input in another committed tx at tip 110; callback must wait until tip >= 114.
    // Spent poll performs 5 RPCs: tip + get_tx(funding) + get_live_cell + get_tx(parent) + re-check get_tx(funding).
    cell_spent.store(true, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 5).await;
    assert!(
        recv.try_recv().is_err(),
        "spent input rejection must wait for confirmations"
    );

    // Shallow reorg restores the live cell before 4 confirmations: must reset and not fire.
    cell_spent.store(false, Ordering::SeqCst);
    tip.store(115, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 3).await;
    assert!(
        recv.try_recv().is_err(),
        "reorg restoring live cell must cancel pending rejection"
    );

    // Spend again at tip 120; inject a transient RPC failure at tip 122 and verify it does
    // not reset `last_block = 120`, so reaching tip 124 (124 - 120 == 4) still fires on time.
    cell_spent.store(true, Ordering::SeqCst);
    tip.store(120, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 5).await;

    fail_live_cell_rpc.store(true, Ordering::SeqCst);
    tip.store(122, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 3).await;
    assert!(
        recv.try_recv().is_err(),
        "transient RPC failure at tip 122 should neither fire early nor reset last_block"
    );

    fail_live_cell_rpc.store(false, Ordering::SeqCst);
    tip.store(124, Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");

    let result = timeout(TokioDuration::from_secs(2), recv)
        .await
        .expect("spent watched input should reject at tip 124 without resetting at tip 122")
        .expect("receive tracing result");
    assert!(
        result.watched_input_spent,
        "result must carry watched_input_spent=true"
    );
    assert!(
        matches!(result.tx_status, TxStatus::Rejected(ref msg) if msg.contains("Funding input spent")),
        "unexpected tracing status: {:?}",
        result.tx_status
    );

    actor.stop(None);
    server.abort();
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn evicted_rejected_funding_tx_waits_for_committed_input_spend_when_watching_input_spent() {
    use ckb_types::packed::OutPoint;
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let parent_tx_hash = Hash256::from([0x31; 32]);
    let funding_tx_hash = Hash256::from([0x42; 32]);
    let input = OutPoint::new(parent_tx_hash.into(), 0);

    let tip = Arc::new(AtomicU64::new(100));
    let cell_spent = Arc::new(AtomicBool::new(false));
    let rpc_requests = Arc::new(AtomicU64::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let tip_srv = tip.clone();
    let cell_spent_srv = cell_spent.clone();
    let rpc_requests_srv = rpc_requests.clone();
    let parent_hex = format!("{:#x}", ckb_types::H256::from(parent_tx_hash));

    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let tip_srv = tip_srv.clone();
            let cell_spent_srv = cell_spent_srv.clone();
            let rpc_requests_srv = rpc_requests_srv.clone();
            let parent_hex = parent_hex.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = match socket.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                let json: serde_json::Value =
                    serde_json::from_str(&req[body_start..]).unwrap_or_default();
                let id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let method = json.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let result = match method {
                    "get_tip_block_number" => {
                        serde_json::json!(format!("{:#x}", tip_srv.load(Ordering::SeqCst)))
                    }
                    "get_live_cell" => {
                        if cell_spent_srv.load(Ordering::SeqCst) {
                            serde_json::json!({ "cell": null, "status": "unknown" })
                        } else {
                            serde_json::json!({
                                "cell": {
                                    "data": null,
                                    "output": {
                                        "capacity": "0x2540be400",
                                        "lock": {
                                            "code_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                                            "hash_type": "data",
                                            "args": "0x"
                                        },
                                        "type": null
                                    }
                                },
                                "status": "live"
                            })
                        }
                    }
                    "get_transaction" => {
                        let hash = json
                            .get("params")
                            .and_then(|p| p.get(0))
                            .and_then(|h| h.as_str())
                            .unwrap_or("");
                        if hash.eq_ignore_ascii_case(&parent_hex) {
                            serde_json::json!({
                                "transaction": null,
                                "cycles": null,
                                "tx_status": {
                                    "status": "committed",
                                    "block_number": "0x50",
                                    "block_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                                    "tx_index": "0x0"
                                }
                            })
                        } else {
                            // Funding tx was evicted from CKB's mempool by RBF, so CKB caches `rejected`.
                            serde_json::json!({
                                "transaction": null,
                                "cycles": null,
                                "tx_status": {
                                    "status": "rejected",
                                    "reason": "PoolRejectedRBF: evicted by conflicting transaction"
                                }
                            })
                        }
                    }
                    _ => serde_json::Value::Null,
                };
                let resp_body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                })
                .to_string();
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                let _ = socket.write_all(http_resp.as_bytes()).await;
                rpc_requests_srv.fetch_add(1, Ordering::SeqCst);
            });
        }
    });

    let wait_for_rpcs = |target: u64| {
        let rpc_requests = rpc_requests.clone();
        async move {
            timeout(TokioDuration::from_secs(2), async move {
                while rpc_requests.load(Ordering::SeqCst) < target {
                    tokio::time::sleep(TokioDuration::from_millis(5)).await;
                }
            })
            .await
            .expect("mock CKB server should receive expected RPC calls");
            tokio::time::sleep(TokioDuration::from_millis(20)).await;
        }
    };

    let (actor, _handle) = Actor::spawn(
        None,
        CkbTxTracingActor::new(),
        CkbTxTracingArguments {
            rpc_url: format!("http://{}", addr),
            polling_interval: Duration::from_secs(3600),
        },
    )
    .await
    .expect("spawn tx tracing actor");

    let (send, mut recv) = oneshot::channel();
    actor
        .send_message(CkbTxTracingMessage::CreateTracer(CkbTxTracer {
            tx_hash: funding_tx_hash,
            confirmations: 4,
            mask: CkbTxTracingMask::Committed | CkbTxTracingMask::WatchedInputSpent,
            watched_inputs: vec![input],
            callback: RpcReplyPort::from(send),
        }))
        .expect("create tracer");
    // Initial poll: tip + get_tx(funding=rejected) + get_live_cell(live) = 3 RPCs.
    wait_for_rpcs(3).await;

    // Even after > 4 blocks of CKB reporting `Rejected`, unconfirmed RBF (cell still live)
    // must NOT fire a `WatchedInputSpent` tracer.
    tip.store(106, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 3).await;
    assert!(
        recv.try_recv().is_err(),
        "mempool-only RBF rejection must not fire WatchedInputSpent before conflicting tx commits"
    );

    // Conflicting tx commits at tip 110: `last_block` must reset to 110 (not reuse 100) and wait 4 blocks.
    cell_spent.store(true, Ordering::SeqCst);
    tip.store(110, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 5).await;
    assert!(
        recv.try_recv().is_err(),
        "spent input on evicted tx must wait 4 confirmations from when the spend committed"
    );

    tip.store(113, Ordering::SeqCst);
    let before = rpc_requests.load(Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");
    wait_for_rpcs(before + 5).await;
    assert!(
        recv.try_recv().is_err(),
        "3 confirmations after committed spend must not fire yet"
    );

    tip.store(114, Ordering::SeqCst);
    actor
        .send_message(CkbTxTracingMessage::run_tracers())
        .expect("run tracers");

    let result = timeout(TokioDuration::from_secs(2), recv)
        .await
        .expect("evicted funding tx with committed input spend should fire at tip 114")
        .expect("receive tracing result");
    assert!(
        result.watched_input_spent,
        "evicted funding tx with committed input spend must report watched_input_spent=true"
    );

    actor.stop(None);
    server.abort();
}
