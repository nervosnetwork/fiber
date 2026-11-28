use super::*;

#[test]
fn test_hold_tlc_samples_roundtrip() {
    HoldTlc::verify_samples_roundtrip(42);
}

#[test]
fn test_hold_tlc_samples_deterministic() {
    let bytes_a = HoldTlc::sample_bytes(42);
    let bytes_b = HoldTlc::sample_bytes(42);
    assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
}

#[test]
fn test_hold_tlc_different_seeds() {
    let bytes_42 = HoldTlc::sample_bytes(42);
    let bytes_99 = HoldTlc::sample_bytes(99);
    assert_ne!(
        bytes_42, bytes_99,
        "Different seeds should produce different bytes"
    );
}

#[test]
fn test_hold_tlc_sample_count() {
    let samples = HoldTlc::samples(42);
    assert_eq!(samples.len(), 2, "Should produce 2 samples");
}
