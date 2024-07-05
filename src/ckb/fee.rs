use super::channel::FUNDING_CELL_WITNESS_LEN;
use super::config::{DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT, DEFAULT_UDT_MINIMAL_CKB_AMOUNT};
use crate::ckb_chain::contracts::{get_cell_deps, get_script_by_contract, Contract};
use ckb_types::core::TransactionBuilder;
use ckb_types::packed::{Bytes, OutPoint, Script};
use ckb_types::prelude::Builder;
use ckb_types::{
    core::FeeRate,
    packed::{CellInput, CellOutput},
    prelude::Pack,
};
use molecule::prelude::Entity;
use tracing::debug;

pub(crate) fn default_minimal_ckb_amount(is_udt: bool) -> u64 {
    if is_udt {
        DEFAULT_UDT_MINIMAL_CKB_AMOUNT
    } else {
        DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT
    }
}

pub(crate) fn commitment_tx_size(udt_type_script: &Option<Script>) -> usize {
    // Note: here we must add args, otherwise the total transaction size will be different
    let dummy_script = get_script_by_contract(Contract::Secp256k1Lock, &[0u8; 20]);

    let cell_deps = get_cell_deps(vec![Contract::FundingLock], udt_type_script);

    let (outputs, outputs_data) = if let Some(type_script) = udt_type_script {
        let dummy_output = CellOutput::new_builder()
            .lock(dummy_script)
            .type_(Some(type_script.clone()).pack())
            .capacity(0.pack())
            .build();
        let dummy_output_data = (0_u128).to_le_bytes().pack();

        let outputs = [dummy_output.clone(), dummy_output];
        let outputs_data = [dummy_output_data.clone(), dummy_output_data];
        (outputs, outputs_data.to_vec())
    } else {
        let dummy_output = CellOutput::new_builder()
            .capacity((0 as u64).pack())
            .lock(dummy_script)
            .build();
        let outputs = [dummy_output.clone(), dummy_output];
        (outputs, vec![Bytes::default(); 2])
    };

    let mock_commitment_tx = TransactionBuilder::default()
        .cell_deps(cell_deps)
        .input(
            CellInput::new_builder()
                .previous_output(OutPoint::default())
                .build(),
        )
        .set_outputs(outputs.to_vec())
        .set_outputs_data(outputs_data.to_vec())
        .set_witnesses(vec![[0; FUNDING_CELL_WITNESS_LEN].pack()])
        .build();
    mock_commitment_tx.data().serialized_size_in_block()
}

/// Note: we use this function to calculate both commitment transaction and shutdown transaction
/// shutdown transaction is just a special commitment transaction.
pub(crate) fn calculate_commitment_tx_fee(fee_rate: u64, udt_type_script: &Option<Script>) -> u64 {
    debug!(
        "calculate_commitment_tx_fee: {} udt_script: {:?}",
        fee_rate, udt_type_script
    );
    let fee_rate: FeeRate = FeeRate::from_u64(fee_rate);

    let tx_size = commitment_tx_size(udt_type_script) as u64;
    let res = fee_rate.fee(tx_size).as_u64();
    debug!("calculate_commitment_tx_fee return: {}", res);
    res
}
