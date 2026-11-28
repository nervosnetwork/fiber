use super::*;

#[test]
fn test_ckb_invoice_samples_roundtrip() {
    CkbInvoice::verify_samples_roundtrip(42);
}

#[test]
fn test_ckb_invoice_samples_deterministic() {
    let bytes_a = CkbInvoice::sample_bytes(42);
    let bytes_b = CkbInvoice::sample_bytes(42);
    assert_eq!(bytes_a, bytes_b, "Same seed must produce identical bytes");
}

#[test]
fn test_ckb_invoice_different_seeds() {
    let bytes_42 = CkbInvoice::sample_bytes(42);
    let bytes_99 = CkbInvoice::sample_bytes(99);
    assert_ne!(
        bytes_42, bytes_99,
        "Different seeds should produce different bytes"
    );
}

#[test]
fn test_ckb_invoice_status_samples_roundtrip() {
    CkbInvoiceStatus::verify_samples_roundtrip(42);
}

#[test]
fn test_ckb_invoice_status_sample_count() {
    let samples = CkbInvoiceStatus::samples(42);
    assert_eq!(samples.len(), 5, "Should produce all 5 enum variants");
}

#[test]
fn test_ckb_invoice_full_no_none() {
    let full = sample_full(42);
    assert!(full.amount.is_some());
    assert!(full.signature.is_some());
    assert!(!full.data.attrs.is_empty());
}
