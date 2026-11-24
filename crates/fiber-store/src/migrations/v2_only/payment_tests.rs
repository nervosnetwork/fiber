use super::*;

#[test]
fn released_tlc_payloads_gain_no_hosted_scope() {
    let old = OldChannelActorData::samples(75).remove(1);
    let converted: fiber_types::TlcState =
        super::super::convert_named(old.tlc_state.clone()).unwrap();
    assert_eq!(
        converted.offered_tlcs.tlcs.len(),
        old.tlc_state.offered_tlcs.tlcs.len()
    );
    assert_eq!(
        converted.received_tlcs.tlcs.len(),
        old.tlc_state.received_tlcs.tlcs.len()
    );
    for tlc in converted.all_tlcs() {
        assert_eq!(tlc.upstream_tlc_key, None);
    }
    let operations: std::collections::VecDeque<fiber_types::RetryableTlcOperation> =
        super::super::convert_named(old.retryable_tlc_operations).unwrap();
    for operation in operations {
        if let fiber_types::RetryableTlcOperation::AddTlc(command) = operation {
            assert_eq!(command.upstream_tlc_key, None);
        }
    }
}

#[test]
fn released_payment_records_gain_no_hosted_execution_scope() {
    let (_dir, store) = database();
    let target = fiber_types_0100::Privkey::from(&[2; 32]).pubkey();
    let hash = fiber_types_0100::Hash256::from([3; 32]);
    let old = fiber_types_0100::PaymentSession {
        request: fiber_types_0100::SendPaymentData {
            target_pubkey: target,
            amount: u128::MAX - 1,
            payment_hash: hash,
            invoice: None,
            final_tlc_expiry_delta: 100,
            tlc_expiry_limit: 1000,
            timeout: None,
            max_fee_amount: Some(u128::MAX),
            max_parts: None,
            keysend: false,
            udt_type_script: None,
            preimage: None,
            custom_records: None,
            allow_self_payment: false,
            hop_hints: vec![],
            router: vec![],
            allow_mpp: false,
            dry_run: false,
            trampoline_hops: None,
            trampoline_context: None,
        },
        last_error: Some("released error".to_owned()),
        last_error_code: None,
        try_limit: 3,
        status: fiber_types_0100::PaymentStatus::Failed,
        created_at: 100,
        last_updated_at: 200,
        cached_attempts: vec![],
    };
    let attempt = old.new_attempt(1, target, target, vec![]);
    let session_key = [
        &[fiber_types::schema::PAYMENT_SESSION_PREFIX],
        hash.as_ref(),
    ]
    .concat();
    let attempt_key = [
        &[fiber_types::schema::ATTEMPT_PREFIX],
        hash.as_ref(),
        &1u64.to_le_bytes(),
    ]
    .concat();
    store.put(&session_key, bincode::serialize(&old).unwrap());
    store.put(&attempt_key, bincode::serialize(&attempt).unwrap());
    migrate(&store).unwrap();
    let session: fiber_types::PaymentSession =
        decode_exact(&store.get(&session_key).unwrap()).unwrap();
    let restored: fiber_types::Attempt = decode_exact(&store.get(&attempt_key).unwrap()).unwrap();
    assert_eq!(session.request.upstream_tlc_key, None);
    assert_eq!(restored.upstream_tlc_key, None);
    assert_eq!(session.request.amount, old.request.amount);
    assert_eq!(session.request.max_fee_amount, old.request.max_fee_amount);
    assert_eq!(session.last_error, old.last_error);
    assert_eq!(session.status, fiber_types::PaymentStatus::Failed);
    assert_eq!(
        restored.payment_hash.as_ref(),
        attempt.payment_hash.as_ref()
    );
    assert_eq!(restored.id, attempt.id);
    let before = snapshot(&store);
    migrate(&store).unwrap();
    assert_eq!(snapshot(&store), before);
}

