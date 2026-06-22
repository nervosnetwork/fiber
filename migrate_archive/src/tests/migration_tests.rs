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

// ─── CchOrder migration tests (mig_20260421_cch_multi_asset) ─────────

mod cch_multi_asset {
    use crate::migrations::mig_20260421_cch_multi_asset::{migrate_cch_order, NewCchOrder};
    use fiber_v070::cch::{CchInvoice, CchOrder as OldCchOrder, CchOrderStatus};
    use fiber_v070::fiber::types::Hash256;

    /// Build a real `Bolt11Invoice` using `lightning_invoice::InvoiceBuilder`
    /// so the test does not depend on a hand-crafted bech32 string surviving
    /// upstream BOLT-11 checksum/version bumps.
    fn build_test_lightning_invoice() -> lightning_invoice::Bolt11Invoice {
        use bitcoin::hashes::Hash as _;
        use lightning_invoice::{Currency as LnCurrency, InvoiceBuilder, PaymentSecret};
        let secp = bitcoin::secp256k1::Secp256k1::new();
        let private_key = bitcoin::secp256k1::SecretKey::from_slice(&[7u8; 32]).unwrap();
        let payment_hash =
            bitcoin::hashes::sha256::Hash::from_slice(&[0xAA; 32]).expect("32-byte hash");
        let duration_since_epoch = std::time::Duration::from_secs(1_700_000_000);
        InvoiceBuilder::new(LnCurrency::Bitcoin)
            .description("migration test".to_string())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([0u8; 32]))
            .duration_since_epoch(duration_since_epoch)
            .min_final_cltv_expiry_delta(36)
            .amount_milli_satoshis(100_000_000)
            .build_signed(|h| secp.sign_ecdsa_recoverable(h, &private_key))
            .expect("build BOLT-11 invoice")
    }

    fn build_old_cch_order() -> OldCchOrder {
        let payment_hash: Hash256 = [0xAA; 32].into();
        let preimage: Hash256 = [0xBB; 32].into();
        let invoice = build_test_lightning_invoice();
        let outgoing_pay_req = invoice.to_string();

        OldCchOrder {
            created_at: 1_700_000_000,
            expiry_delta_seconds: 3_600,
            wrapped_btc_type_script: ckb_jsonrpc_types_legacy::Script::default(),
            outgoing_pay_req,
            incoming_invoice: CchInvoice::Lightning(invoice),
            payment_hash,
            payment_preimage: Some(preimage),
            amount_sats: 100_000,
            fee_sats: 250,
            status: CchOrderStatus::Pending,
            failure_reason: None,
        }
    }

    /// End-to-end roundtrip mirroring what
    /// `mig_20260421_cch_multi_asset::MigrationObj::migrate` does in
    /// production: bincode-serialize the v0.7.0 `CchOrder`, deserialize it
    /// back through the snapshot type, run `migrate_cch_order`, then
    /// serialize the new layout and confirm it deserializes via the
    /// migration's `NewCchOrder` shadow type. Catches any drift in either
    /// the old or new bincode layouts (which would otherwise only surface
    /// on a real upgrade).
    #[test]
    fn test_migrate_cch_order_bytes_roundtrip() {
        let old = build_old_cch_order();
        let old_bytes = bincode::serialize(&old).expect("serialize old CchOrder");

        let decoded: OldCchOrder =
            bincode::deserialize(&old_bytes).expect("deserialize old CchOrder");
        let new = migrate_cch_order(decoded);

        let new_bytes = bincode::serialize(&new).expect("serialize new CchOrder");
        let _: NewCchOrder =
            bincode::deserialize(&new_bytes).expect("deserialize new CchOrder");
    }

    /// Field-by-field check that the migration preserves identifying data
    /// and applies the documented sat→msat conversion plus the legacy
    /// 1:1 sat-to-smallest-unit assumption for the new
    /// `fiber_invoice_amount` field.
    #[test]
    fn test_migrate_cch_order_preserves_fields() {
        let old = build_old_cch_order();
        let old_amount_sats = old.amount_sats;
        let old_fee_sats = old.fee_sats;
        let old_payment_hash = old.payment_hash;
        let old_created_at = old.created_at;
        let old_expiry_delta = old.expiry_delta_seconds;
        let old_pay_req = old.outgoing_pay_req.clone();

        let new = migrate_cch_order(old);

        assert_eq!(new.created_at, old_created_at);
        assert_eq!(new.expiry_delta_seconds, old_expiry_delta);
        assert_eq!(new.outgoing_pay_req, old_pay_req);
        assert_eq!(new.payment_hash, old_payment_hash);
        // Legacy 1:1 wrapped-BTC fee math: 1 sat == 1 smallest unit on
        // the Fiber side, BTC amounts shift sat → msat (×1000).
        assert_eq!(new.lightning_invoice_amount, old_amount_sats * 1_000);
        assert_eq!(new.btc_fee_msat, old_fee_sats * 1_000);
        assert_eq!(new.fiber_invoice_amount, old_amount_sats);
        // Legacy single-asset orders all carried a wrapped-BTC type
        // script, so the migration always sets `Some(_)`.
        assert!(new.fiber_type_script.is_some());
    }
}
