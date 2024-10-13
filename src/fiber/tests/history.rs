use super::test_utils::generate_pubkey;
use crate::fiber::history::PaymentResult;
use crate::fiber::history::{PairResult, PaymentHistory};
use crate::fiber::types::Pubkey;

#[test]
fn test_history() {
    let mut history = PaymentHistory::new();
    let source: Pubkey = generate_pubkey().into();
    let target1: Pubkey = generate_pubkey().into();

    let result1 = PairResult {
        fail_time: 1,
        fail_amount: 2,
        success_time: 3,
        success_amount: 4,
    };
    history.add_result(source.clone(), target1.clone(), result1);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(target1, result1)].into_iter().collect()
        })
    );

    let target2: Pubkey = generate_pubkey().into();
    let result2 = PairResult {
        fail_time: 5,
        fail_amount: 6,
        success_time: 7,
        success_amount: 8,
    };
    history.add_result(source.clone(), target2.clone(), result2);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(target1, result1.clone()), (target2, result2)]
                .into_iter()
                .collect()
        })
    );
}
