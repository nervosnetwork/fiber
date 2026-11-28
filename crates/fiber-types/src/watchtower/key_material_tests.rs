use super::*;

fn key(byte: u8) -> Privkey {
    Privkey::from(&[byte; 32])
}

fn settlement_data() -> SettlementData {
    SettlementData {
        local_amount: 0,
        remote_amount: 0,
        tlcs: Vec::new(),
    }
}

fn channel_data(local_key: Option<Privkey>, local_pubkey: Option<Pubkey>) -> ChannelData {
    ChannelData {
        channel_id: Hash256::from([7; 32]),
        funding_udt_type_script: None,
        local_settlement_key: local_key,
        local_settlement_key_pubkey: local_pubkey,
        remote_settlement_key: key(2).pubkey(),
        local_funding_pubkey: key(3).pubkey(),
        remote_funding_pubkey: key(4).pubkey(),
        remote_settlement_data: settlement_data(),
        pending_remote_settlement_data: settlement_data(),
        local_settlement_data: settlement_data(),
        revocation_data: None,
        channel_features: ChannelFeatures::V2,
    }
}

#[test]
fn from_parts_rejects_a_contradicting_pair() {
    let error = SettlementKey::from_parts(Some(key(1)), Some(key(9).pubkey()))
        .expect_err("a public key that does not match the secret must be rejected");
    assert!(error.contains("does not match"), "{error}");
}

#[test]
fn from_parts_round_trips_through_the_persisted_pair() {
    let local = SettlementKey::from_parts(Some(key(1)), Some(key(1).pubkey()))
        .unwrap()
        .expect("local key material");
    assert_eq!(local.secret(), Some(&key(1)));
    assert!(!local.is_external());
    assert_eq!(
        local.to_parts(),
        (Some(key(1)), Some(key(1).pubkey())),
        "writers persist a pair the readers can trust"
    );

    let external = SettlementKey::from_parts(None, Some(key(2).pubkey()))
        .unwrap()
        .expect("public-only key material");
    assert_eq!(external.secret(), None);
    assert!(external.is_external());
    assert_eq!(external.to_parts(), (None, Some(key(2).pubkey())));

    assert_eq!(SettlementKey::from_parts(None, None), Ok(None));
}

#[test]
fn stored_rows_trust_the_secret_over_a_contradicting_public_key() {
    let data = channel_data(Some(key(1)), Some(key(9).pubkey()));
    assert_eq!(data.settlement_key(), Some(SettlementKey::Local(key(1))));
    assert_eq!(
        data.local_settlement_pubkey(),
        key(1).pubkey(),
        "signatures come from the secret, so a contradicting stored key is ignored"
    );
    assert_eq!(
        data.expected_onchain_pubkey(&crate::channel_signer::OnchainKeyPurpose::Settlement),
        Ok(key(1).pubkey())
    );

    let external = channel_data(None, Some(key(2).pubkey()));
    assert_eq!(
        external.settlement_key(),
        Some(SettlementKey::External(key(2).pubkey()))
    );
    assert_eq!(
        channel_data(None, None).settlement_key(),
        None,
        "a row with no key material has no settlement key"
    );
}

#[test]
fn external_settlement_rejects_secrets_and_missing_derivation_metadata() {
    let mut data = settlement_data();
    data.tlcs.push(SettlementTlc {
        tlc_id: TLCId::Offered(1),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 1,
        payment_hash: Hash256::from([8; 32]),
        expiry: 1000,
        local_key: None,
        local_key_pubkey: Some(key(1).pubkey()),
        local_key_commitment_number: Some(7),
        remote_key: key(2).pubkey(),
    });
    assert!(data.validate_signing_keys(true).is_ok());
    data.tlcs[0].local_key = Some(key(1));
    assert!(data.validate_signing_keys(true).is_err());
    data.tlcs[0].local_key = None;
    data.tlcs[0].local_key_commitment_number = None;
    assert!(data.validate_signing_keys(true).is_err());
}

