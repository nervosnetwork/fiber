use crate::invoice::{
    Attribute, CkbInvoice, CkbInvoiceStatus, Currency, InvoiceData, InvoiceSignature,
};
use core::time::Duration;
use fiber_types::schema::{CKB_INVOICE_PREFIX, CKB_INVOICE_STATUS_PREFIX};
/// StoreSample implementation for `CkbInvoice` and `CkbInvoiceStatus`.
use fiber_types::HashAlgorithm;

use super::{deterministic_hash256, deterministic_recoverable_signature, StoreSample};

impl StoreSample for CkbInvoice {
    const STORE_PREFIX: u8 = CKB_INVOICE_PREFIX;
    const TYPE_NAME: &'static str = "CkbInvoice";

    fn samples(seed: u64) -> Vec<Self> {
        vec![sample_minimal(seed), sample_full(seed)]
    }
}

impl StoreSample for CkbInvoiceStatus {
    const STORE_PREFIX: u8 = CKB_INVOICE_STATUS_PREFIX;
    const TYPE_NAME: &'static str = "CkbInvoiceStatus";

    fn samples(_seed: u64) -> Vec<Self> {
        // All enum variants to ensure each one round-trips.
        vec![
            CkbInvoiceStatus::Open,
            CkbInvoiceStatus::Cancelled,
            CkbInvoiceStatus::Expired,
            CkbInvoiceStatus::Received,
            CkbInvoiceStatus::Paid,
        ]
    }
}

/// Minimal CkbInvoice: required fields only, Options are None, no attributes.
fn sample_minimal(seed: u64) -> CkbInvoice {
    CkbInvoice {
        currency: Currency::Fibd,
        amount: None,
        signature: None,
        data: InvoiceData {
            timestamp: 1_704_067_200_000,
            payment_hash: deterministic_hash256(seed, 0),
            attrs: vec![],
        },
    }
}

/// Full CkbInvoice: all Options are Some, multiple attributes present.
fn sample_full(seed: u64) -> CkbInvoice {
    let recoverable_sig = deterministic_recoverable_signature(seed, 50);
    CkbInvoice {
        currency: Currency::Fibt,
        amount: Some(100_000_000_000),
        signature: Some(InvoiceSignature(recoverable_sig)),
        data: InvoiceData {
            timestamp: 1_704_070_800_000,
            payment_hash: deterministic_hash256(seed, 10),
            attrs: vec![
                Attribute::FinalHtlcTimeout(9_600_000),
                Attribute::FinalHtlcMinimumExpiryDelta(9_600_000),
                Attribute::ExpiryTime(Duration::from_secs(3600)),
                Attribute::Description("test invoice".to_string()),
                Attribute::FallbackAddr(
                    "ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsq...".to_string(),
                ),
                Attribute::HashAlgorithm(HashAlgorithm::CkbHash),
                Attribute::PaymentSecret(deterministic_hash256(seed, 11)),
            ],
        },
    }
}

#[cfg(test)]
mod tests {
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
}
