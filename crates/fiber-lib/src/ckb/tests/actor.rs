use std::time::Duration;

use super::test_utils::{
    create_mock_chain_actor_with_shared_state, submit_tx, CellStatus, MockChainActor,
    MockChainController,
};
use crate::ckb::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};
use crate::ckb::{
    CkbChainMessage, CkbOutPointSpendTracer, CkbOutPointSpendTracingResult, LiveCell,
};
use crate::create_mock_chain_actor;
use ckb_types::bytes::Bytes;
use ckb_types::core::tx_pool::TxStatus;
use ckb_types::core::TransactionView;
use ckb_types::packed::{CellInput, CellOutput, OutPoint};
use ckb_types::prelude::Builder;
use fiber_types::Hash256;
use molecule::prelude::Entity;
use ractor::{call_t, Actor, ActorRef};
use tokio::sync::oneshot;

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_submit_empty_tx() {
    let actor = create_mock_chain_actor().await;
    assert!(matches!(
        submit_tx(actor, TransactionView::new_advanced_builder().build()).await,
        TxStatus::Committed(..)
    ));
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_submit_one_output_tx() {
    let actor = create_mock_chain_actor().await;
    assert!(matches!(
        submit_tx(
            actor,
            TransactionView::new_advanced_builder()
                .output(CellOutput::default())
                .output_data(ckb_types::packed::Bytes::default())
                .build()
        )
        .await,
        TxStatus::Committed(..),
    ));
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_submit_mocked_secp256k1_tx() {
    let actor = create_mock_chain_actor().await;
    let capacity = 100u64;
    let output = CellOutput::new_builder()
        .capacity(capacity)
        .lock(get_script_by_contract(
            Contract::Secp256k1Lock,
            &b"whatever1"[..],
        ))
        .build();
    let tx = TransactionView::new_advanced_builder()
        .output(output)
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    assert!(matches!(
        submit_tx(actor.clone(), tx.clone()).await,
        TxStatus::Committed(..)
    ));
    let out_point = tx.output_pts_iter().next().unwrap();
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(
            get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock])
                .await
                .expect("get cell deps"),
        )
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity)
                .lock(get_script_by_contract(
                    Contract::FundingLock,
                    &b"whatever2"[..],
                ))
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    assert!(matches!(
        submit_tx(actor, tx).await,
        TxStatus::Committed(..)
    ));
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_repeatedly_consume_the_same_cell() {
    let actor = create_mock_chain_actor().await;
    let capacity = 100u64;
    let output = CellOutput::new_builder()
        .capacity(capacity)
        .lock(get_script_by_contract(
            Contract::Secp256k1Lock,
            &b"whatever1"[..],
        ))
        .build();
    let tx = TransactionView::new_advanced_builder()
        .output(output)
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    assert!(matches!(
        submit_tx(actor.clone(), tx.clone()).await,
        TxStatus::Committed(..)
    ));
    let out_point = tx.output_pts_iter().next().unwrap();
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(
            get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock])
                .await
                .expect("get cell deps"),
        )
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity)
                .lock(get_script_by_contract(
                    Contract::FundingLock,
                    &b"whatever2"[..],
                ))
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    assert!(matches!(
        submit_tx(actor.clone(), tx).await,
        TxStatus::Committed(..)
    ));
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(
            get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock])
                .await
                .expect("get cell deps"),
        )
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity)
                .lock(get_script_by_contract(
                    Contract::FundingLock,
                    &b"whatever3"[..],
                ))
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    assert!(matches!(submit_tx(actor, tx).await, TxStatus::Rejected(_)));
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_submit_malformed_commitment_tx() {
    let actor = create_mock_chain_actor().await;
    let capacity = 100u64;
    let output = CellOutput::new_builder()
        .capacity(capacity)
        .lock(get_script_by_contract(
            Contract::FundingLock,
            &b"whatever1"[..],
        ))
        .build();
    let tx = TransactionView::new_advanced_builder()
        .output(output)
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    assert!(matches!(
        submit_tx(actor.clone(), tx.clone()).await,
        TxStatus::Committed(..)
    ));
    let out_point = tx.output_pts_iter().next().unwrap();
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(
            get_cell_deps_by_contracts(vec![Contract::FundingLock])
                .await
                .expect("get cell deps"),
        )
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity)
                .lock(get_script_by_contract(
                    Contract::CommitmentLock,
                    &b"whatever2"[..],
                ))
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    assert!(matches!(submit_tx(actor, tx).await, TxStatus::Rejected(_)));
}

const GET_LIVE_CELL_TIMEOUT_MS: u64 = 1000;