#[test]
fn interrupted_migration_before_version_advance_resumes_cleanly() {
    let (_dir, store) = database();
    let target = fiber_types_0100::Privkey::from(&[2; 32]).pubkey();
    let make_session = |hash_byte: u8| fiber_types_0100::PaymentSession {
        request: fiber_types_0100::SendPaymentData {
            target_pubkey: target,
            amount: 42_000,
            payment_hash: fiber_types_0100::Hash256::from([hash_byte; 32]),
            invoice: None,
            final_tlc_expiry_delta: 100,
            tlc_expiry_limit: 1000,
            timeout: None,
            max_fee_amount: Some(1_000),
            max_parts: None,
            keysend: false,
            udt_type_script: None,
            preimage: None,
            custom_records: None,
            allow_self_payment: false,
            hop_hints: vec![],
            router: vec![],
            allow_mpp: false,
            dry_run: false,
            trampoline_hops: None,
            trampoline_context: None,
        },
        last_error: Some("interrupted error".to_owned()),
        last_error_code: None,
        try_limit: 3,
        status: fiber_types_0100::PaymentStatus::Failed,
        created_at: 100,
        last_updated_at: 200,
        cached_attempts: vec![],
    };

    // 1. Half-migrated payment sessions: one already converted, one still in released layout.
    let old_session_done = make_session(0x21);
    let converted_session_done: fiber_types::PaymentSession =
        super::super::convert_named(old_session_done.clone()).unwrap();
    let session_done_key = [
        &[fiber_types::schema::PAYMENT_SESSION_PREFIX],
        converted_session_done.request.payment_hash.as_ref(),
    ]
    .concat();
    let old_session_pending = make_session(0x22);
    let session_pending_key = [
        &[fiber_types::schema::PAYMENT_SESSION_PREFIX],
        old_session_pending.request.payment_hash.as_ref(),
    ]
    .concat();
    store.put(
        &session_done_key,
        bincode::serialize(&converted_session_done).unwrap(),
    );
    store.put(
        &session_pending_key,
        bincode::serialize(&old_session_pending).unwrap(),
    );

    // 2. Half-migrated attempts: one already converted (trailing upstream_tlc_key byte), one old.
    let old_attempt_done = old_session_done.new_attempt(1, target, target, vec![]);
    let converted_attempt_done: fiber_types::Attempt =
        super::super::convert_named(old_attempt_done.clone()).unwrap();
    let attempt_done_key = [
        &[fiber_types::schema::ATTEMPT_PREFIX],
        converted_attempt_done.payment_hash.as_ref(),
        &converted_attempt_done.id.to_le_bytes(),
    ]
    .concat();
    let old_attempt_pending = old_session_pending.new_attempt(2, target, target, vec![]);
    let attempt_pending_key = [
        &[fiber_types::schema::ATTEMPT_PREFIX],
        old_attempt_pending.payment_hash.as_ref(),
        &old_attempt_pending.id.to_le_bytes(),
    ]
    .concat();
    store.put(
        &attempt_done_key,
        bincode::serialize(&converted_attempt_done).unwrap(),
    );
    store.put(
        &attempt_pending_key,
        bincode::serialize(&old_attempt_pending).unwrap(),
    );

    // 3. Half-migrated V2 watchtower channels (with non-empty 0x02-prefixed TLC keys).
    let raw_key = [0x02u8; 32];
    let old_tlc = fiber_types_0100::SettlementTlc {
        tlc_id: fiber_types_0100::TLCId::Offered(3),
        hash_algorithm: fiber_types_0100::HashAlgorithm::CkbHash,
        payment_amount: 10_000,
        payment_hash: fiber_types_0100::Hash256::from([0x33; 32]),
        expiry: 1_700_000_000_000,
        local_key: fiber_types_0100::Privkey::from(&raw_key),
        remote_key: target,
    };
    let old_settlement = fiber_types_0100::SettlementData {
        local_amount: 50_000,
        remote_amount: 60_000,
        tlcs: vec![old_tlc],
    };
    let mut old_watch_done = super::super::OldChannelData::samples(91).remove(0);
    old_watch_done.commitment_contract_features =
        fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH;
    old_watch_done.local_settlement_data = old_settlement.clone();
    let converted_watch_done = super::super::convert_watch_channel(old_watch_done.clone()).unwrap();
    let watch_done_key = [
        &[WATCHTOWER_CHANNEL_PREFIX],
        converted_watch_done.channel_id.as_ref(),
    ]
    .concat();
    let mut old_watch_pending = super::super::OldChannelData::samples(92).remove(0);
    old_watch_pending.commitment_contract_features =
        fiber_types_0100::CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH;
    old_watch_pending.remote_settlement_data = old_settlement.clone();
    let watch_pending_key = [
        &[WATCHTOWER_CHANNEL_PREFIX],
        old_watch_pending.channel_id.as_ref(),
    ]
    .concat();
    store.put(
        &watch_done_key,
        bincode::serialize(&converted_watch_done).unwrap(),
    );
    store.put(
        &watch_pending_key,
        bincode::serialize(&old_watch_pending).unwrap(),
    );

    // 4. Half-migrated superseded settlements with non-empty TLC snapshots.
    let converted_settlement = super::super::convert_settlement(old_settlement.clone()).unwrap();
    let superseded_done_key = [
        &[WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX],
        converted_watch_done.channel_id.as_ref(),
    ]
    .concat();
    let superseded_pending_key = [
        &[WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX],
        old_watch_pending.channel_id.as_ref(),
    ]
    .concat();
    store.put(
        &superseded_done_key,
        bincode::serialize(&vec![converted_settlement.clone()]).unwrap(),
    );
    store.put(
        &superseded_pending_key,
        bincode::serialize(&vec![old_settlement]).unwrap(),
    );

    // db-version is STILL RELEASED_VERSION, simulating a crash before version advancement.
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        RELEASED_VERSION.as_bytes()
    );
    migrate(&store).expect("interrupted migration must resume and complete cleanly");
    assert_eq!(
        store.get(MIGRATION_VERSION_KEY).unwrap(),
        MIGRATION_DB_VERSION.as_bytes()
    );

    // Verify both pre-converted and newly converted records decode cleanly as current types.
    let s1: fiber_types::PaymentSession =
        decode_exact(&store.get(&session_done_key).unwrap()).unwrap();
    let s2: fiber_types::PaymentSession =
        decode_exact(&store.get(&session_pending_key).unwrap()).unwrap();
    assert_eq!(s1.request.upstream_tlc_key, None);
    assert_eq!(s2.request.upstream_tlc_key, None);

    let a1: fiber_types::Attempt = decode_exact(&store.get(&attempt_done_key).unwrap()).unwrap();
    let a2: fiber_types::Attempt = decode_exact(&store.get(&attempt_pending_key).unwrap()).unwrap();
    assert_eq!(a1.upstream_tlc_key, None);
    assert_eq!(a2.upstream_tlc_key, None);
    assert_eq!(a2.id, 2);

    let w1: fiber_types::ChannelData = decode_exact(&store.get(&watch_done_key).unwrap()).unwrap();
    let w2: fiber_types::ChannelData =
        decode_exact(&store.get(&watch_pending_key).unwrap()).unwrap();
    assert_eq!(w1.local_settlement_data.tlcs.len(), 1);
    assert_eq!(w2.remote_settlement_data.tlcs.len(), 1);

    let sup1: Vec<fiber_types::SettlementData> =
        decode_exact(&store.get(&superseded_done_key).unwrap()).unwrap();
    let sup2: Vec<fiber_types::SettlementData> =
        decode_exact(&store.get(&superseded_pending_key).unwrap()).unwrap();
    assert_eq!(sup1, vec![converted_settlement.clone()]);
    assert_eq!(sup2, vec![converted_settlement]);
}
