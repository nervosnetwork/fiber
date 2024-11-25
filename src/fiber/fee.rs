use super::channel::FUNDING_CELL_WITNESS_LEN;
use crate::ckb::contracts::{get_cell_deps, get_script_by_contract, Contract};
use ckb_types::core::TransactionBuilder;
use ckb_types::packed::{Bytes, Script};
use ckb_types::prelude::Builder;
use ckb_types::{
    core::FeeRate,
    packed::{CellInput, CellOutput},
    prelude::Pack,
};
use molecule::prelude::Entity;
use tracing::debug;

fn commitment_tx_size(udt_type_script: &Option<Script>) -> usize {
    // when there is pending htlcs, the commitment lock args will be 56 bytes, otherwise 46 bytes.
    // to simplify the calculation, we use hardcoded 56 bytes here.
    let commitment_lock_script = get_script_by_contract(Contract::CommitmentLock, &[0u8; 56]);
    let cell_deps = get_cell_deps(vec![Contract::FundingLock], udt_type_script);

    let (output, output_data) = if let Some(type_script) = udt_type_script {
        let output = CellOutput::new_builder()
            .lock(commitment_lock_script)
            .type_(Some(type_script.clone()).pack())
            .capacity(0.pack())
            .build();
        let output_data = (0_u128).to_le_bytes().pack();
        (output, output_data)
    } else {
        let output = CellOutput::new_builder()
            .capacity(0.pack())
            .lock(commitment_lock_script)
            .build();
        (output, Bytes::default())
    };

    let mock_commitment_tx = TransactionBuilder::default()
        .cell_deps(cell_deps)
        .input(CellInput::default())
        .output(output)
        .output_data(output_data)
        .set_witnesses(vec![[0; FUNDING_CELL_WITNESS_LEN].pack()])
        .build();
    mock_commitment_tx.data().serialized_size_in_block()
}

pub(crate) fn shutdown_tx_size(
    udt_type_script: &Option<Script>,
    shutdown_scripts: (Script, Script),
) -> usize {
    let (script_a, script_b) = shutdown_scripts;
    let cell_deps = get_cell_deps(vec![Contract::FundingLock], udt_type_script);

    let (outputs, outputs_data) = if let Some(type_script) = udt_type_script {
        let output_a = CellOutput::new_builder()
            .lock(script_a)
            .type_(Some(type_script.clone()).pack())
            .capacity(0.pack())
            .build();
        let output_b = CellOutput::new_builder()
            .lock(script_b)
            .type_(Some(type_script.clone()).pack())
            .capacity(0.pack())
            .build();
        let dummy_output_data = (0_u128).to_le_bytes().pack();

        let outputs = [output_a, output_b];
        let outputs_data = [dummy_output_data.clone(), dummy_output_data];
        (outputs, outputs_data.to_vec())
    } else {
        let output_a = CellOutput::new_builder()
            .capacity(0.pack())
            .lock(script_a)
            .build();
        let output_b = CellOutput::new_builder()
            .capacity(0.pack())
            .lock(script_b)
            .build();
        let outputs = [output_a, output_b];
        (outputs, vec![Bytes::default(); 2])
    };

    let mock_shutdown_tx = TransactionBuilder::default()
        .cell_deps(cell_deps)
        .input(CellInput::default())
        .set_outputs(outputs.to_vec())
        .set_outputs_data(outputs_data.to_vec())
        .set_witnesses(vec![[0; FUNDING_CELL_WITNESS_LEN].pack()])
        .build();
    mock_shutdown_tx.data().serialized_size_in_block()
}

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

pub(crate) fn calculate_shutdown_tx_fee(
    fee_rate: u64,
    udt_type_script: &Option<Script>,
    shutdown_scripts: (Script, Script),
) -> u64 {
    let fee_rate: FeeRate = FeeRate::from_u64(fee_rate);
    let tx_size = shutdown_tx_size(udt_type_script, shutdown_scripts) as u64;
    fee_rate.fee(tx_size).as_u64()
}

pub(crate) fn calculate_tlc_forward_fee(amount: u128, fee_proportational_millionths: u128) -> u128 {
    let fee = fee_proportational_millionths * amount;
    let base_fee = fee / 1_000_000;
    let remainder = fee % 1_000_000;
    if remainder > 0 {
        base_fee + 1
    } else {
        base_fee
    }
}
