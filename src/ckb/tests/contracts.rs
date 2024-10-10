use ckb_types::{core::TransactionView, packed::CellOutput, prelude::Pack};
use molecule::prelude::{Builder, Entity};

use crate::ckb::contracts::{get_script_by_contract, Contract, ContractsContext, MockContext};

// This test is to ensure that the same transaction is generated for different mock contexts.
// If different transactions are generated, then the mock context is not deterministic.
// We can't use the mock context to verify the validity of the transactions.
#[test]
fn test_same_tx_for_different_mock_context() {
    let mock_ctx1 = MockContext::new();
    let mock_ctx2 = MockContext::new();
    let capacity = 100u64;
    let output = CellOutput::new_builder()
        .capacity(capacity.pack())
        .lock(get_script_by_contract(
            Contract::FundingLock,
            &b"whatever"[..],
        ))
        .build();
    assert_eq!(
        TransactionView::new_advanced_builder()
            .cell_deps(ContractsContext::from(mock_ctx1).get_cell_deps(vec![Contract::FundingLock]))
            .output(output.clone())
            .output_data(Default::default())
            .build(),
        TransactionView::new_advanced_builder()
            .cell_deps(ContractsContext::from(mock_ctx2).get_cell_deps(vec![Contract::FundingLock]))
            .output(output)
            .output_data(Default::default())
            .build()
    );
}
