/// StoreSample implementations for payment types.
use std::collections::HashMap;

use crate::payment::PaymentCustomRecords;
use crate::schema::PAYMENT_CUSTOM_RECORD_PREFIX;

use super::StoreSample;

impl StoreSample for PaymentCustomRecords {
    const STORE_PREFIX: u8 = PAYMENT_CUSTOM_RECORD_PREFIX;
    const TYPE_NAME: &'static str = "PaymentCustomRecords";

    fn samples(_seed: u64) -> Vec<Self> {
        vec![
            PaymentCustomRecords {
                data: HashMap::new(),
            },
            PaymentCustomRecords {
                data: {
                    let mut map = HashMap::new();
                    map.insert(1, vec![1, 2, 3]);
                    map.insert(2, vec![4, 5, 6]);
                    map
                },
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payment_custom_records_samples_roundtrip() {
        PaymentCustomRecords::verify_samples_roundtrip(42);
    }
}