#[test]
fn settlement_snapshot_rejects_unencodable_tlc_count() {
    let mut data = settlement_data();
    let entry = SettlementTlc {
        tlc_id: TLCId::Offered(1),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 1,
        payment_hash: Hash256::from([8; 32]),
        expiry: 1000,
        local_key: None,
        local_key_pubkey: Some(key(1).pubkey()),
        local_key_commitment_number: Some(7),
        remote_key: key(2).pubkey(),
    };
    data.tlcs = vec![entry; MAX_WITNESS_TLC_COUNT];
    let (local, remote) = (key(1).pubkey(), key(2).pubkey());
    assert!(data.validate_signing_keys(true).is_ok());
    assert!(settlement_data_witness(&data, false, local, remote).is_ok());
    data.tlcs.push(data.tlcs[0].clone());
    assert!(data
        .validate_signing_keys(true)
        .unwrap_err()
        .contains("max 254"));
    assert!(settlement_data_witness(&data, false, local, remote).is_err());
    // A malformed persisted snapshot must be a mismatch, not a panic.
    let mut args = [0; 58];
    args[57] = ChannelFeatures::V2.bits();
    assert!(!settlement_matches_commitment_lock_args(
        &args, &data, local, remote, None,
    ));
}

#[test]
fn superseded_tlc_keys_remain_verifiable_without_settlement_key_fallback() {
    let data = channel_data(None, Some(key(1).pubkey()));
    let purpose = crate::channel_signer::OnchainKeyPurpose::Tlc {
        commitment_number: 7,
    };
    assert!(data.expected_onchain_pubkey(&purpose).is_err());
    let mut superseded = settlement_data();
    superseded.tlcs.push(SettlementTlc {
        tlc_id: TLCId::Offered(1),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 1,
        payment_hash: Hash256::from([8; 32]),
        expiry: 1000,
        local_key: None,
        local_key_pubkey: Some(key(5).pubkey()),
        local_key_commitment_number: Some(7),
        remote_key: key(2).pubkey(),
    });
    assert_eq!(
        data.expected_onchain_pubkey_with_settlements(&purpose, &[superseded.clone()]),
        Ok(key(5).pubkey())
    );
    let mut conflicting = superseded.clone();
    conflicting.tlcs[0].local_key_pubkey = Some(key(6).pubkey());
    assert!(data
        .expected_onchain_pubkey_with_settlements(&purpose, &[superseded, conflicting])
        .is_err());
}

#[test]
fn inverted_keys_swap_sides_and_drop_the_secret() {
    let keys = WatchChannelKeys {
        local_settlement: SettlementKey::Local(key(1)),
        remote_settlement: key(2).pubkey(),
        local_funding: key(3).pubkey(),
        remote_funding: key(4).pubkey(),
    };
    let mirrored = keys.inverted();
    assert_eq!(
        mirrored.local_settlement,
        SettlementKey::External(key(2).pubkey()),
        "the mirrored row can only verify the original owner's signatures"
    );
    assert_eq!(mirrored.remote_settlement, key(1).pubkey());
    assert_eq!(mirrored.local_funding, key(4).pubkey());
    assert_eq!(mirrored.remote_funding, key(3).pubkey());
}

fn build_template_witness(htlc_count: u8, with_preimage: bool, sig: [u8; 65]) -> Vec<u8> {
    let mut w = vec![
        16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0,    // xudt placeholder
        0x01, // unlock_count = 1
        htlc_count,
    ];
    w.extend(std::iter::repeat_n(0x11u8, htlc_count as usize * 97));
    w.extend(std::iter::repeat_n(0x22u8, 72)); // party balances
    w.push(if htlc_count > 0 { 0x00 } else { 0xfe }); // unlock_type
    w.push(u8::from(with_preimage));
    w.extend_from_slice(&sig);
    if with_preimage {
        w.extend_from_slice(&[0xaa; 32]);
    }
    w
}

fn test_wallet_lock() -> Script {
    use ckb_types::packed::Byte32;
    use ckb_types::prelude::*;
    Script::new_builder()
        .code_hash(Byte32::from([0x33; 32]))
        .args([0x11u8; 20].pack())
        .build()
}

fn test_udt_type() -> Script {
    use ckb_types::packed::Byte32;
    use ckb_types::prelude::*;
    Script::new_builder()
        .code_hash(Byte32::from([0x44; 32]))
        .args([0x22u8; 32])
        .build()
}

