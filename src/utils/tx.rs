use ckb_hash::blake2b_256;
use ckb_types::{core::TransactionView, prelude::*};

/// Empty cell_deps before compute transaction's signing message,
/// to maintain signature validity during script upgrades.
/// https://github.com/nervosnetwork/fiber-scripts/pull/17
pub fn compute_tx_message(tx: &TransactionView) -> [u8; 32] {
    let tx = tx
        .data()
        .raw()
        .as_builder()
        .cell_deps(Default::default())
        .build();
    blake2b_256(tx.as_slice())
}
