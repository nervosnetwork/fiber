use crate::ckb::funding::{
    secp_sighash_placeholder_witness, verify_peer_funding_contribution, FundingContext,
    FundingTxBuilder, LiveCellsExclusionMap, PeerInputCell,
    SECP_SIGHASH_PLACEHOLDER_SIGNATURE_BYTES,
};
use crate::ckb::{is_secp_sighash_placeholder_witness, FundingError, FundingRequest, FundingTx};
use ckb_types::{
    core::{Capacity, TransactionView},
    packed::{self, CellInput, Script},
    prelude::*,
};
use std::collections::HashSet;

fn dummy_funding_context() -> FundingContext {
    let script = Script::default();
    FundingContext {
        rpc_url: String::new(),
        funding_source_lock_script: script.clone(),
        funding_source_lock_script_cell_deps: Vec::new(),
        funding_cell_lock_script: script,
        funding_udt_type_script: None,
        allow_peer_funding_source_lock: false,
    }
}

fn dummy_funding_builder(
    funding_tx: FundingTx,
    request: FundingRequest,
    context: FundingContext,
) -> FundingTxBuilder {
    FundingTxBuilder {
        funding_tx,
        request,
        context,
    }
}

fn out_point(seed: u8, index: u32) -> packed::OutPoint {
    packed::OutPoint::new(packed::Byte32::from_slice(&[seed; 32]).unwrap(), index)
}

fn funding_tx_with_inputs(out_points: &[packed::OutPoint]) -> FundingTx {
    let inputs: Vec<CellInput> = out_points
        .iter()
        .map(|out_point| CellInput::new(out_point.clone(), 0))
        .collect();
    packed::Transaction::default()
        .as_advanced_builder()
        .set_inputs(inputs)
        .build()
        .into()
}

fn funding_tx_hash(funding_tx: &FundingTx) -> packed::Byte32 {
    funding_tx.as_ref().expect("funding tx present").hash()
}

fn excluded_out_points(map: &LiveCellsExclusionMap) -> HashSet<packed::OutPoint> {
    map.map
        .values()
        .flat_map(|exclusion| exclusion.input_out_points.iter().cloned())
        .collect()
}

fn has_exclusion(map: &LiveCellsExclusionMap, tx_hash: &packed::Byte32) -> bool {
    map.map.contains_key(tx_hash)
}

fn exclusion_map_is_empty(map: &LiveCellsExclusionMap) -> bool {
    map.map.is_empty()
}

fn test_output(capacity: u64, type_script: Option<Script>) -> packed::CellOutput {
    packed::CellOutput::new_builder()
        .capacity(Capacity::shannons(capacity).pack())
        .lock(Script::default())
        .type_(type_script.pack())
        .build()
}

fn test_tx(outputs: Vec<packed::CellOutput>, outputs_data: Vec<packed::Bytes>) -> TransactionView {
    packed::Transaction::default()
        .as_advanced_builder()
        .set_outputs(outputs)
        .set_outputs_data(outputs_data)
        .build()
}

fn peer_input_cell(
    capacity: u64,
    type_script: Option<Script>,
    data: packed::Bytes,
) -> PeerInputCell {
    PeerInputCell {
        output: test_output(capacity, type_script),
        data: data.raw_data(),
    }
}

#[test]
fn test_verify_peer_funding_contribution_rejects_insufficient_ckb_inputs() {
    let local_tx = test_tx(vec![], vec![]);
    let remote_tx = test_tx(
        vec![test_output(100, None), test_output(50, None)],
        vec![packed::Bytes::default(), packed::Bytes::default()],
    );

    let result = verify_peer_funding_contribution(
        &local_tx,
        &remote_tx,
        &[peer_input_cell(149, None, packed::Bytes::default())],
    );
    assert!(matches!(result, Err(FundingError::InvalidPeerFundingTx)));

    verify_peer_funding_contribution(
        &local_tx,
        &remote_tx,
        &[peer_input_cell(150, None, packed::Bytes::default())],
    )
    .expect("peer inputs cover funding cell and change");
}