fn build_template_and_funded(
    htlc_count: u8,
    with_preimage: bool,
    funding_since: u64,
) -> (Transaction, Transaction) {
    use ckb_types::core::{DepType, TransactionBuilder};
    use ckb_types::packed::{Byte32, CellDep, CellInput, OutPoint};
    use ckb_types::prelude::*;

    let commitment_outpoint = OutPoint::new(Byte32::from([1u8; 32]), 0);
    let wallet_outpoint = OutPoint::new(Byte32::from([2u8; 32]), 1);
    let wallet_lock = test_wallet_lock();
    let udt_type = test_udt_type();
    let residual_lock = Script::new_builder()
        .code_hash(Byte32::from([0x55; 32]))
        .args([0xaau8; 56].pack())
        .build();
    let residual_out = CellOutput::new_builder()
        .lock(residual_lock)
        .capacity(28_400_000_000u64)
        .type_(Some(udt_type.clone()).pack())
        .build();
    let residual_data = 100u128.to_le_bytes().to_vec().pack();
    let template_udt_out = CellOutput::new_builder()
        .capacity(14_200_000_000u64)
        .type_(Some(udt_type.clone()).pack())
        .build();
    let funded_udt_out = CellOutput::new_builder()
        .lock(wallet_lock.clone())
        .capacity(14_200_000_000u64)
        .type_(Some(udt_type).pack())
        .build();
    let udt_data = 50u128.to_le_bytes().to_vec().pack();
    let change_out = CellOutput::new_builder()
        .lock(wallet_lock)
        .capacity(6_100_000_000u64)
        .build();
    let w0 = build_template_witness(htlc_count, with_preimage, [0u8; 65]);

    let template = TransactionBuilder::default()
        .cell_dep(
            CellDep::new_builder()
                .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                .dep_type(DepType::DepGroup)
                .build(),
        )
        .input(CellInput::new(commitment_outpoint.clone(), 0))
        .output(residual_out.clone())
        .output_data(residual_data.clone())
        .output(template_udt_out)
        .output_data(udt_data.clone())
        .witness(w0.clone().pack())
        .build()
        .data();

    let funded = TransactionBuilder::default()
        .cell_dep(
            CellDep::new_builder()
                .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                .dep_type(DepType::DepGroup)
                .build(),
        )
        .cell_dep(
            CellDep::new_builder()
                .out_point(OutPoint::new(Byte32::from([0x20; 32]), 0))
                .dep_type(DepType::Code)
                .build(),
        )
        .input(CellInput::new(commitment_outpoint, 0))
        .input(CellInput::new(wallet_outpoint, funding_since))
        .output(residual_out)
        .output_data(residual_data)
        .output(funded_udt_out)
        .output_data(udt_data)
        .output(change_out)
        .output_data(Bytes::default())
        .witness(w0.pack())
        .witness([0xdeu8, 0xad, 0xbe, 0xef].pack())
        .witness([0xcau8, 0xfe].pack())
        .build()
        .data();

    (template, funded)
}

