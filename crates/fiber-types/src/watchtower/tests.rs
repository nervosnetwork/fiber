use super::*;
use ckb_types::prelude::*;

fn keys() -> (Pubkey, Pubkey) {
    (
        Privkey::from(&[1; 32]).pubkey(),
        Privkey::from(&[2; 32]).pubkey(),
    )
}

fn tlc(payment_hash: [u8; 32]) -> SettlementTlc {
    SettlementTlc {
        tlc_id: TLCId::Received(0),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 5,
        payment_hash: Hash256::from(payment_hash),
        expiry: 1_000,
        local_key: None,
        local_key_pubkey: Some(Privkey::from(&[3; 32]).pubkey()),
        local_key_commitment_number: None,
        remote_key: Privkey::from(&[4; 32]).pubkey(),
    }
}

fn settlement(local_amount: u128, payment_hash: [u8; 32]) -> SettlementData {
    SettlementData {
        local_amount,
        remote_amount: 1,
        tlcs: vec![tlc(payment_hash)],
    }
}

fn commitment_tx(data: &SettlementData, for_remote: bool) -> Transaction {
    let (local, remote) = keys();
    let hash = settlement_witness_hash(data, for_remote, local, remote);
    let mut args = vec![0u8; 36];
    args.extend_from_slice(&hash);
    args.push(0x00);
    args.push(ChannelFeatures::V2.bits());
    ckb_types::core::TransactionBuilder::default()
        .output(
            CellOutput::new_builder()
                .lock(Script::new_builder().args(args.pack()).build())
                .capacity(1000u64)
                .build(),
        )
        .output_data(Bytes::default())
        .build()
        .data()
}

#[test]
fn matching_settlement_binds_to_commitment_lock_args() {
    let (local, remote) = keys();
    let data = settlement(15, [9; 32]);
    let tx = commitment_tx(&data, true);
    assert!(settlement_matches_commitment_tx(
        &tx,
        &data,
        local,
        remote,
        Some(true)
    ));
    assert!(settlement_matches_commitment_tx(
        &tx, &data, local, remote, None
    ));
    assert!(!settlement_matches_commitment_tx(
        &tx,
        &data,
        local,
        remote,
        Some(false)
    ));
}

#[test]
fn only_v2_lock_layout_is_accepted() {
    let (local, remote) = keys();
    let data = settlement(15, [9; 32]);
    let tx = commitment_tx(&data, true);
    let output = tx.raw().outputs().get(0).unwrap();
    let args = output.lock().args().raw_data();
    assert_eq!(args.len(), ChannelFeatures::LOCK_ARGS_LEN);
    assert!(!settlement_matches_commitment_lock_args(
        &args[..57],
        &data,
        local,
        remote,
        None
    ));
    let mut unsupported = args.to_vec();
    unsupported[57] = 0;
    assert!(!settlement_matches_commitment_lock_args(
        &unsupported,
        &data,
        local,
        remote,
        None
    ));
    unsupported[57] = 2;
    assert!(!settlement_matches_commitment_lock_args(
        &unsupported,
        &data,
        local,
        remote,
        None
    ));
}

#[test]
fn full_payment_hash_is_committed() {
    let (local, remote) = keys();
    let committed = settlement(15, [9; 32]);
    let mut changed = [9; 32];
    changed[31] ^= 1;
    let tx = commitment_tx(&committed, true);
    assert!(!settlement_matches_commitment_tx(
        &tx,
        &settlement(15, changed),
        local,
        remote,
        None
    ));
}

#[test]
fn local_amount_mismatch_is_rejected() {
    let (local, remote) = keys();
    let committed = settlement(15, [9; 32]);
    let lied = settlement(99, [9; 32]);
    let tx = commitment_tx(&committed, true);
    assert!(!settlement_matches_commitment_tx(
        &tx, &lied, local, remote, None
    ));
}

#[test]
fn tlc_set_mismatch_is_rejected() {
    let (local, remote) = keys();
    let committed = settlement(15, [9; 32]);
    let lied = settlement(15, [8; 32]);
    let tx = commitment_tx(&committed, true);
    assert!(!settlement_matches_commitment_tx(
        &tx, &lied, local, remote, None
    ));
}

#[test]
fn empty_or_short_lock_args_do_not_match() {
    let (local, remote) = keys();
    let data = settlement(15, [9; 32]);
    let tx = ckb_types::core::TransactionBuilder::default()
        .output(
            CellOutput::new_builder()
                .lock(Script::new_builder().args([0u8; 20].pack()).build())
                .capacity(1000u64)
                .build(),
        )
        .output_data(Bytes::default())
        .build()
        .data();
    assert!(!settlement_matches_commitment_tx(
        &tx, &data, local, remote, None
    ));
    assert!(!settlement_matches_commitment_tx(
        &Transaction::default(),
        &data,
        local,
        remote,
        None
    ));
}