#[test]
fn test_verify_peer_funding_contribution_rejects_excess_udt_change() {
    let udt_type_script = Script::new_builder()
        .code_hash(packed::Byte32::from_slice(&[1u8; 32]).unwrap())
        .hash_type(packed::Byte::new(0))
        .build();
    let local_tx = test_tx(vec![], vec![]);
    let remote_tx = test_tx(
        vec![
            test_output(14_200_000_000, Some(udt_type_script.clone())),
            test_output(14_200_000_000, Some(udt_type_script.clone())),
        ],
        vec![1500u128.to_le_bytes().pack(), 500u128.to_le_bytes().pack()],
    );

    let result = verify_peer_funding_contribution(
        &local_tx,
        &remote_tx,
        &[peer_input_cell(
            28_400_000_000,
            Some(udt_type_script.clone()),
            1999u128.to_le_bytes().pack(),
        )],
    );
    assert!(matches!(result, Err(FundingError::InvalidPeerFundingTx)));

    verify_peer_funding_contribution(
        &local_tx,
        &remote_tx,
        &[peer_input_cell(
            28_400_000_000,
            Some(udt_type_script),
            2000u128.to_le_bytes().pack(),
        )],
    )
    .expect("peer UDT inputs cover funding cell and UDT change");
}

#[test]
fn test_exclusion_map_migrate_funding_tx_releases_stale_inputs() {
    let mut map = LiveCellsExclusionMap::new();

    let local_in_a = out_point(1, 0);
    let local_in_b = out_point(2, 0);
    let tx0 = funding_tx_with_inputs(&[local_in_a.clone(), local_in_b.clone()]);
    map.add_funding_tx(&tx0);
    let tx0_hash = funding_tx_hash(&tx0);
    assert!(has_exclusion(&map, &tx0_hash));

    let peer_in = out_point(3, 0);
    let tx1 = funding_tx_with_inputs(&[local_in_a.clone(), local_in_b.clone(), peer_in.clone()]);
    let tx1_hash = funding_tx_hash(&tx1);
    assert_ne!(tx0_hash, tx1_hash);

    map.migrate_funding_tx(&tx0_hash, &tx1);

    assert!(!has_exclusion(&map, &tx0_hash));
    assert!(has_exclusion(&map, &tx1_hash));
    let excluded = excluded_out_points(&map);
    assert!(excluded.contains(&local_in_a));
    assert!(excluded.contains(&local_in_b));
    assert!(excluded.contains(&peer_in));

    map.remove(&tx1_hash);
    map.truncate(u64::MAX);
    assert!(exclusion_map_is_empty(&map));
    let excluded = excluded_out_points(&map);
    assert!(!excluded.contains(&local_in_a));
    assert!(!excluded.contains(&local_in_b));
}

#[test]
fn test_exclusion_map_migrate_funding_tx_is_noop_when_absent() {
    let mut map = LiveCellsExclusionMap::new();

    let tx1 = funding_tx_with_inputs(&[out_point(7, 0)]);
    let absent_hash = funding_tx_hash(&funding_tx_with_inputs(&[out_point(9, 0)]));

    map.migrate_funding_tx(&absent_hash, &tx1);

    assert!(exclusion_map_is_empty(&map));
}

#[test]
fn test_external_funding_build_ckb_funding_cell() {
    let context = dummy_funding_context();
    let request = FundingRequest {
        script: Script::default(),
        udt_type_script: None,
        local_amount: 100_000_000_000,
        remote_amount: 50_000_000_000,
        funding_fee_rate: 1000,
        local_reserved_ckb_amount: 6_200_000_000,
        remote_reserved_ckb_amount: 6_200_000_000,
    };

    let builder = dummy_funding_builder(FundingTx::new(), request.clone(), context.clone());
    let (output, data) = builder.build_funding_cell().expect("build funding cell");

    let expected_capacity: u64 = request.local_amount as u64 + request.local_reserved_ckb_amount;
    let actual_capacity: u64 = output.capacity().unpack();
    assert_eq!(actual_capacity, expected_capacity);
    assert_eq!(output.lock(), context.funding_cell_lock_script);
    assert!(output.type_().is_none());
    assert_eq!(data, packed::Bytes::default());
}

