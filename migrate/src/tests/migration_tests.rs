/// Migration tests: generate v0.6.1 sample data, run migration functions,
/// and verify the output deserializes correctly as v0.7.0 types.
use crate::migrations::mig_20260203_trampoline::{
    migrate_attempt, migrate_channel_state, migrate_payment_session,
};
use fiber_v061::fiber::channel::ChannelActorState as OldChannelActorState;
use fiber_v061::fiber::payment::{Attempt as OldAttempt, PaymentSession as OldPaymentSession};
use fiber_v061::store::sample::StoreSample;
use fiber_v070::fiber::channel::ChannelActorState as NewChannelActorState;
use fiber_v070::fiber::payment::{Attempt as NewAttempt, PaymentSession as NewPaymentSession};

// ─── ChannelActorState migration tests ──────────────────────────────

#[test]
fn test_migrate_channel_state_from_v061_samples() {
    let old_samples = OldChannelActorState::samples(42);
    for (i, old) in old_samples.into_iter().enumerate() {
        let new = migrate_channel_state(old);

        // The migrated state should roundtrip through bincode
        let bytes = bincode::serialize(&new).unwrap_or_else(|e| {
            panic!("ChannelActorState sample {i}: serialize after migration failed: {e}")
        });
        let deserialized: NewChannelActorState = bincode::deserialize(&bytes).unwrap_or_else(|e| {
            panic!("ChannelActorState sample {i}: deserialize after migration failed: {e}")
        });

        // Verify new fields have correct default values
        assert!(
            !deserialized.is_one_way,
            "ChannelActorState sample {i}: is_one_way should default to false"
        );
    }
}

