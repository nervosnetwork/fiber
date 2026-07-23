use crate::ckb::funding::{
    validate_peer_funding_tx_complexity, MAX_PEER_ADDED_CELL_DEPS, MAX_PEER_ADDED_FUNDING_INPUTS,
    MAX_PEER_ADDED_FUNDING_OUTPUTS, MAX_PEER_FUNDING_TX_SERIALIZED_SIZE,
};
use crate::ckb::FundingError;
use ckb_types::{
    core::TransactionView,
    packed::{self, Byte32, CellDep, CellInput, CellOutput, OutPoint},
    prelude::*,
};

#[derive(Default)]
struct TxSpec {
    inputs: usize,
    outputs: usize,
    cell_deps: usize,
}

fn build_tx(spec: TxSpec) -> TransactionView {
    let inputs: Vec<CellInput> = (0..spec.inputs)
        .map(|i| CellInput::new(OutPoint::new(Byte32::default(), i as u32), 0))
        .collect();
    let outputs: Vec<CellOutput> = (0..spec.outputs)
        .map(|_| CellOutput::new_builder().build())
        .collect();
    let outputs_data: Vec<packed::Bytes> = (0..spec.outputs)
        .map(|_| packed::Bytes::default())
        .collect();
    let cell_deps: Vec<CellDep> = (0..spec.cell_deps)
        .map(|_| CellDep::new_builder().build())
        .collect();

    packed::Transaction::default()
        .as_advanced_builder()
        .set_inputs(inputs)
        .set_outputs(outputs)
        .set_outputs_data(outputs_data)
        .set_cell_deps(cell_deps)
        .build()
}

fn assert_limit_err(result: Result<(), FundingError>, needle: &str) {
    match result {
        Err(FundingError::PeerFundingTxExceedsLimit(msg)) => assert!(
            msg.contains(needle),
            "expected limit message to contain {needle:?}, got: {msg}"
        ),
        other => panic!("expected PeerFundingTxExceedsLimit, got: {other:?}"),
    }
}

#[test]
fn small_peer_delta_passes() {
    let local = build_tx(TxSpec {
        inputs: 1,
        outputs: 1,
        cell_deps: 1,
    });
    let remote = build_tx(TxSpec {
        inputs: 1 + MAX_PEER_ADDED_FUNDING_INPUTS,
        outputs: 1 + MAX_PEER_ADDED_FUNDING_OUTPUTS,
        cell_deps: 1 + MAX_PEER_ADDED_CELL_DEPS,
    });
    assert!(
        validate_peer_funding_tx_complexity(&local, &remote).is_ok(),
        "deltas exactly at the limits should be accepted"
    );
}

#[test]
fn peer_adds_funding_cell_and_change_passes() {
    let local = build_tx(TxSpec {
        outputs: 1,
        ..Default::default()
    });
    let remote = build_tx(TxSpec {
        outputs: 1 + MAX_PEER_ADDED_FUNDING_OUTPUTS,
        ..Default::default()
    });
    assert!(
        validate_peer_funding_tx_complexity(&local, &remote).is_ok(),
        "peer may add funding cell plus UDT and CKB change outputs beyond the local shell"
    );
}

#[test]
fn initiator_first_tx_update_ckb_only_passes() {
    let local = build_tx(TxSpec::default());
    let remote = build_tx(TxSpec {
        inputs: 2,
        outputs: 2,
        cell_deps: 2,
    });
    assert!(
        validate_peer_funding_tx_complexity(&local, &remote).is_ok(),
        "acceptor with no local tx yet should accept funding cell plus CKB change"
    );
}

#[test]
fn initiator_first_tx_update_udt_with_separate_change_outputs_passes() {
    let local = build_tx(TxSpec::default());
    let remote = build_tx(TxSpec {
        inputs: 2,
        outputs: 3,
        cell_deps: 2,
    });
    assert!(
        validate_peer_funding_tx_complexity(&local, &remote).is_ok(),
        "acceptor with no local tx yet should accept funding cell plus UDT and CKB change"
    );
}

#[test]
fn too_many_peer_inputs_rejected_without_rpc() {
    let local = build_tx(TxSpec {
        inputs: 1,
        ..Default::default()
    });
    let remote = build_tx(TxSpec {
        inputs: 1 + MAX_PEER_ADDED_FUNDING_INPUTS + 1,
        ..Default::default()
    });
    assert_limit_err(
        validate_peer_funding_tx_complexity(&local, &remote),
        "peer-added inputs",
    );
}

#[test]
fn too_many_peer_outputs_rejected() {
    let local = build_tx(TxSpec {
        outputs: 1,
        ..Default::default()
    });
    let remote = build_tx(TxSpec {
        outputs: 1 + MAX_PEER_ADDED_FUNDING_OUTPUTS + 1,
        ..Default::default()
    });
    assert_limit_err(
        validate_peer_funding_tx_complexity(&local, &remote),
        "peer-added outputs",
    );
}

#[test]
fn too_many_peer_cell_deps_rejected() {
    let local = build_tx(TxSpec {
        cell_deps: 1,
        ..Default::default()
    });
    let remote = build_tx(TxSpec {
        cell_deps: 1 + MAX_PEER_ADDED_CELL_DEPS + 1,
        ..Default::default()
    });
    assert_limit_err(
        validate_peer_funding_tx_complexity(&local, &remote),
        "peer-added cell deps",
    );
}

#[test]
fn oversized_serialized_tx_rejected() {
    let local = build_tx(TxSpec::default());
    // A single output carrying data larger than the serialized-size cap keeps
    // every per-element delta within bounds while exceeding the size budget.
    let big_data: packed::Bytes = vec![0u8; MAX_PEER_FUNDING_TX_SERIALIZED_SIZE + 1].pack();
    let remote = packed::Transaction::default()
        .as_advanced_builder()
        .set_outputs(vec![CellOutput::new_builder().build()])
        .set_outputs_data(vec![big_data])
        .build();
    assert_limit_err(
        validate_peer_funding_tx_complexity(&local, &remote),
        "serialized size",
    );
}
