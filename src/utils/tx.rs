use ckb_hash::new_blake2b;
use ckb_types::{core::TransactionView, prelude::*};

/// Compute a transaction's message by hashing its inputs and outputs.
/// This is used instead of tx hash to maintain signature validity during script upgrades.
/// https://github.com/nervosnetwork/fiber-scripts/pull/17
pub fn compute_tx_message(tx: &TransactionView) -> [u8; 32] {
    let mut hasher = new_blake2b();
    // iter input and hash outpoint
    for input in tx.inputs() {
        hasher.update(input.previous_output().as_slice());
    }
    for (output, data) in tx.outputs_with_data_iter() {
        hasher.update(output.as_slice());
        hasher.update((data.len() as u32).to_le_bytes().as_slice());
        hasher.update(&data);
    }
    let mut hash_result = [0u8; 32];
    hasher.finalize(&mut hash_result);
    hash_result
}
