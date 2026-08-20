use super::test_utils::{create_mock_chain_actor_with_shared_state, submit_tx, CellStatus};
use crate::ckb::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};
use crate::ckb::{CkbChainMessage, LiveCell};
use crate::create_mock_chain_actor;
use ckb_types::bytes::Bytes;
use ckb_types::core::tx_pool::TxStatus;
use ckb_types::core::TransactionView;
use ckb_types::packed::{CellInput, CellOutput, OutPoint};
use ckb_types::prelude::Builder;
use molecule::prelude::Entity;
use ractor::{call_t, ActorRef};

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
