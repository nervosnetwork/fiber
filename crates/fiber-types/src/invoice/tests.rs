use super::*;

#[test]
fn invoice_without_final_expiry_delta_uses_protocol_default() {
    let now = crate::now_timestamp_as_millis_u64();
    let invoice = CkbInvoice {
        currency: Currency::Fibd,
        amount: Some(1_000),
        signature: None,
        data: InvoiceData {
            timestamp: now.into(),
            payment_hash: Hash256::default(),
            attrs: Vec::new(),
        },
    };

    assert_eq!(
        invoice.final_tlc_minimum_expiry_delta_or_default(),
        DEFAULT_FINAL_TLC_EXPIRY_DELTA
    );
    assert!(invoice.is_tlc_expire_too_soon(now + 12 * 60 * 60 * 1_000));
    assert!(!invoice.is_tlc_expire_too_soon(now + 48 * 60 * 60 * 1_000));

    let explicit_delta = 36 * 60 * 60 * 1_000;
    let mut invoice_with_explicit_delta = invoice;
    invoice_with_explicit_delta
        .data
        .attrs
        .push(Attribute::FinalHtlcMinimumExpiryDelta(explicit_delta));
    assert_eq!(
        invoice_with_explicit_delta.final_tlc_minimum_expiry_delta_or_default(),
        explicit_delta
    );
}
