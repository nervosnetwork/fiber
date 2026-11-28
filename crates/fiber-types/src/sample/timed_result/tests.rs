use super::*;

#[test]
fn test_timed_result_samples_roundtrip() {
    TimedResult::verify_samples_roundtrip(42);
}