async fn get_live_cell(
    actor: &ActorRef<CkbChainMessage>,
    outpoint: OutPoint,
) -> Result<Option<LiveCell>, ckb_sdk::RpcError> {
    call_t!(
        actor.clone(),
        CkbChainMessage::GetLiveCell,
        GET_LIVE_CELL_TIMEOUT_MS,
        outpoint
    )
    .expect("chain actor alive")
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_get_live_cell_returns_output_and_exact_data() {
    let (actor, shared_state) = create_mock_chain_actor_with_shared_state().await;
    let data: Vec<u8> = (0..16u8).collect();
    let output = CellOutput::new_builder().capacity(100u64).build();
    let outpoint = shared_state
        .write()
        .unwrap()
        .context
        .create_cell(output.clone(), Bytes::from(data.clone()));

    let live = get_live_cell(&actor, outpoint).await.unwrap();
    let live = live.expect("live cell should be found");

    assert_eq!(live.output, output);
    assert_eq!(live.data.raw_data().to_vec(), data);
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_get_live_cell_missing_returns_none() {
    let actor = create_mock_chain_actor().await;
    let live = get_live_cell(&actor, OutPoint::default()).await.unwrap();
    assert!(live.is_none());
}

async fn assert_non_live_outpoint_returns_none(status: CellStatus) {
    let (actor, shared_state) = create_mock_chain_actor_with_shared_state().await;
    let output = CellOutput::new_builder().capacity(100u64).build();
    let outpoint = {
        let mut state = shared_state.write().unwrap();
        let outpoint = state
            .context
            .create_cell(output, Bytes::from(vec![7u8; 16]));
        state.cell_status.insert(outpoint.clone(), status);
        outpoint
    };

    let live = get_live_cell(&actor, outpoint).await.unwrap();
    assert!(live.is_none());
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_get_live_cell_consumed_returns_none() {
    assert_non_live_outpoint_returns_none(CellStatus::Consumed).await;
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_get_live_cell_pending_returns_none() {
    assert_non_live_outpoint_returns_none(CellStatus::Pending).await;
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn test_get_live_cell_rejected_returns_none() {
    assert_non_live_outpoint_returns_none(CellStatus::Rejected).await;
}

async fn spawn_controlled_mock_chain_actor(
    controller: &MockChainController,
) -> ActorRef<CkbChainMessage> {
    Actor::spawn(
        None,
        MockChainActor::new(),
        (None, controller.shared_state()),
    )
    .await
    .expect("start mock chain actor")
    .0
}

async fn submit_funded_cell(
    controller: &MockChainController,
    actor: &ActorRef<CkbChainMessage>,
) -> (Hash256, OutPoint) {
    let output = CellOutput::new_builder()
        .capacity(100u64)
        .lock(get_script_by_contract(
            Contract::Secp256k1Lock,
            &b"fund"[..],
        ))
        .build();
    let tx = TransactionView::new_advanced_builder()
        .output(output)
        .output_data(ckb_types::packed::Bytes::default())
        .build();
    let tx_hash: Hash256 = tx.hash().into();
    let outpoint = tx.output_pts_iter().next().expect("one output");
    call_t!(actor.clone(), CkbChainMessage::SendTx, 1000, tx)
        .expect("chain actor alive")
        .expect("funding tx accepted");
    controller.commit(tx_hash).expect("commit funding tx");
    (tx_hash, outpoint)
}

async fn build_spending_tx(outpoint: OutPoint) -> TransactionView {
    let cell_deps = get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock])
        .await
        .expect("get cell deps");
    TransactionView::new_advanced_builder()
        .cell_deps(cell_deps)
        .input(CellInput::new_builder().previous_output(outpoint).build())
        .output(
            CellOutput::new_builder()
                .capacity(50u64)
                .lock(get_script_by_contract(
                    Contract::Secp256k1Lock,
                    &b"spend"[..],
                ))
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build()
}

async fn submit_funded_cells(
    controller: &MockChainController,
    actor: &ActorRef<CkbChainMessage>,
    count: usize,
) -> Vec<OutPoint> {
    let output = CellOutput::new_builder()
        .capacity(100u64)
        .lock(get_script_by_contract(
            Contract::Secp256k1Lock,
            &b"fund"[..],
        ))
        .build();
    let tx = (0..count)
        .fold(TransactionView::new_advanced_builder(), |builder, _| {
            builder
                .output(output.clone())
                .output_data(ckb_types::packed::Bytes::default())
        })
        .build();
    let tx_hash: Hash256 = tx.hash().into();
    let outpoints = tx.output_pts_iter().collect();
    call_t!(actor.clone(), CkbChainMessage::SendTx, 1000, tx)
        .expect("chain actor alive")
        .expect("funding tx accepted");
    controller.commit(tx_hash).expect("commit funding tx");
    outpoints
}

async fn build_multi_input_spending_tx(outpoints: &[OutPoint]) -> TransactionView {
    let cell_deps = get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock])
        .await
        .expect("get cell deps");
    outpoints
        .iter()
        .fold(
            TransactionView::new_advanced_builder().cell_deps(cell_deps),
            |builder, outpoint| {
                builder.input(
                    CellInput::new_builder()
                        .previous_output(outpoint.clone())
                        .build(),
                )
            },
        )
        .output(
            CellOutput::new_builder()
                .capacity(50u64)
                .lock(get_script_by_contract(
                    Contract::Secp256k1Lock,
                    &b"spend"[..],
                ))
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build()
}

async fn submit_pending_tx(actor: &ActorRef<CkbChainMessage>, tx: TransactionView) -> Hash256 {
    let tx_hash: Hash256 = tx.hash().into();
    call_t!(actor.clone(), CkbChainMessage::SendTx, 1000, tx)
        .expect("chain actor alive")
        .expect("pending transaction accepted");
    tx_hash
}

fn spend_tracer(
    outpoint: OutPoint,
    callback: oneshot::Sender<Result<CkbOutPointSpendTracingResult, String>>,
) -> CkbOutPointSpendTracer {
    CkbOutPointSpendTracer {
        outpoint,
        lock_script: get_script_by_contract(Contract::Secp256k1Lock, &b"fund"[..]),
        confirmations: 1,
        callback: callback.into(),
    }
}

async fn expect_spend_result(
    tracer_rx: oneshot::Receiver<Result<CkbOutPointSpendTracingResult, String>>,
) -> CkbOutPointSpendTracingResult {
    tokio::time::timeout(Duration::from_secs(1), tracer_rx)
        .await
        .expect("outpoint spend tracer timed out")
        .expect("outpoint spend tracer callback dropped")
        .expect("outpoint spend tracing failed")
}

async fn assert_no_spend_delivery(
    tracer_rx: &mut oneshot::Receiver<Result<CkbOutPointSpendTracingResult, String>>,
) {
    match tokio::time::timeout(Duration::from_millis(200), tracer_rx).await {
        Err(_) => {}
        Ok(Ok(result)) => panic!("tracer unexpectedly delivered a result: {:?}", result),
        Ok(Err(_)) => panic!("tracer callback closed before explicit removal"),
    }
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn shared_mock_chain_outpoint_spend_notifies_only_after_commit() {
    let controller = MockChainController::new();
    let actor = spawn_controlled_mock_chain_actor(&controller).await;

    let (_fund_hash, outpoint) = submit_funded_cell(&controller, &actor).await;
    let spend_tx = build_spending_tx(outpoint.clone()).await;
    let spend_hash = submit_pending_tx(&actor, spend_tx.clone()).await;

    let (tracer_tx, mut tracer_rx) = oneshot::channel();
    actor
        .send_message(CkbChainMessage::CreateOutPointSpendTracer(spend_tracer(
            outpoint.clone(),
            tracer_tx,
        )))
        .expect("create outpoint spend tracer");

    assert!(matches!(
        controller.transaction_status(spend_hash),
        Some(TxStatus::Pending)
    ));
    assert!(get_live_cell(&actor, outpoint.clone())
        .await
        .unwrap()
        .is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut tracer_rx)
            .await
            .is_err(),
        "tracer fired while spend was still pending"
    );

    controller.commit(spend_hash).expect("commit spending tx");

    let result = expect_spend_result(tracer_rx).await;
    assert_eq!(result.outpoint, outpoint);
    assert_eq!(result.spending_transaction, spend_tx);
    assert_eq!(result.input_index, 0);
    assert_eq!(result.block_number, 0);
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn shared_mock_chain_outpoint_spend_reject_does_not_notify_and_leaves_input_live() {
    let controller = MockChainController::new();
    let actor = spawn_controlled_mock_chain_actor(&controller).await;

    let (_fund_hash, outpoint) = submit_funded_cell(&controller, &actor).await;
    let spend_tx = build_spending_tx(outpoint.clone()).await;
    let spend_hash = submit_pending_tx(&actor, spend_tx).await;

    let (tracer_tx, mut tracer_rx) = oneshot::channel();
    actor
        .send_message(CkbChainMessage::CreateOutPointSpendTracer(spend_tracer(
            outpoint.clone(),
            tracer_tx,
        )))
        .expect("create outpoint spend tracer");

    controller
        .reject(spend_hash, "controlled rejection")
        .expect("reject pending transaction");

    assert_no_spend_delivery(&mut tracer_rx).await;
    actor
        .send_message(CkbChainMessage::RemoveOutPointSpendTracers(
            outpoint.clone(),
        ))
        .expect("remove outpoint spend tracer");
    assert!(get_live_cell(&actor, outpoint).await.unwrap().is_some());
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn shared_mock_chain_outpoint_spend_registration_after_commit_finds_historical_spender() {
    let controller = MockChainController::new();
    let actor = spawn_controlled_mock_chain_actor(&controller).await;

    let (_fund_hash, outpoint) = submit_funded_cell(&controller, &actor).await;
    let spend_tx = build_spending_tx(outpoint.clone()).await;
    let spend_hash = submit_pending_tx(&actor, spend_tx.clone()).await;
    controller.commit(spend_hash).expect("commit spending tx");

    let (tracer_tx, tracer_rx) = oneshot::channel();
    actor
        .send_message(CkbChainMessage::CreateOutPointSpendTracer(spend_tracer(
            outpoint.clone(),
            tracer_tx,
        )))
        .expect("create outpoint spend tracer");

    let result = expect_spend_result(tracer_rx).await;
    assert_eq!(result.outpoint, outpoint);
    assert_eq!(result.spending_transaction, spend_tx);
    assert_eq!(result.input_index, 0);
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn shared_mock_chain_outpoint_spend_registration_commit_race_cannot_lose_observation() {
    let controller = MockChainController::new();
    let actor = spawn_controlled_mock_chain_actor(&controller).await;

    let (_fund_hash, outpoint) = submit_funded_cell(&controller, &actor).await;
    let spend_tx = build_spending_tx(outpoint.clone()).await;
    let spend_hash = submit_pending_tx(&actor, spend_tx).await;

    let registration = controller.pause_next_tracer_registration();
    let (tracer_tx, tracer_rx) = oneshot::channel();
    actor
        .send_message(CkbChainMessage::CreateOutPointSpendTracer(spend_tracer(
            outpoint.clone(),
            tracer_tx,
        )))
        .expect("create outpoint spend tracer");
    tokio::time::timeout(Duration::from_secs(1), registration.wait_until_paused())
        .await
        .expect("tracer registration did not pause");

    controller
        .commit(spend_hash)
        .expect("commit while tracer registration is paused");
    registration.resume();

    let result = expect_spend_result(tracer_rx).await;
    assert_eq!(result.outpoint, outpoint);
    assert_eq!(result.input_index, 0);
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn shared_mock_chain_outpoint_spend_removal_prevents_late_delivery() {
    let controller = MockChainController::new();
    let actor = spawn_controlled_mock_chain_actor(&controller).await;

    let (_fund_hash, outpoint) = submit_funded_cell(&controller, &actor).await;
    let spend_tx = build_spending_tx(outpoint.clone()).await;
    let spend_hash = submit_pending_tx(&actor, spend_tx).await;

    let (tracer_tx, tracer_rx) = oneshot::channel();
    actor
        .send_message(CkbChainMessage::CreateOutPointSpendTracer(spend_tracer(
            outpoint.clone(),
            tracer_tx,
        )))
        .expect("create outpoint spend tracer");
    actor
        .send_message(CkbChainMessage::RemoveOutPointSpendTracers(
            outpoint.clone(),
        ))
        .expect("remove outpoint spend tracer");
    // Barrier: GetLiveCell is processed after the removal (FIFO mailbox), so the
    // stored replier task has been aborted before the commit below.
    let _ = get_live_cell(&actor, outpoint.clone()).await.unwrap();

    controller.commit(spend_hash).expect("commit spending tx");

    match tokio::time::timeout(Duration::from_millis(200), tracer_rx).await {
        Ok(Ok(result)) => panic!("tracer unexpectedly delivered after removal: {:?}", result),
        Ok(Err(_)) | Err(_) => {}
    }
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test(flavor = "current_thread"))]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn shared_mock_chain_outpoint_spend_burst_does_not_lose_watched_input() {
    const INPUT_COUNT: usize = 32;
    const WATCHED_INPUT_INDEX: usize = 20;

    let controller = MockChainController::new();
    let actor = spawn_controlled_mock_chain_actor(&controller).await;
    let outpoints = submit_funded_cells(&controller, &actor, INPUT_COUNT).await;
    let watched_outpoint = outpoints[WATCHED_INPUT_INDEX].clone();
    let spend_tx = build_multi_input_spending_tx(&outpoints).await;
    let spend_hash = submit_pending_tx(&actor, spend_tx.clone()).await;

    let (tracer_tx, tracer_rx) = oneshot::channel();
    actor
        .send_message(CkbChainMessage::CreateOutPointSpendTracer(spend_tracer(
            watched_outpoint.clone(),
            tracer_tx,
        )))
        .expect("create outpoint spend tracer");
    // FIFO barrier: the tracer is registered before the synchronous burst commit.
    let _ = get_live_cell(&actor, watched_outpoint.clone())
        .await
        .unwrap();

    controller.commit(spend_hash).expect("commit burst spend");

    let result = expect_spend_result(tracer_rx).await;
    assert_eq!(result.outpoint, watched_outpoint);
    assert_eq!(result.spending_transaction, spend_tx);
    assert_eq!(result.input_index, WATCHED_INPUT_INDEX);
}