#[test]
fn test_migrate_channel_state_bytes_roundtrip() {
    // Simulate what the real migration does: serialize v0.6.1 data,
    // then deserialize as v0.6.1, migrate, serialize as v0.7.0
    let old_bytes_list = OldChannelActorState::sample_bytes(42);
    for (i, old_bytes) in old_bytes_list.iter().enumerate() {
        let old: OldChannelActorState = bincode::deserialize(old_bytes).unwrap_or_else(|e| {
            panic!("ChannelActorState sample {i}: deserialize as v0.6.1 failed: {e}")
        });

        let new = migrate_channel_state(old);

        let new_bytes = bincode::serialize(&new).unwrap_or_else(|e| {
            panic!("ChannelActorState sample {i}: serialize as v0.7.0 failed: {e}")
        });

        let result: Result<NewChannelActorState, _> = bincode::deserialize(&new_bytes);
        assert!(
            result.is_ok(),
            "ChannelActorState sample {i}: v0.7.0 deserialize failed: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_migrate_channel_state_preserves_fields() {
    let old_samples = OldChannelActorState::samples(42);
    for (i, old) in old_samples.into_iter().enumerate() {
        let old_is_acceptor = old.is_acceptor;
        let old_to_local_amount = old.to_local_amount;
        let old_to_remote_amount = old.to_remote_amount;
        let old_commitment_fee_rate = old.commitment_fee_rate;

        let new = migrate_channel_state(old);

        assert_eq!(
            new.is_acceptor, old_is_acceptor,
            "ChannelActorState sample {i}: is_acceptor should be preserved"
        );
        assert_eq!(
            new.to_local_amount, old_to_local_amount,
            "ChannelActorState sample {i}: to_local_amount should be preserved"
        );
        assert_eq!(
            new.to_remote_amount, old_to_remote_amount,
            "ChannelActorState sample {i}: to_remote_amount should be preserved"
        );
        assert_eq!(
            new.commitment_fee_rate, old_commitment_fee_rate,
            "ChannelActorState sample {i}: commitment_fee_rate should be preserved"
        );
    }
}

// ─── PaymentSession migration tests ─────────────────────────────────

#[test]
fn test_migrate_payment_session_from_v061_samples() {
    let old_samples = OldPaymentSession::samples(42);
    for (i, old) in old_samples.into_iter().enumerate() {
        let new = migrate_payment_session(old);

        let bytes = bincode::serialize(&new).unwrap_or_else(|e| {
            panic!("PaymentSession sample {i}: serialize after migration failed: {e}")
        });
        let deserialized: NewPaymentSession = bincode::deserialize(&bytes).unwrap_or_else(|e| {
            panic!("PaymentSession sample {i}: deserialize after migration failed: {e}")
        });

        // Verify new fields have correct default values
        assert!(
            deserialized.last_error_code.is_none(),
            "PaymentSession sample {i}: last_error_code should default to None"
        );
        assert!(
            deserialized.request.trampoline_hops.is_none(),
            "PaymentSession sample {i}: trampoline_hops should default to None"
        );
        assert!(
            deserialized.request.trampoline_context.is_none(),
            "PaymentSession sample {i}: trampoline_context should default to None"
        );
    }
}

#[test]
fn test_migrate_payment_session_bytes_roundtrip() {
    let old_bytes_list = OldPaymentSession::sample_bytes(42);
    for (i, old_bytes) in old_bytes_list.iter().enumerate() {
        let old: OldPaymentSession = bincode::deserialize(old_bytes).unwrap_or_else(|e| {
            panic!("PaymentSession sample {i}: deserialize as v0.6.1 failed: {e}")
        });

        let new = migrate_payment_session(old);

        let new_bytes = bincode::serialize(&new).unwrap_or_else(|e| {
            panic!("PaymentSession sample {i}: serialize as v0.7.0 failed: {e}")
        });

        let result: Result<NewPaymentSession, _> = bincode::deserialize(&new_bytes);
        assert!(
            result.is_ok(),
            "PaymentSession sample {i}: v0.7.0 deserialize failed: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_migrate_payment_session_preserves_fields() {
    let old_samples = OldPaymentSession::samples(42);
    for (i, old) in old_samples.into_iter().enumerate() {
        let old_try_limit = old.try_limit;
        let old_created_at = old.created_at;
        let old_amount = old.request.amount;
        let old_keysend = old.request.keysend;

        let new = migrate_payment_session(old);

        assert_eq!(
            new.try_limit, old_try_limit,
            "PaymentSession sample {i}: try_limit should be preserved"
        );
        assert_eq!(
            new.created_at, old_created_at,
            "PaymentSession sample {i}: created_at should be preserved"
        );
        assert_eq!(
            new.request.amount, old_amount,
            "PaymentSession sample {i}: amount should be preserved"
        );
        assert_eq!(
            new.request.keysend, old_keysend,
            "PaymentSession sample {i}: keysend should be preserved"
        );
    }
}

// ─── Attempt migration tests ────────────────────────────────────────

#[test]
fn test_migrate_attempt_from_v061_samples() {
    let old_samples = OldAttempt::samples(42);
    for (i, old) in old_samples.into_iter().enumerate() {
        let new = migrate_attempt(old);

        let bytes = bincode::serialize(&new).unwrap_or_else(|e| {
            panic!("Attempt sample {i}: serialize after migration failed: {e}")
        });
        let _deserialized: NewAttempt = bincode::deserialize(&bytes).unwrap_or_else(|e| {
            panic!("Attempt sample {i}: deserialize after migration failed: {e}")
        });
    }
}

#[test]
fn test_migrate_attempt_bytes_roundtrip() {
    let old_bytes_list = OldAttempt::sample_bytes(42);
    for (i, old_bytes) in old_bytes_list.iter().enumerate() {
        let old: OldAttempt = bincode::deserialize(old_bytes)
            .unwrap_or_else(|e| panic!("Attempt sample {i}: deserialize as v0.6.1 failed: {e}"));

        let new = migrate_attempt(old);

        let new_bytes = bincode::serialize(&new)
            .unwrap_or_else(|e| panic!("Attempt sample {i}: serialize as v0.7.0 failed: {e}"));

        let result: Result<NewAttempt, _> = bincode::deserialize(&new_bytes);
        assert!(
            result.is_ok(),
            "Attempt sample {i}: v0.7.0 deserialize failed: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_migrate_attempt_preserves_fields() {
    let old_samples = OldAttempt::samples(42);
    for (i, old) in old_samples.into_iter().enumerate() {
        let old_id = old.id;
        let old_try_limit = old.try_limit;
        let old_tried_times = old.tried_times;
        let old_created_at = old.created_at;
        let old_route_hops_len = old.route_hops.len();

        let new = migrate_attempt(old);

        assert_eq!(new.id, old_id, "Attempt sample {i}: id should be preserved");
        assert_eq!(
            new.try_limit, old_try_limit,
            "Attempt sample {i}: try_limit should be preserved"
        );
        assert_eq!(
            new.tried_times, old_tried_times,
            "Attempt sample {i}: tried_times should be preserved"
        );
        assert_eq!(
            new.created_at, old_created_at,
            "Attempt sample {i}: created_at should be preserved"
        );
        assert_eq!(
            new.route_hops.len(),
            old_route_hops_len,
            "Attempt sample {i}: route_hops length should be preserved"
        );
    }
}

// ─── v0.6.1 bytes → cannot deserialize as v0.7.0 (validates migration need) ─

#[test]
fn test_v061_channel_state_bytes_cannot_deserialize_as_v070() {
    // This confirms that v0.6.1 serialized data CANNOT be directly
    // deserialized as v0.7.0, validating the need for migration.
    let old_bytes_list = OldChannelActorState::sample_bytes(42);
    for old_bytes in &old_bytes_list {
        let result: Result<NewChannelActorState, _> = bincode::deserialize(old_bytes);
        // At least one sample should fail to deserialize as v0.7.0
        // (due to the is_one_way field difference)
        assert!(
            result.is_err(),
            "ChannelActorState sample unexpectedly deserialized as v0.7.0"
        );
    }
    // If all samples happen to deserialize (unlikely but possible for
    // minimal samples), that's still acceptable - the migration code
    // handles this case with the `is_ok()` skip check.
}

// ─── Determinism test for migration output ──────────────────────────

#[test]
fn test_migration_output_is_deterministic() {
    // Running migration on the same v0.6.1 sample data twice
    // should produce identical v0.7.0 bytes.
    let old_channel_samples_1 = OldChannelActorState::samples(42);
    let old_channel_samples_2 = OldChannelActorState::samples(42);
    for (s1, s2) in old_channel_samples_1
        .into_iter()
        .zip(old_channel_samples_2.into_iter())
    {
        let new1 = bincode::serialize(&migrate_channel_state(s1)).unwrap();
        let new2 = bincode::serialize(&migrate_channel_state(s2)).unwrap();
        assert_eq!(
            new1, new2,
            "ChannelActorState migration is not deterministic"
        );
    }

    let old_payment_samples_1 = OldPaymentSession::samples(42);
    let old_payment_samples_2 = OldPaymentSession::samples(42);
    for (s1, s2) in old_payment_samples_1
        .into_iter()
        .zip(old_payment_samples_2.into_iter())
    {
        let new1 = bincode::serialize(&migrate_payment_session(s1)).unwrap();
        let new2 = bincode::serialize(&migrate_payment_session(s2)).unwrap();
        assert_eq!(new1, new2, "PaymentSession migration is not deterministic");
    }

    let old_attempt_samples_1 = OldAttempt::samples(42);
    let old_attempt_samples_2 = OldAttempt::samples(42);
    for (s1, s2) in old_attempt_samples_1
        .into_iter()
        .zip(old_attempt_samples_2.into_iter())
    {
        let new1 = bincode::serialize(&migrate_attempt(s1)).unwrap();
        let new2 = bincode::serialize(&migrate_attempt(s2)).unwrap();
        assert_eq!(new1, new2, "Attempt migration is not deterministic");
    }
}
