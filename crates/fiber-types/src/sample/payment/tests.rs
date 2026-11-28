use super::*;

#[test]
fn test_payment_custom_records_samples_roundtrip() {
    PaymentCustomRecords::verify_samples_roundtrip(42);
}
