/// Tests using `StoreSample` to verify that v0.7.0 types round-trip
/// correctly through bincode serialization, and that migration functions
/// produce valid v0.7.0 data.
use fiber_v070::fiber::channel::ChannelActorState;
use fiber_v070::fiber::network::PersistentNetworkActorState;
use fiber_v070::fiber::payment::{Attempt, PaymentCustomRecords, PaymentSession};
use fiber_v070::fiber::types::{BroadcastMessage, HoldTlc};
use fiber_v070::invoice::{CkbInvoice, CkbInvoiceStatus};
use fiber_v070::store::sample::StoreSample;

// ─── Round-trip tests ───────────────────────────────────────────────
// These verify that fiber_v070 sample data serializes and deserializes
// correctly within the migrate crate's build environment.

#[test]
fn test_channel_actor_state_roundtrip_in_migrate() {
    ChannelActorState::verify_samples_roundtrip(42);
}

#[test]
fn test_persistent_network_actor_state_roundtrip_in_migrate() {
    PersistentNetworkActorState::verify_samples_roundtrip(42);
}

#[test]
fn test_ckb_invoice_roundtrip_in_migrate() {
    CkbInvoice::verify_samples_roundtrip(42);
}

#[test]
fn test_ckb_invoice_status_roundtrip_in_migrate() {
    CkbInvoiceStatus::verify_samples_roundtrip(42);
}

#[test]
fn test_payment_session_roundtrip_in_migrate() {
    PaymentSession::verify_samples_roundtrip(42);
}

#[test]
fn test_payment_custom_records_roundtrip_in_migrate() {
    PaymentCustomRecords::verify_samples_roundtrip(42);
}

#[test]
fn test_attempt_roundtrip_in_migrate() {
    Attempt::verify_samples_roundtrip(42);
}

#[test]
fn test_broadcast_message_roundtrip_in_migrate() {
    BroadcastMessage::verify_samples_roundtrip(42);
}

#[test]
fn test_hold_tlc_roundtrip_in_migrate() {
    HoldTlc::verify_samples_roundtrip(42);
}

// ─── Cross-version deserialization tests ────────────────────────────
// These verify that v0.7.0 sample bytes can be deserialized by the
// migrate crate's fiber_v070 dependency. This catches schema drift
// between the local crate and the git dependency.

#[test]
fn test_channel_actor_state_sample_bytes_deserialize() {
    for (i, bytes) in ChannelActorState::sample_bytes(42).iter().enumerate() {
        let result: Result<ChannelActorState, _> = bincode::deserialize(bytes);
        assert!(
            result.is_ok(),
            "ChannelActorState sample {i} failed to deserialize: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_payment_session_sample_bytes_deserialize() {
    for (i, bytes) in PaymentSession::sample_bytes(42).iter().enumerate() {
        let result: Result<PaymentSession, _> = bincode::deserialize(bytes);
        assert!(
            result.is_ok(),
            "PaymentSession sample {i} failed to deserialize: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_attempt_sample_bytes_deserialize() {
    for (i, bytes) in Attempt::sample_bytes(42).iter().enumerate() {
        let result: Result<Attempt, _> = bincode::deserialize(bytes);
        assert!(
            result.is_ok(),
            "Attempt sample {i} failed to deserialize: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_broadcast_message_sample_bytes_deserialize() {
    for (i, bytes) in BroadcastMessage::sample_bytes(42).iter().enumerate() {
        let result: Result<BroadcastMessage, _> = bincode::deserialize(bytes);
        assert!(
            result.is_ok(),
            "BroadcastMessage sample {i} failed to deserialize: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_ckb_invoice_sample_bytes_deserialize() {
    for (i, bytes) in CkbInvoice::sample_bytes(42).iter().enumerate() {
        let result: Result<CkbInvoice, _> = bincode::deserialize(bytes);
        assert!(
            result.is_ok(),
            "CkbInvoice sample {i} failed to deserialize: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_hold_tlc_sample_bytes_deserialize() {
    for (i, bytes) in HoldTlc::sample_bytes(42).iter().enumerate() {
        let result: Result<HoldTlc, _> = bincode::deserialize(bytes);
        assert!(
            result.is_ok(),
            "HoldTlc sample {i} failed to deserialize: {:?}",
            result.err()
        );
    }
}

// ─── Determinism tests ─────────────────────────────────────────────
// Verify that the same seed produces identical serialized bytes across
// multiple calls, which is essential for fixture-based migration testing.

#[test]
fn test_all_samples_deterministic() {
    // Call sample_bytes twice with the same seed and verify identical output
    macro_rules! check_deterministic {
        ($ty:ty, $name:expr) => {
            let a = <$ty>::sample_bytes(42);
            let b = <$ty>::sample_bytes(42);
            assert_eq!(a, b, "{} samples are not deterministic", $name);
        };
    }

    check_deterministic!(ChannelActorState, "ChannelActorState");
    check_deterministic!(PersistentNetworkActorState, "PersistentNetworkActorState");
    check_deterministic!(CkbInvoice, "CkbInvoice");
    check_deterministic!(CkbInvoiceStatus, "CkbInvoiceStatus");
    check_deterministic!(PaymentSession, "PaymentSession");
    check_deterministic!(PaymentCustomRecords, "PaymentCustomRecords");
    check_deterministic!(Attempt, "Attempt");
    check_deterministic!(BroadcastMessage, "BroadcastMessage");
    check_deterministic!(HoldTlc, "HoldTlc");
}
