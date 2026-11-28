use super::*;

#[test]
fn test_cch_order_status_samples_roundtrip() {
    CchOrderStatus::verify_samples_roundtrip(42);
}
