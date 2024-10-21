use crate::fiber::history::{PaymentHistory, TimedResult};
use crate::fiber::tests::test_utils::{generate_pubkey, MemoryStore};
use crate::fiber::types::Pubkey;
use crate::store::Store;
use tempfile::tempdir;

trait Round {
    fn round_to_2(self) -> f64;
}

impl Round for f64 {
    fn round_to_2(self) -> f64 {
        (self * 100.0).round() / 100.0
    }
}

#[test]
fn test_history() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, MemoryStore::default());
    let from: Pubkey = generate_pubkey().into();
    let target: Pubkey = generate_pubkey().into();

    let result1 = TimedResult {
        fail_time: 1,
        fail_amount: 2,
        success_time: 3,
        success_amount: 4,
    };
    history.add_result(from, target, result1);
    assert_eq!(history.get_result(&from, &target), Some(&result1));

    let target2 = generate_pubkey().into();
    let result2 = TimedResult {
        fail_time: 5,
        fail_amount: 6,
        success_time: 7,
        success_amount: 8,
    };

    history.add_result(from, target2, result2);
    assert_eq!(history.get_result(&from, &target2), Some(&result2));
}

#[test]
fn test_history_apply_channel_result() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, MemoryStore::default());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    history.apply_pair_result(from, target, 10, false, 11);
    assert_eq!(
        history.get_result(&from, &target),
        Some(&TimedResult {
            fail_time: 11,
            fail_amount: 10,
            success_time: 0,
            success_amount: 0,
        })
    );

    let target2 = generate_pubkey();
    history.apply_pair_result(from, target2, 10, true, 12);
    assert_eq!(
        history.get_result(&from, &target2),
        Some(&TimedResult {
            fail_time: 0,
            fail_amount: 0,
            success_time: 12,
            success_amount: 10,
        })
    );
}

#[test]
fn test_history_interal_success_fail() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, MemoryStore::default());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let result = TimedResult {
        fail_time: 1,
        fail_amount: 2,
        success_time: 3,
        success_amount: 4,
    };
    history.add_result(from, target, result);

    history.apply_pair_result(from, target, 10, true, 11);
    assert_eq!(
        history.get_result(&from, &target),
        Some(&TimedResult {
            fail_time: 1,
            fail_amount: 11, // amount + 1
            success_time: 11,
            success_amount: 10,
        })
    );

    // time is too short
    history.apply_pair_result(from, target, 12, false, 13);
    assert_eq!(
        history.get_result(&from, &target),
        Some(&TimedResult {
            fail_time: 1,
            fail_amount: 11,
            success_time: 11,
            success_amount: 10,
        })
    );

    history.apply_pair_result(from, target, 12, false, 61 * 1000);
    assert_eq!(
        history.get_result(&from, &target),
        Some(&TimedResult {
            fail_time: 61 * 1000,
            fail_amount: 12,
            success_time: 11,   // will not update
            success_amount: 10, // will not update
        })
    );

    history.apply_pair_result(from, target, 9, false, 61 * 1000 * 2);
    assert_eq!(
        history.get_result(&from, &target),
        Some(&TimedResult {
            fail_time: 61 * 1000 * 2,
            fail_amount: 9,
            success_time: 11,
            success_amount: 8, // amount - 1
        })
    );
}

#[test]
fn test_history_probability() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, MemoryStore::default());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.get_channel_probability(from, target.clone(), 10, 100);
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 10,
        fail_amount: 10,
    };
    history.add_result(from, target, result);
    assert_eq!(
        history.get_channel_probability(from, target.clone(), 1, 10),
        1.0
    );
    assert_eq!(
        history.get_channel_probability(from, target.clone(), 1, 8),
        1.0
    );

    // graph of amount is less than history's success_amount and fail_amount
    assert_eq!(
        history.get_channel_probability(from, target.clone(), 1, 4),
        1.0
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 5, 9)
            .round_to_2(),
        1.0
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 6, 9)
            .round_to_2(),
        0.75
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 7, 9)
            .round_to_2(),
        0.50
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 8, 9)
            .round_to_2(),
        0.25
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 5, 10)
            .round_to_2(),
        1.0
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 6, 10)
            .round_to_2(),
        0.80
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 7, 10)
            .round_to_2(),
        0.60
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 8, 10)
            .round_to_2(),
        0.40
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 9, 10)
            .round_to_2(),
        0.20
    );

    assert_eq!(
        history
            .get_channel_probability(from, target.clone(), 10, 10)
            .round_to_2(),
        0.0
    );
}

#[test]
fn test_history_direct_probability() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, MemoryStore::default());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.get_direct_probability(from, target.clone());
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 0,
        fail_amount: 0,
    };
    history.add_result(from, target, result);
    assert_eq!(history.get_direct_probability(from, target.clone()), 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 10,
        fail_amount: 10,
    };
    history.add_result(from, target, result);
    let prob = history.get_direct_probability(from, target.clone());
    eprintln!("prob: {}", prob);
    assert_eq!(prob, 0.0);
}

#[test]
fn test_history_probability_small_fail_amount() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, MemoryStore::default());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.get_channel_probability(from, target.clone(), 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 50000000,
        fail_time: 10,
        fail_amount: 10,
    };
    history.add_result(from, target, result);
    assert_eq!(
        history.get_channel_probability(from, target.clone(), 50000000, 100000000),
        0.0
    );
}

#[test]
fn test_history_probability_range() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, MemoryStore::default());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.get_channel_probability(from, target.clone(), 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 10000000,
        fail_time: 10,
        fail_amount: 50000000,
    };

    history.add_result(from, target, result);

    for amount in (1..10000000).step_by(100000) {
        let prob = history.get_channel_probability(from, target.clone(), amount, 100000000);
        assert_eq!(prob, 1.0);
    }

    let mut prev_prob = history.get_channel_probability(from, target.clone(), 10000000, 100000000);
    for amount in (10000005..50000000).step_by(10000) {
        let prob = history.get_channel_probability(from, target.clone(), amount, 100000000);
        eprintln!(
            "amount: {}, prob: {}, prev_prob: {}",
            amount, prob, prev_prob
        );
        assert!(prob < prev_prob);
        prev_prob = prob;
    }

    for amount in (50000001..100000000).step_by(100000) {
        let prob = history.get_channel_probability(from, target.clone(), amount, 100000000);
        assert_eq!(prob, 0.0);
    }
}

#[test]
fn test_history_load_store() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test_history_load_store");
    let store = Store::new(path);
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, store.clone());
    let from = generate_pubkey();
    let target = generate_pubkey();

    let result = TimedResult {
        success_time: 3,
        success_amount: 10000000,
        fail_time: 10,
        fail_amount: 50000000,
    };

    history.add_result(from, target, result);
    let result = history.get_result(&from, &target).unwrap().clone();
    history.reset();
    assert_eq!(history.get_result(&from, &target), None);
    history.load_from_store();
    assert_eq!(history.get_result(&from, &target), Some(&result));

    history.apply_pair_result(from, target, 1, false, 11);
    history.reset();
    history.load_from_store();
    assert_eq!(
        history.get_result(&from, &target),
        Some(&TimedResult {
            fail_time: 11,
            fail_amount: 1,
            success_time: 3,
            success_amount: 0,
        })
    );
}
