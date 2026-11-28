use super::first_input_tx_hash;
use ckb_jsonrpc_types::{BlockNumber, Uint32};
use ckb_sdk::rpc::ckb_indexer::{CellType, Tx, TxWithCell};
use ckb_types::H256;

fn build_tx(io_type: CellType, tx_hash: u8) -> Tx {
    Tx::Ungrouped(TxWithCell {
        tx_hash: H256::from([tx_hash; 32]),
        block_number: BlockNumber::from(1_u64),
        tx_index: Uint32::from(0_u32),
        io_index: Uint32::from(0_u32),
        io_type,
    })
}

#[test]
fn test_find_first_input_tx_hash_returns_first_input_from_page() {
    let output_only_page = vec![build_tx(CellType::Output, 1), build_tx(CellType::Output, 2)];
    let input_page = vec![
        build_tx(CellType::Output, 3),
        build_tx(CellType::Input, 4),
        build_tx(CellType::Input, 5),
    ];

    assert_eq!(first_input_tx_hash(&output_only_page), None);
    assert_eq!(first_input_tx_hash(&input_page), Some(H256::from([4; 32])));
}