#[test]
fn test_external_funding_build_udt_funding_cell() {
    let context = dummy_funding_context();
    let udt_type_script = Script::new_builder()
        .code_hash(packed::Byte32::from_slice(&[1u8; 32]).unwrap())
        .hash_type(packed::Byte::new(0))
        .build();

    let request = FundingRequest {
        script: Script::default(),
        udt_type_script: Some(udt_type_script.clone()),
        local_amount: 1_000_000,
        remote_amount: 500_000,
        funding_fee_rate: 1000,
        local_reserved_ckb_amount: 14_200_000_000,
        remote_reserved_ckb_amount: 14_200_000_000,
    };

    let builder = dummy_funding_builder(FundingTx::new(), request.clone(), context.clone());
    let (output, data) = builder.build_funding_cell().expect("build funding cell");
    let expected_capacity = request.local_reserved_ckb_amount;
    let actual_capacity: u64 = output.capacity().unpack();
    assert_eq!(actual_capacity, expected_capacity);
    assert_eq!(output.lock(), context.funding_cell_lock_script);
    assert_eq!(
        output.type_().to_opt().expect("has type script"),
        udt_type_script
    );
    let expected_udt_amount: u128 = request.local_amount;
    let data_bytes = data.raw_data();
    assert_eq!(data_bytes.len(), 16);
    let mut amount_bytes = [0u8; 16];
    amount_bytes.copy_from_slice(&data_bytes[..16]);
    assert_eq!(u128::from_le_bytes(amount_bytes), expected_udt_amount);
}

#[test]
fn test_external_funding_build_ckb_funding_cell_after_remote_added() {
    let context = dummy_funding_context();
    let request = FundingRequest {
        script: Script::default(),
        udt_type_script: None,
        local_amount: 100_000_000_000,
        remote_amount: 50_000_000_000,
        funding_fee_rate: 1000,
        local_reserved_ckb_amount: 6_200_000_000,
        remote_reserved_ckb_amount: 6_200_000_000,
    };
    let existing_funding_output = packed::CellOutput::new_builder()
        .capacity(
            Capacity::shannons(request.local_amount as u64 + request.local_reserved_ckb_amount)
                .pack(),
        )
        .lock(context.funding_cell_lock_script.clone())
        .build();
    let existing_tx = packed::Transaction::default()
        .as_advanced_builder()
        .set_outputs(vec![existing_funding_output])
        .set_outputs_data(vec![packed::Bytes::default()])
        .build();
    let funding_tx: FundingTx = existing_tx.into();
    let builder = dummy_funding_builder(funding_tx, request.clone(), context.clone());
    let (output, data) = builder.build_funding_cell().expect("build funding cell");
    let expected_capacity: u64 = request.local_amount as u64
        + request.remote_amount as u64
        + request.local_reserved_ckb_amount
        + request.remote_reserved_ckb_amount;
    let actual_capacity: u64 = output.capacity().unpack();
    assert_eq!(actual_capacity, expected_capacity);
    assert_eq!(output.lock(), context.funding_cell_lock_script);
    assert_eq!(data, packed::Bytes::default());
}

#[test]
fn test_external_funding_build_udt_funding_cell_after_remote_added() {
    let context = dummy_funding_context();
    let udt_type_script = Script::new_builder()
        .code_hash(packed::Byte32::from_slice(&[1u8; 32]).unwrap())
        .hash_type(packed::Byte::new(0))
        .build();
    let request = FundingRequest {
        script: Script::default(),
        udt_type_script: Some(udt_type_script.clone()),
        local_amount: 1_000_000,
        remote_amount: 500_000,
        funding_fee_rate: 1000,
        local_reserved_ckb_amount: 14_200_000_000,
        remote_reserved_ckb_amount: 14_200_000_000,
    };
    let existing_funding_output = packed::CellOutput::new_builder()
        .capacity(Capacity::shannons(request.local_reserved_ckb_amount).pack())
        .type_(Some(udt_type_script.clone()).pack())
        .lock(context.funding_cell_lock_script.clone())
        .build();
    let existing_tx = packed::Transaction::default()
        .as_advanced_builder()
        .set_outputs(vec![existing_funding_output])
        .set_outputs_data(vec![request.local_amount.to_le_bytes().to_vec().pack()])
        .build();
    let funding_tx: FundingTx = existing_tx.into();
    let builder = dummy_funding_builder(funding_tx, request.clone(), context.clone());
    let (output, data) = builder.build_funding_cell().expect("build funding cell");
    let expected_capacity = request.local_reserved_ckb_amount + request.remote_reserved_ckb_amount;
    let actual_capacity: u64 = output.capacity().unpack();
    assert_eq!(actual_capacity, expected_capacity);
    assert_eq!(
        output.type_().to_opt().expect("has type script"),
        udt_type_script
    );

    let data_bytes = data.raw_data();
    assert_eq!(data_bytes.len(), 16);
    let mut amount_bytes = [0u8; 16];
    amount_bytes.copy_from_slice(&data_bytes[..16]);
    assert_eq!(
        u128::from_le_bytes(amount_bytes),
        request.local_amount + request.remote_amount
    );
}

