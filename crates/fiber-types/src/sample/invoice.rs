use crate::invoice::{
    Attribute, CkbInvoice, CkbInvoiceStatus, Currency, InvoiceData, InvoiceSignature,
};
use crate::schema::{CKB_INVOICE_PREFIX, CKB_INVOICE_STATUS_PREFIX};
use crate::HashAlgorithm;
use core::time::Duration;

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
mod tests;
