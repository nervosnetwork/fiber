use ckb_jsonrpc_types::Status;
use ckb_types::core::TransactionView;
use ckb_types::packed::{CellInput, CellOutput};
use ckb_types::prelude::{Builder, Pack};
use molecule::prelude::Entity;
use ractor::{Actor, ActorRef};

use super::test_utils::{submit_tx, MockChainActor};
use crate::ckb::actor::CkbChainMessage;
use crate::ckb::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};

pub async fn create_mock_chain_actor() -> ActorRef<CkbChainMessage> {
    Actor::spawn(None, MockChainActor::new(), ())
        .await
        .expect("start mock chain actor")
        .0
}

#[tokio::test]
async fn test_submit_empty_tx() {
    let actor = create_mock_chain_actor().await;
    assert_eq!(
        submit_tx(actor, TransactionView::new_advanced_builder().build()).await,
        Status::Committed
    );
}

#[tokio::test]
async fn test_submit_one_output_tx() {
    let actor = create_mock_chain_actor().await;
    assert_eq!(
        submit_tx(
            actor,
            TransactionView::new_advanced_builder()
                .output(CellOutput::default())
                .output_data(Default::default())
                .build()
        )
        .await,
        Status::Committed
    );
}

#[tokio::test]
async fn test_submit_mocked_secp256k1_tx() {
    let actor = create_mock_chain_actor().await;
    let capacity = 100u64;
    let output = CellOutput::new_builder()
        .capacity(capacity.pack())
        .lock(get_script_by_contract(
            Contract::Secp256k1Lock,
            &b"whatever1"[..],
        ))
        .build();
    let tx = TransactionView::new_advanced_builder()
        .output(output)
        .output_data(Default::default())
        .build();
    assert_eq!(
        submit_tx(actor.clone(), tx.clone()).await,
        Status::Committed
    );
    let out_point = tx.output_pts_iter().next().unwrap();
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock]))
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(get_script_by_contract(
                    Contract::FundingLock,
                    &b"whatever2"[..],
                ))
                .build(),
        )
        .output_data(Default::default())
        .build();
    assert_eq!(submit_tx(actor, tx).await, Status::Committed);
}

#[tokio::test]
async fn test_repeatedly_consume_the_same_cell() {
    let actor = create_mock_chain_actor().await;
    let capacity = 100u64;
    let output = CellOutput::new_builder()
        .capacity(capacity.pack())
        .lock(get_script_by_contract(
            Contract::Secp256k1Lock,
            &b"whatever1"[..],
        ))
        .build();
    let tx = TransactionView::new_advanced_builder()
        .output(output)
        .output_data(Default::default())
        .build();
    assert_eq!(
        submit_tx(actor.clone(), tx.clone()).await,
        Status::Committed
    );
    let out_point = tx.output_pts_iter().next().unwrap();
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock]))
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(get_script_by_contract(
                    Contract::FundingLock,
                    &b"whatever2"[..],
                ))
                .build(),
        )
        .output_data(Default::default())
        .build();
    assert_eq!(submit_tx(actor.clone(), tx).await, Status::Committed);
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock]))
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(get_script_by_contract(
                    Contract::FundingLock,
                    &b"whatever3"[..],
                ))
                .build(),
        )
        .output_data(Default::default())
        .build();
    assert_eq!(submit_tx(actor, tx).await, Status::Rejected);
}

#[tokio::test]
async fn test_submit_malformed_commitment_tx() {
    let actor = create_mock_chain_actor().await;
    let capacity = 100u64;
    let output = CellOutput::new_builder()
        .capacity(capacity.pack())
        .lock(get_script_by_contract(
            Contract::FundingLock,
            &b"whatever1"[..],
        ))
        .build();
    let tx = TransactionView::new_advanced_builder()
        .output(output)
        .output_data(Default::default())
        .build();
    assert_eq!(
        submit_tx(actor.clone(), tx.clone()).await,
        Status::Committed
    );
    let out_point = tx.output_pts_iter().next().unwrap();
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(get_cell_deps_by_contracts(vec![Contract::FundingLock]))
        .input(
            CellInput::new_builder()
                .previous_output(out_point.clone())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(get_script_by_contract(
                    Contract::CommitmentLock,
                    &b"whatever2"[..],
                ))
                .build(),
        )
        .output_data(Default::default())
        .build();
    assert_eq!(submit_tx(actor, tx).await, Status::Rejected);
}
