use super::channel::FUNDING_CELL_WITNESS_LEN;
use crate::ckb::contracts::{
    check_udt_script, get_cell_deps_count, get_script_by_contract, Contract,
};
use crate::fiber::channel::{
    occupied_capacity, tlc_expiry_delay, ProcessingChannelError, ProcessingChannelResult,
    DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE, MAX_COMMITMENT_DELAY_EPOCHS,
    MIN_COMMITMENT_DELAY_EPOCHS, SYS_MAX_TLC_NUMBER_IN_FLIGHT,
};
use crate::fiber::config::{MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA};
use ckb_types::core::{EpochNumberWithFraction, TransactionBuilder};
use ckb_types::packed::{Bytes, CellDep, Script};
use ckb_types::prelude::{Builder, PackVec};
use ckb_types::{
    core::FeeRate,
    packed::{CellInput, CellOutput},
    prelude::Pack,
};
use molecule::prelude::Entity;

fn commitment_tx_size(udt_type_script: &Option<Script>) -> usize {
    let commitment_lock_script = get_script_by_contract(Contract::CommitmentLock, &[0u8; 57]);
    let cell_deps_count = get_cell_deps_count(vec![Contract::FundingLock], udt_type_script);
    let cell_deps = vec![CellDep::default(); cell_deps_count].pack();

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
    let cell_deps_count = get_cell_deps_count(vec![Contract::FundingLock], udt_type_script);
    let cell_deps = vec![CellDep::default(); cell_deps_count].pack();

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
    let fee_rate: FeeRate = FeeRate::from_u64(fee_rate);
    let tx_size = commitment_tx_size(udt_type_script) as u64;
    fee_rate.fee(tx_size).as_u64()
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

pub(crate) fn calculate_tlc_forward_fee(
    amount: u128,
    fee_proportational_millionths: u128,
) -> Result<u128, String> {
    let fee = fee_proportational_millionths
        .checked_mul(amount)
        .ok_or_else(|| {
            format!(
                "fee_proportational_millionths {} * amount {} overflow",
                fee_proportational_millionths, amount
            )
        })?;
    let base_fee = fee / 1_000_000;
    let remainder = fee % 1_000_000;
    if remainder > 0 {
        Ok(base_fee + 1)
    } else {
        Ok(base_fee)
    }
}

pub(crate) fn check_open_channel_parameters(
    udt_type_script: &Option<Script>,
    shutdown_script: &Script,
    reserved_ckb_amount: u64,
    funding_fee_rate: u64,
    commitment_fee_rate: u64,
    commitment_delay_epoch: u64,
    max_tlc_number_in_flight: u64,
) -> ProcessingChannelResult {
    if let Some(udt_type_script) = udt_type_script {
        if !check_udt_script(udt_type_script) {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Invalid UDT type script: {:?}",
                udt_type_script
            )));
        }
    }

    // reserved_ckb_amount
    let occupied_capacity = occupied_capacity(shutdown_script, udt_type_script)?.as_u64();
    if reserved_ckb_amount < occupied_capacity {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Reserved CKB amount {} is less than {}",
            reserved_ckb_amount, occupied_capacity,
        )));
    }

    // funding_fee_rate
    if funding_fee_rate < DEFAULT_FEE_RATE {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Funding fee rate is less than {}",
            DEFAULT_FEE_RATE,
        )));
    }

    // commitment_fee_rate
    if commitment_fee_rate < DEFAULT_COMMITMENT_FEE_RATE {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Commitment fee rate is less than {}",
            DEFAULT_COMMITMENT_FEE_RATE,
        )));
    }
    let commitment_fee = calculate_commitment_tx_fee(commitment_fee_rate, udt_type_script);
    let reserved_fee = reserved_ckb_amount - occupied_capacity;
    if commitment_fee * 2 > reserved_fee {
        return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee {} which calculated by commitment fee rate {} is larger than half of reserved fee {}",
                commitment_fee, commitment_fee_rate, reserved_fee
            )));
    }

    // commitment_delay_epoch
    let epoch = EpochNumberWithFraction::from_full_value_unchecked(commitment_delay_epoch);
    if !epoch.is_well_formed() {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Commitment delay epoch {} is not a valid value",
            commitment_delay_epoch,
        )));
    }

    let min = EpochNumberWithFraction::new(MIN_COMMITMENT_DELAY_EPOCHS, 0, 1);
    if epoch < min {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Commitment delay epoch {} is less than the minimal value {}",
            epoch, min
        )));
    }

    let max = EpochNumberWithFraction::new(MAX_COMMITMENT_DELAY_EPOCHS, 0, 1);
    if epoch > max {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Commitment delay epoch {} is greater than the maximal value {}",
            epoch, max
        )));
    }

    // max_tlc_number_in_flight
    if max_tlc_number_in_flight > SYS_MAX_TLC_NUMBER_IN_FLIGHT {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Max TLC number in flight {} is greater than the system maximal value {}",
            max_tlc_number_in_flight, SYS_MAX_TLC_NUMBER_IN_FLIGHT
        )));
    }

    Ok(())
}

pub(crate) fn check_tlc_delta_with_epochs(
    tlc_expiry_delta: u64,
    commitment_delay_epoch: u64,
) -> ProcessingChannelResult {
    if tlc_expiry_delta < MIN_TLC_EXPIRY_DELTA {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "TLC expiry delta is too small, expect larger than {}, got {}",
            MIN_TLC_EXPIRY_DELTA, tlc_expiry_delta
        )));
    }
    if tlc_expiry_delta > MAX_PAYMENT_TLC_EXPIRY_LIMIT {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "TLC expiry delta is too large, expected to be smaller than {}, got {}",
            MAX_PAYMENT_TLC_EXPIRY_LIMIT, tlc_expiry_delta
        )));
    }

    let epochs = EpochNumberWithFraction::from_full_value(commitment_delay_epoch);
    if !epochs.is_well_formed() {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Commitment delay epoch {} is not a valid value",
            commitment_delay_epoch,
        )));
    }

    let min = EpochNumberWithFraction::new(MIN_COMMITMENT_DELAY_EPOCHS, 0, 1);
    if epochs < min {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Commitment delay epoch {} is less than the minimal value {}",
            epochs, min
        )));
    }

    let max = EpochNumberWithFraction::new(MAX_COMMITMENT_DELAY_EPOCHS, 0, 1);
    if epochs > max {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "Commitment delay epoch {} is greater than the maximal value {}",
            epochs, max
        )));
    }

    let epoch_delay_milliseconds = tlc_expiry_delay(&epochs);
    if tlc_expiry_delta < epoch_delay_milliseconds {
        return Err(ProcessingChannelError::InvalidParameter(format!(
            "TLC expiry delta {} is smaller than 2/3 commitment_delay_epoch delay {}",
            tlc_expiry_delta, epoch_delay_milliseconds
        )));
    }

    Ok(())
}
