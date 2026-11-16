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