#[test]
fn test_external_funding_build_funding_cell_overflow() {
    let context = dummy_funding_context();
    let request = FundingRequest {
        script: Script::default(),
        udt_type_script: None,
        local_amount: u64::MAX as u128,
        remote_amount: u64::MAX as u128,
        funding_fee_rate: 1000,
        local_reserved_ckb_amount: u64::MAX,
        remote_reserved_ckb_amount: u64::MAX,
    };

    let builder = dummy_funding_builder(FundingTx::new(), request, context);
    let result = builder.build_funding_cell();
    assert!(result.is_err(), "should overflow");
    match result {
        Err(FundingError::OverflowError) => {}
        other => panic!("expected OverflowError, got {:?}", other),
    }
}

#[test]
fn test_external_funding_build_udt_funding_cell_overflow() {
    let context = dummy_funding_context();
    let udt_type_script = Script::new_builder()
        .code_hash(packed::Byte32::from_slice(&[1u8; 32]).unwrap())
        .hash_type(packed::Byte::new(0))
        .build();

    let request = FundingRequest {
        script: Script::default(),
        udt_type_script: Some(udt_type_script.clone()),
        local_amount: u128::MAX,
        remote_amount: 1,
        funding_fee_rate: 1000,
        local_reserved_ckb_amount: 14_200_000_000,
        remote_reserved_ckb_amount: 14_200_000_000,
    };
    let existing_funding_output = packed::CellOutput::new_builder()
        .capacity(Capacity::shannons(request.local_reserved_ckb_amount).pack())
        .type_(Some(udt_type_script.clone()).pack())
        .lock(context.funding_cell_lock_script.clone())
        .build();
    let existing_tx = packed::Transaction::default()
        .as_advanced_builder()
        .set_outputs(vec![existing_funding_output])
        .set_outputs_data(vec![request.local_amount.to_le_bytes().to_vec().pack()])
        .build();

    let funding_tx: FundingTx = existing_tx.into();
    let builder = dummy_funding_builder(funding_tx, request, context);
    let result = builder.build_funding_cell();
    assert!(result.is_err(), "should overflow");
    match result {
        Err(FundingError::OverflowError) => {}
        other => panic!("expected OverflowError, got {:?}", other),
    }
}

#[test]
fn test_external_funding_build_ckb_funding_cell_amount_cast_overflow() {
    let context = dummy_funding_context();
    let request = FundingRequest {
        script: Script::default(),
        udt_type_script: None,
        local_amount: (u64::MAX as u128) + 1,
        remote_amount: 0,
        funding_fee_rate: 1000,
        local_reserved_ckb_amount: 0,
        remote_reserved_ckb_amount: 0,
    };

    let builder = dummy_funding_builder(FundingTx::new(), request, context);
    let result = builder.build_funding_cell();
    match result {
        Err(FundingError::OverflowError) => {}
        other => panic!("expected OverflowError, got {:?}", other),
    }
}

#[test]
fn test_secp_sighash_placeholder_witness_matches_sdk_layout() {
    let placeholder = secp_sighash_placeholder_witness();
    let lock = placeholder.lock().to_opt().expect("has lock placeholder");

    assert_eq!(lock.len(), SECP_SIGHASH_PLACEHOLDER_SIGNATURE_BYTES);
    assert!(is_secp_sighash_placeholder_witness(placeholder.as_slice()));
}