#[test]
fn external_funding_watchtower_tx_validation_and_normalization() {
    use ckb_types::core::DepType;
    use ckb_types::packed::{Byte32, CellDep, CellInput, OutPoint};
    use ckb_types::prelude::*;

    let expiry_since = settlement_timestamp_since(100_000);
    let (template, funded) = build_template_and_funded(1, false, expiry_since);
    let sig = [0x55u8; 65];

    // 1. Valid timeout TLC with zero placeholder in witness[0] normalizes cleanly and preserves witness[1..].
    let norm_zero = validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &funded,
        Some(expiry_since),
        Some(&sig),
    )
    .expect("valid zero-placeholder funded tx");
    assert_eq!(norm_zero.witnesses().len(), 3);
    assert_eq!(
        norm_zero.witnesses().get(1).unwrap().raw_data().as_ref(),
        &[0xde, 0xad, 0xbe, 0xef]
    );
    assert_eq!(
        norm_zero.witnesses().get(2).unwrap().raw_data().as_ref(),
        &[0xca, 0xfe]
    );

    // 2. Filled witness[0] matching `submitted_signature` normalizes to identical tx & digest.
    let filled_w0 = build_template_witness(1, false, sig);
    let filled = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![
            filled_w0.pack(),
            [0xdeu8, 0xad, 0xbe, 0xef].pack(),
            [0xcau8, 0xfe].pack(),
        ])
        .build()
        .data();
    let norm_filled = validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &filled,
        Some(expiry_since),
        Some(&sig),
    )
    .expect("valid filled-signature funded tx");
    assert_eq!(norm_filled.as_slice(), norm_zero.as_slice());
    assert_eq!(
        external_funding_watchtower_tx_hash(&filled),
        external_funding_watchtower_tx_hash(&funded)
    );

    // Pre-signing mode (`submitted_signature: None`) rejects non-zero signature slot in witness[0].
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &filled,
        Some(expiry_since),
        None,
    )
    .is_err());

    // Wrong signature in witness[0] rejected.
    let other_sig = [0x66u8; 65];
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &filled,
        Some(expiry_since),
        Some(&other_sig),
    )
    .is_err());

    // 3. Changing client funding witness changes external_funding_watchtower_tx_hash.
    let diff_witness = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![
            funded.witnesses().get(0).unwrap(),
            [0xdeu8, 0xad, 0xbe, 0xee].pack(),
            [0xcau8, 0xfe].pack(),
        ])
        .build()
        .data();
    assert_ne!(
        external_funding_watchtower_tx_hash(&funded),
        external_funding_watchtower_tx_hash(&diff_witness)
    );

    // 4. Missing or too-early absolute timestamp `since` (or 1-input tx) rejected for timeout TLC.
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &template,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());
    let early = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_inputs(vec![
            funded.raw().inputs().get(0).unwrap(),
            CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), expiry_since - 1),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &early,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());
    let no_since = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_inputs(vec![
            funded.raw().inputs().get(0).unwrap(),
            CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), 0),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &no_since,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    // 4b. First-absolute-timestamp-input rule: [expiry - 1, expiry] fails while
    //     [expiry, expiry - 1] succeeds (matching CommitmentLock::load_expiry_raw_since_value).
    let second_wallet_outpoint = OutPoint::new(Byte32::from([3u8; 32]), 0);
    let early_then_valid = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_inputs(vec![
            funded.raw().inputs().get(0).unwrap(),
            CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), expiry_since - 1),
            CellInput::new(second_wallet_outpoint.clone(), expiry_since),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &early_then_valid,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());
    let valid_then_early = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_inputs(vec![
            funded.raw().inputs().get(0).unwrap(),
            CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), expiry_since),
            CellInput::new(second_wallet_outpoint, expiry_since - 1),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &valid_then_early,
        Some(expiry_since),
        Some(&sig),
    )
    .is_ok());

    // 5. Non-timeout preimage TLC allows 1-input fee-from-payout AND 2-input funding,
    //    and rejects unexpected absolute timestamp `since`.
    let (preimage_tmpl, preimage_funded_ok) = build_template_and_funded(1, true, 0);
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &preimage_tmpl,
        &preimage_funded_ok,
        None,
        Some(&sig),
    )
    .is_ok());
    // 1-input fee deducted from payout output[1] (residual output[0] preserved, placeholder lock replaced).
    let wallet_lock = test_wallet_lock();
    let udt_type = test_udt_type();
    let fee_from_payout = preimage_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![
            preimage_tmpl.raw().outputs().get(0).unwrap(),
            CellOutput::new_builder()
                .lock(wallet_lock.clone())
                .type_(Some(udt_type.clone()).pack())
                .capacity(14_299_999_000u64)
                .build(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &preimage_tmpl,
        &fee_from_payout,
        None,
        Some(&sig),
    )
    .is_ok());
    // 1-output `two_parties_all_settled` template allows deducting fee directly from outputs[0]
    // once placeholder lock is replaced, and rejects leaving Script::default() in place.
    let final_tmpl = ckb_types::core::TransactionBuilder::default()
        .cell_dep(
            CellDep::new_builder()
                .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                .dep_type(DepType::DepGroup)
                .build(),
        )
        .input(preimage_tmpl.raw().inputs().get(0).unwrap())
        .output(
            CellOutput::new_builder()
                .capacity(20_000_000_000u64)
                .build(),
        )
        .output_data(Bytes::default())
        .witness(build_template_witness(0, false, [0u8; 65]).pack())
        .build()
        .data();
    let final_placeholder_lock = final_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![CellOutput::new_builder()
            .capacity(19_999_999_000u64)
            .build()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &final_tmpl,
        &final_placeholder_lock,
        None,
        Some(&sig),
    )
    .is_err());
    let final_fee_from_payout = final_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![CellOutput::new_builder()
            .lock(wallet_lock.clone())
            .capacity(19_999_999_000u64)
            .build()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &final_tmpl,
        &final_fee_from_payout,
        None,
        Some(&sig),
    )
    .is_ok());
    // Balance withdrawal with no pending TLCs (htlc_count == 0, unlock_type == 0xFE) allows
    // a mature absolute-timestamp wallet funding input when required_expiry_since is None.
    let (balance_tmpl, balance_funded_with_ts) = build_template_and_funded(0, false, expiry_since);
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &balance_tmpl,
        &balance_funded_with_ts,
        None,
        Some(&sig),
    )
    .is_ok());
    // Balance withdrawal with an expired TLC still present in witness (htlc_count == 1,
    // unlock_type == 0xFE) must ALSO allow a mature absolute-timestamp wallet funding input.
    let mut balance_with_expired_tlc_w = vec![
        16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0,    // xudt placeholder
        0x01, // unlock_count = 1
        0x01, // htlc_count = 1 (expired TLC retained in witness)
    ];
    balance_with_expired_tlc_w.extend(std::iter::repeat_n(0x11u8, 97));
    balance_with_expired_tlc_w.extend(std::iter::repeat_n(0x22u8, 72));
    balance_with_expired_tlc_w.push(0xfe); // balance unlock_type
    balance_with_expired_tlc_w.push(0x00); // with_preimage = false
    balance_with_expired_tlc_w.extend_from_slice(&[0u8; 65]);
    let balance_with_expired_tmpl = balance_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![balance_with_expired_tlc_w.clone().pack()])
        .build()
        .data();
    let balance_with_expired_funded = balance_funded_with_ts
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![
            balance_with_expired_tlc_w.pack(),
            balance_funded_with_ts.witnesses().get(1).unwrap(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &balance_with_expired_tmpl,
        &balance_with_expired_funded,
        None,
        Some(&sig),
    )
    .is_ok());
    let (_, preimage_funded_bad_since) = build_template_and_funded(1, true, expiry_since);
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &preimage_tmpl,
        &preimage_funded_bad_since,
        None,
        Some(&sig),
    )
    .is_err());

    // 5a. Placeholder lock, missing template cell_deps, occupied capacity 1-shannon boundary,
    //     and UDT payout preservation (burn vs split).
    // Placeholder lock left on outputs[1] of a 2-output tx is rejected.
    let placeholder_out1 = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![
            funded.raw().outputs().get(0).unwrap(),
            CellOutput::new_builder()
                .type_(Some(udt_type.clone()).pack())
                .capacity(14_200_000_000u64)
                .build(),
            funded.raw().outputs().get(2).unwrap(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &placeholder_out1,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    // Missing template cell_deps rejected.
    let empty_deps = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_cell_deps(vec![])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &empty_deps,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    // Duplicate cell_deps rejected before consuming the request (CKB DuplicateCellDeps).
    let template_dep = template.raw().cell_deps().get(0).unwrap();
    let dup_deps = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .cell_dep(template_dep)
        .build()
        .data();
    let dup_err = validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &dup_deps,
        Some(expiry_since),
        Some(&sig),
    )
    .expect_err("duplicate cell_deps must be rejected");
    assert!(dup_err.contains("duplicate cell_deps"), "{dup_err}");

    // Occupied capacity one-shannon boundary on a standard 20-byte-args lock (61 CKB = 6_100_000_000).
    let exact_change = CellOutput::new_builder()
        .lock(wallet_lock.clone())
        .capacity(6_100_000_000u64)
        .build();
    let exact_occ = exact_change
        .occupied_capacity(ckb_types::core::Capacity::bytes(0).unwrap())
        .unwrap()
        .as_u64();
    assert_eq!(exact_occ, 6_100_000_000u64);
    let under_occupied_final = final_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![CellOutput::new_builder()
            .lock(wallet_lock.clone())
            .capacity(exact_occ - 1)
            .build()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &final_tmpl,
        &under_occupied_final,
        None,
        Some(&sig),
    )
    .is_err());
    let exact_occupied_final = final_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![CellOutput::new_builder()
            .lock(wallet_lock.clone())
            .capacity(exact_occ)
            .build()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &final_tmpl,
        &exact_occupied_final,
        None,
        Some(&sig),
    )
    .is_ok());

    // UDT payout preservation on final 1-output template: dropping type/data (burn) rejected,
    // underpaying rejected, splitting across two UDT outputs totaling template amount accepted.
    let final_udt_tmpl = ckb_types::core::TransactionBuilder::default()
        .cell_dep(
            CellDep::new_builder()
                .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                .dep_type(DepType::DepGroup)
                .build(),
        )
        .input(preimage_tmpl.raw().inputs().get(0).unwrap())
        .output(
            CellOutput::new_builder()
                .type_(Some(udt_type.clone()).pack())
                .capacity(14_200_000_000u64)
                .build(),
        )
        .output_data(100u128.to_le_bytes().to_vec().pack())
        .witness(build_template_witness(0, false, [0u8; 65]).pack())
        .build()
        .data();
    let burned_udt = final_udt_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![CellOutput::new_builder()
            .lock(wallet_lock.clone())
            .capacity(14_199_999_000u64)
            .build()])
        .set_outputs_data(vec![Bytes::default()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &final_udt_tmpl,
        &burned_udt,
        None,
        Some(&sig),
    )
    .is_err());
    let underpaid_udt = final_udt_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![CellOutput::new_builder()
            .lock(wallet_lock.clone())
            .type_(Some(udt_type.clone()).pack())
            .capacity(14_200_000_000u64)
            .build()])
        .set_outputs_data(vec![99u128.to_le_bytes().to_vec().pack()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &final_udt_tmpl,
        &underpaid_udt,
        None,
        Some(&sig),
    )
    .is_err());
    let split_udt = final_udt_tmpl
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![
            CellOutput::new_builder()
                .lock(wallet_lock.clone())
                .type_(Some(udt_type.clone()).pack())
                .capacity(14_200_000_000u64)
                .build(),
            CellOutput::new_builder()
                .lock(wallet_lock)
                .type_(Some(udt_type).pack())
                .capacity(14_200_000_000u64)
                .build(),
        ])
        .set_outputs_data(vec![
            40u128.to_le_bytes().to_vec().pack(),
            60u128.to_le_bytes().to_vec().pack(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &final_udt_tmpl,
        &split_udt,
        None,
        Some(&sig),
    )
    .is_ok());

    // 5b. Multi-input same-lock wallet omitting trailing empty witnesses (e.g. 3 inputs, 2 witnesses) is accepted.
    let multi_same_lock = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .input(CellInput::new(
            OutPoint::new(Byte32::from([0x23; 32]), 1),
            0,
        ))
        .set_witnesses(vec![
            funded.witnesses().get(0).unwrap(),
            funded.witnesses().get(1).unwrap(),
        ])
        .build()
        .data();
    assert_eq!(multi_same_lock.raw().inputs().len(), 3);
    assert_eq!(multi_same_lock.witnesses().len(), 2);
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &multi_same_lock,
        Some(expiry_since),
        Some(&sig),
    )
    .is_ok());

    // 6. Duplicate inputs (both duplicating inputs[0] and duplicates among inputs[1..]) rejected.
    let dup_input = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_inputs(vec![
            funded.raw().inputs().get(0).unwrap(),
            CellInput::new(
                funded.raw().inputs().get(0).unwrap().previous_output(),
                expiry_since,
            ),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &dup_input,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    let dup_funding_input = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .input(funded.raw().inputs().get(1).unwrap())
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &dup_funding_input,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    // 6b. Template with non-zero signature slot is rejected.
    let nonzero_template = template
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![build_template_witness(1, false, sig).pack()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &nonzero_template,
        &funded,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    // 6c. Retry witness[0] validation and hash binding reject missing or tampered witness[0].
    let funded_with_sig = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![
            build_template_witness(1, false, sig).pack(),
            funded.witnesses().get(1).unwrap(),
            funded.witnesses().get(2).unwrap(),
        ])
        .build()
        .data();
    assert!(validate_external_funding_retry_witness_zero(&funded, &sig).is_ok());
    assert!(validate_external_funding_retry_witness_zero(&funded_with_sig, &sig).is_ok());
    assert_eq!(
        external_funding_watchtower_tx_hash(&funded),
        external_funding_watchtower_tx_hash(&funded_with_sig)
    );
    let missing_w0 = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![])
        .build()
        .data();
    assert!(validate_external_funding_retry_witness_zero(&missing_w0, &sig).is_err());
    assert_ne!(
        external_funding_watchtower_tx_hash(&funded),
        external_funding_watchtower_tx_hash(&missing_w0)
    );
    let wrong_sig_w0 = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![
            build_template_witness(1, false, [0x99u8; 65]).pack(),
            funded.witnesses().get(1).unwrap(),
            funded.witnesses().get(2).unwrap(),
        ])
        .build()
        .data();
    assert!(validate_external_funding_retry_witness_zero(&wrong_sig_w0, &sig).is_err());

    // 7. Tampered inputs[0], outputs[0] (when template has 2 outputs), outputs_data[0],
    //    witness[0], non-zero version, or non-empty header_deps rejected.
    let bad_in0_outpoint = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_inputs(vec![
            CellInput::new(OutPoint::new(Byte32::from([0x99; 32]), 0), 0),
            funded.raw().inputs().get(1).unwrap(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &bad_in0_outpoint,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    let bad_in0_since = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_inputs(vec![
            CellInput::new(
                funded.raw().inputs().get(0).unwrap().previous_output(),
                expiry_since,
            ),
            funded.raw().inputs().get(1).unwrap(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &bad_in0_since,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    let orig_out0 = funded.raw().outputs().get(0).unwrap();
    let orig_lock = orig_out0.lock();
    let mut flipped_lock_args = orig_lock.args().raw_data().to_vec();
    assert!(!flipped_lock_args.is_empty());
    flipped_lock_args[0] ^= 0x01;
    let tampered_out0 = orig_out0
        .as_builder()
        .lock(
            orig_lock
                .as_builder()
                .args(flipped_lock_args.pack())
                .build(),
        )
        .build();
    let bad_out0 = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![
            tampered_out0,
            funded.raw().outputs().get(1).unwrap(),
            funded.raw().outputs().get(2).unwrap(),
        ])
        .build()
        .data();
    let bad_out0_err = validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &bad_out0,
        Some(expiry_since),
        Some(&sig),
    )
    .expect_err("tampered output[0] must be rejected");
    assert!(
        bad_out0_err.contains("output[0] must match template commitment output"),
        "unexpected error: {bad_out0_err}"
    );

    let bad_out_data0 = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs_data(vec![
            99u128.to_le_bytes().to_vec().pack(),
            funded.raw().outputs_data().get(1).unwrap(),
            funded.raw().outputs_data().get(2).unwrap(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &bad_out_data0,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    let bad_version = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .version(1u32)
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &bad_version,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    let bad_header_deps = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .header_dep(Byte32::from([0xab; 32]))
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &bad_header_deps,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    let mut bad_preimage_w0 = build_template_witness(1, true, [0u8; 65]);
    *bad_preimage_w0.last_mut().unwrap() ^= 1;
    let bad_w0_tx = preimage_funded_ok
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![bad_preimage_w0.pack(), [0x01u8].pack()])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &preimage_tmpl,
        &bad_w0_tx,
        None,
        Some(&sig),
    )
    .is_err());

    // 8. Oversized tx (bytes) and excessive item counts rejected.
    let oversized_witness = funded
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(vec![
            funded.witnesses().get(0).unwrap(),
            vec![0u8; MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_BYTES + 1].pack(),
        ])
        .build()
        .data();
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &oversized_witness,
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());

    let mut too_many_deps = funded.clone().into_view().as_advanced_builder();
    for i in 0..MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS {
        too_many_deps = too_many_deps.cell_dep(
            CellDep::new_builder()
                .out_point(OutPoint::new(Byte32::from([i as u8; 32]), i as u32))
                .dep_type(DepType::Code)
                .build(),
        );
    }
    assert!(validate_and_normalize_external_funding_watchtower_tx(
        &template,
        &too_many_deps.build().data(),
        Some(expiry_since),
        Some(&sig),
    )
    .is_err());
}

#[test]
fn first_pending_prefers_self_fundable_claim_over_lower_id_timeout() {
    use ckb_types::core::TransactionBuilder;
    use ckb_types::prelude::*;

    let make_req = |payout_capacity: u64, required_expiry_since: Option<u64>| {
        let tx = TransactionBuilder::default()
            .output(CellOutput::new_builder().capacity(payout_capacity).build())
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        PendingWatchtowerRequest {
            content: crate::channel_signer::OnchainSigningContent {
                key_purpose: crate::channel_signer::OnchainKeyPurpose::Settlement,
                transaction: tx,
            },
            created_at_ms: 1_000,
            required_expiry_since,
        }
    };

    let timeout_id = Hash256::from([0x01; 32]);
    let small_id = Hash256::from([0x02; 32]);
    let self_fundable_id = Hash256::from([0xff; 32]);
    let mut state = WatchtowerExternalSignerState::default();
    state.pending_requests.insert(
        timeout_id,
        make_req(10_000_000_000, Some(settlement_timestamp_since(100_000))),
    );
    state
        .pending_requests
        .insert(small_id, make_req(5_100_000_000, None));
    state
        .pending_requests
        .insert(self_fundable_id, make_req(20_000_000_000, None));

    // Even though `self_fundable_id` > `timeout_id`, the self-fundable CKB claim must be exposed.
    assert_eq!(
        state.first_pending().map(|(id, _)| id),
        Some(self_fundable_id)
    );
    state.pending_requests.remove(&self_fundable_id);
    // Without a self-fundable claim, non-timeout still precedes a lower-ID timeout claim.
    assert_eq!(state.first_pending().map(|(id, _)| id), Some(small_id));
}

#[test]
fn first_pending_requires_fee_buffer_for_self_fundable_threshold() {
    use ckb_types::core::TransactionBuilder;
    use ckb_types::prelude::*;

    let make_req = |payout_capacity: u64, type_script: Option<Script>| {
        let tx = TransactionBuilder::default()
            .output(
                CellOutput::new_builder()
                    .capacity(payout_capacity)
                    .type_(type_script.pack())
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        PendingWatchtowerRequest {
            content: crate::channel_signer::OnchainSigningContent {
                key_purpose: crate::channel_signer::OnchainKeyPurpose::Settlement,
                transaction: tx,
            },
            created_at_ms: 1_000,
            required_expiry_since: None,
        }
    };

    let barely_above_61_req = make_req(6_100_000_001, None);
    let at_threshold_req = make_req(EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY, None);
    let ckb_200_req = make_req(20_000_000_000, None);
    let udt_200_req = make_req(20_000_000_000, Some(test_udt_type()));

    assert!(!pending_request_priority(&barely_above_61_req).0);
    assert!(pending_request_priority(&at_threshold_req).0);
    assert!(pending_request_priority(&ckb_200_req).0);
    assert!(!pending_request_priority(&udt_200_req).0);

    let low_id = Hash256::from([0x01; 32]);
    let high_id = Hash256::from([0xff; 32]);

    // 61 CKB + 1 shannon with a lower request_id must rank below a 200 CKB claim.
    let mut ckb_state = WatchtowerExternalSignerState::default();
    ckb_state
        .pending_requests
        .insert(low_id, barely_above_61_req.clone());
    ckb_state.pending_requests.insert(high_id, ckb_200_req);
    assert_eq!(ckb_state.first_pending().map(|(id, _)| id), Some(high_id));

    // Against a 200 CKB UDT claim (not self-fundable), 61 CKB + 1 shannon also ranks below it
    // because it lacks the fee buffer, whereas EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY ranks above it.
    let mut udt_state = WatchtowerExternalSignerState::default();
    udt_state
        .pending_requests
        .insert(low_id, barely_above_61_req);
    udt_state.pending_requests.insert(high_id, udt_200_req);
    assert_eq!(udt_state.first_pending().map(|(id, _)| id), Some(high_id));

    let threshold_id = Hash256::from([0xfe; 32]);
    udt_state
        .pending_requests
        .insert(threshold_id, at_threshold_req);
    assert_eq!(
        udt_state.first_pending().map(|(id, _)| id),
        Some(threshold_id)
    );
}
