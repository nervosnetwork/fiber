use super::channel::{ChannelActorState, FUNDING_CELL_WITNESS_LEN};
use super::config::{DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT, DEFAULT_UDT_MINIMAL_CKB_AMOUNT};
use crate::ckb::contracts::{get_cell_deps, get_script_by_contract, Contract};
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

pub(crate) fn commitment_tx_size(
    udt_type_script: &Option<Script>,
    shutdown_scripts: Option<(Script, Script)>,
) -> usize {
    let (script_a, script_b) =
        if let Some((local_shutdown_script, remote_shutdown_script)) = shutdown_scripts {
            (local_shutdown_script, remote_shutdown_script)
        } else {
            let script = get_script_by_contract(Contract::Secp256k1Lock, &[0u8; 20]);
            (script.clone(), script)
        };

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

pub(crate) fn calculate_commitment_tx_fee(fee_rate: u64, udt_type_script: &Option<Script>) -> u64 {
    debug!(
        "calculate_commitment_tx_fee: {} udt_script: {:?}",
        fee_rate, udt_type_script
    );
    let fee_rate: FeeRate = FeeRate::from_u64(fee_rate);

    let tx_size = commitment_tx_size(udt_type_script, None) as u64;
    let res = fee_rate.fee(tx_size).as_u64();
    debug!("calculate_commitment_tx_fee return: {}", res);
    res
}

/// Note: the shutdown scripts are optional, if not provided, the default lock script will be used
pub(crate) fn calculate_shutdown_tx_fee(fee_rate: u64, state: &ChannelActorState) -> u64 {
    let udt_type_script = &state.funding_udt_type_script;
    let shutdown_scripts = (
        state
            .remote_shutdown_script
            .clone()
            .unwrap_or(state.get_default_remote_funding_script()),
        state
            .local_shutdown_script
            .clone()
            .unwrap_or(state.get_default_local_funding_script()),
    );
    debug!(
        "calculate_shutdown_tx_fee: {} udt_script: {:?}",
        fee_rate, udt_type_script
    );
    let fee_rate: FeeRate = FeeRate::from_u64(fee_rate);
    let tx_size = commitment_tx_size(udt_type_script, Some(shutdown_scripts)) as u64;
    let res = fee_rate.fee(tx_size).as_u64();
    debug!("calculate_shutdown_tx_fee return: {}", res);
    res
}
