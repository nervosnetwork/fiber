use crate::payment::TimedResult;
use crate::schema::PAYMENT_HISTORY_TIMED_RESULT_PREFIX;

use super::StoreSample;

impl StoreSample for TimedResult {
    const STORE_PREFIX: u8 = PAYMENT_HISTORY_TIMED_RESULT_PREFIX;
    const TYPE_NAME: &'static str = "TimedResult";

    fn samples(_seed: u64) -> Vec<Self> {
        vec![
            // Default/empty result
            TimedResult {
                fail_time: 0,
                fail_amount: 0,
                success_time: 0,
                success_amount: 0,
            },
            // Failed only
            TimedResult {
                fail_time: 1_704_067_200_000,
                fail_amount: 1_000_000,
                success_time: 0,
                success_amount: 0,
            },
            // Success only
            TimedResult {
                fail_time: 0,
                fail_amount: 0,
                success_time: 1_704_067_300_000,
                success_amount: 5_000_000,
            },
            // Both failed and success
            TimedResult {
                fail_time: 1_704_067_200_000,
                fail_amount: 500_000,
                success_time: 1_704_067_400_000,
                success_amount: 10_000_000,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timed_result_samples_roundtrip() {
        TimedResult::verify_samples_roundtrip(42);
    }
}
