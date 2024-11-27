use crate::fiber::graph::SessionRouteNode;
use crate::fiber::history::DEFAULT_BIMODAL_DECAY_TIME;
use crate::fiber::history::{InternalPairResult, InternalResult};
use crate::fiber::history::{PaymentHistory, TimedResult};
use crate::fiber::tests::test_utils::{generate_pubkey, generate_store};
use crate::fiber::types::Pubkey;
use crate::now_timestamp_as_millis_u64;
use crate::store::Store;
use ckb_types::packed::OutPoint;
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
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
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
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
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
fn test_history_internal_result() {
    //let mut history = PaymentHistory::new(generate_pubkey().into(), None,generate_store());

    let mut internal_result = InternalResult::default();
    let from = generate_pubkey();
    let target = generate_pubkey();
    internal_result.add(from, target, 10, 11, true);
    assert_eq!(internal_result.pairs.len(), 1);
    assert_eq!(
        internal_result.pairs.get(&(from, target)).unwrap(),
        &InternalPairResult {
            amount: 11,
            success: true,
            time: 10
        }
    );

    internal_result.add_fail_pair(from, target);
    assert_eq!(internal_result.pairs.len(), 2);

    let res = internal_result.pairs.get(&(from, target)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    assert_ne!(res.time, 0);

    let res = internal_result.pairs.get(&(target, from)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    assert_ne!(res.time, 0);

    internal_result.add_fail_pair_balanced(from, target, 100);
    assert_eq!(internal_result.pairs.len(), 2);
    let res = internal_result.pairs.get(&(from, target)).unwrap();
    assert_eq!(res.amount, 100);
    assert_eq!(res.success, false);
}

#[test]
fn test_history_internal_result_fail_pair() {
    let mut internal_result = InternalResult::default();
    let from = generate_pubkey();
    let target = generate_pubkey();

    let route = vec![
        SessionRouteNode {
            pubkey: from,
            amount: 10,
            channel_outpoint: OutPoint::default(),
        },
        SessionRouteNode {
            pubkey: target,
            amount: 5,
            channel_outpoint: OutPoint::default(),
        },
    ];

    internal_result.fail_pair(&route, 0);
    assert_eq!(internal_result.pairs.len(), 0);

    internal_result.fail_pair(&route, 1);
    assert_eq!(internal_result.pairs.len(), 2);
    let res = internal_result.pairs.get(&(from, target)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);

    let res = internal_result.pairs.get(&(target, from)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
}

#[test]
fn test_history_internal_result_success_range_pair() {
    let mut internal_result = InternalResult::default();
    let node1 = generate_pubkey();
    let node2 = generate_pubkey();
    let node3 = generate_pubkey();

    let route = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: OutPoint::default(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: OutPoint::default(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    internal_result.succeed_range_pairs(&route, 0, 2);
    assert_eq!(internal_result.pairs.len(), 2);
    let res = internal_result.pairs.get(&(node1, node2)).unwrap();
    assert_eq!(res.amount, 10);
    assert_eq!(res.success, true);
    let res = internal_result.pairs.get(&(node2, node3)).unwrap();
    assert_eq!(res.amount, 5);
    assert_eq!(res.success, true);
}

#[test]
fn test_history_internal_result_fail_range_pair() {
    let mut internal_result = InternalResult::default();
    let node1 = generate_pubkey();
    let node2 = generate_pubkey();
    let node3 = generate_pubkey();

    let route = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: OutPoint::default(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: OutPoint::default(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    internal_result.fail_range_pairs(&route, 0, 2);
    assert_eq!(internal_result.pairs.len(), 4);
    let res = internal_result.pairs.get(&(node1, node2)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    let res = internal_result.pairs.get(&(node2, node1)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    let res = internal_result.pairs.get(&(node2, node3)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    let res = internal_result.pairs.get(&(node3, node2)).unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);

    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    history.apply_internal_result(internal_result);

    assert!(matches!(
        history.get_result(&node1, &node2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&node2, &node1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&node2, &node3),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&node3, &node2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));
}

#[test]
fn test_history_apply_internal_result_fail_node() {
    let mut internal_result = InternalResult::default();
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let node1 = generate_pubkey();
    let node2 = generate_pubkey();
    let node3 = generate_pubkey();
    let route = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: OutPoint::default(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: OutPoint::default(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    internal_result.fail_node(&route, 1);
    assert_eq!(internal_result.pairs.len(), 4);

    history.apply_pair_result(node1, node2, 10, true, 1);
    history.apply_pair_result(node2, node3, 11, true, 2);
    assert!(matches!(
        history.get_result(&node1, &node2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 10,
            success_time: 1,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&node2, &node3),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 11,
            success_time: 2,
            ..
        })
    ));

    history.apply_internal_result(internal_result);
    assert!(matches!(
        history.get_result(&node1, &node2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 1,
            ..
        })
    ));
    assert!(matches!(
        history.get_result(&node2, &node1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&node2, &node3),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 2,
            ..
        })
    ));
    assert!(matches!(
        history.get_result(&node3, &node2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));
}

#[test]
fn test_history_interal_success_fail() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
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
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.eval_probability(from, target.clone(), 10, 100);
    assert_eq!(prob, 1.0);

    let now = now_timestamp_as_millis_u64();
    let result = TimedResult {
        success_time: now,
        success_amount: 5,
        fail_time: now,
        fail_amount: 10,
    };
    history.add_result(from, target, result);

    assert_eq!(history.eval_probability(from, target.clone(), 1, 10), 1.0);
    assert_eq!(history.eval_probability(from, target.clone(), 1, 8), 1.0);

    // graph of amount is less than history's success_amount and fail_amount
    assert_eq!(history.eval_probability(from, target.clone(), 1, 4), 1.0);

    let p1 = history
        .eval_probability(from, target.clone(), 5, 9)
        .round_to_2();
    assert!(p1 <= 1.0);

    let p2 = history
        .eval_probability(from, target.clone(), 6, 9)
        .round_to_2();
    assert!(p2 <= 0.75);
    assert!(p2 < p1);

    let p3 = history
        .eval_probability(from, target.clone(), 7, 9)
        .round_to_2();
    assert!(p3 <= 0.50 && p3 < p2);

    let p4 = history
        .eval_probability(from, target.clone(), 8, 9)
        .round_to_2();
    assert!(p4 <= 0.25 && p4 < p3);

    let p1 = history
        .eval_probability(from, target.clone(), 5, 10)
        .round_to_2();
    assert!(p1 <= 1.0);

    let p2 = history
        .eval_probability(from, target.clone(), 6, 10)
        .round_to_2();
    assert!(p2 <= 0.80 && p2 < p1);

    let p3 = history
        .eval_probability(from, target.clone(), 7, 10)
        .round_to_2();
    assert!(p3 <= 0.60 && p3 < p2);

    let p4 = history
        .eval_probability(from, target.clone(), 8, 10)
        .round_to_2();
    assert!(p4 <= 0.40 && p4 < p3);

    let p5 = history
        .eval_probability(from, target.clone(), 9, 10)
        .round_to_2();
    assert!(p5 <= 0.20 && p5 < p4);

    assert_eq!(
        history
            .eval_probability(from, target.clone(), 10, 10)
            .round_to_2(),
        0.0
    );
}

#[test]
fn test_history_direct_probability() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.get_direct_probability(&from, &target);
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 0,
        fail_amount: 0,
    };
    history.add_result(from, target, result);
    assert_eq!(history.get_direct_probability(&from, &target), 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 10,
        fail_amount: 10,
    };
    history.add_result(from, target, result);
    let prob = history.get_direct_probability(&from, &target);
    assert_eq!(prob, 0.0);
}

#[test]
fn test_history_small_fail_amount_probability() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.eval_probability(from, target.clone(), 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 50000000,
        fail_time: now_timestamp_as_millis_u64(),
        fail_amount: 10,
    };
    history.add_result(from, target, result);
    assert_eq!(
        history.eval_probability(from, target.clone(), 50000000, 100000000),
        0.0
    );
}

#[test]
fn test_history_channel_probability_range() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.eval_probability(from, target.clone(), 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let now = now_timestamp_as_millis_u64();
    let result = TimedResult {
        success_time: now,
        success_amount: 10000000,
        fail_time: now,
        fail_amount: 50000000,
    };

    history.add_result(from, target, result);

    for amount in (1..10000000).step_by(100000) {
        let prob = history.eval_probability(from, target.clone(), amount, 100000000);
        assert_eq!(prob, 1.0);
    }

    let mut prev_prob = history.eval_probability(from, target.clone(), 10000000, 100000000);
    for amount in (10000005..50000000).step_by(10000) {
        let prob = history.eval_probability(from, target.clone(), amount, 100000000);
        assert!(prob < prev_prob);
        prev_prob = prob;
    }

    for amount in (50000001..100000000).step_by(100000) {
        let prob = history.eval_probability(from, target.clone(), amount, 100000000);
        assert!(prob < 0.0001);
    }
}

#[test]
fn test_history_eval_probability_range() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let target = generate_pubkey();
    let from: Pubkey = generate_pubkey().into();

    let prob = history.eval_probability(from, target.clone(), 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let now = now_timestamp_as_millis_u64();
    let result = TimedResult {
        success_time: now,
        success_amount: 10000000,
        fail_time: now,
        fail_amount: 50000000,
    };

    history.add_result(from, target, result);
    let prob1 = history.eval_probability(from, target.clone(), 50000000, 100000000);
    assert!(0.0 <= prob1 && prob1 < 0.001);
    let prob2 = history.eval_probability(from, target.clone(), 50000000 - 10, 100000000);
    assert!(0.0 < prob2 && prob2 < 0.001);
    assert!(prob2 > prob1);

    let mut prev_prob = prob2;
    for _i in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let prob = history.eval_probability(from, target.clone(), 50000000 - 10, 100000000);
        assert!(prob > prev_prob);
        prev_prob = prob;
    }

    history.reset();
    let now = now_timestamp_as_millis_u64();
    let result = TimedResult {
        success_time: now,
        success_amount: 10000000,
        fail_time: now,
        fail_amount: 50000000,
    };
    history.add_result(from, target, result);
    prev_prob = 0.0;
    for gap in (10..10000000).step_by(100000) {
        let prob = history.eval_probability(from, target, 50000000 - gap, 100000000);
        assert!(prob > prev_prob);
        prev_prob = prob;
    }

    prev_prob = 0.0;
    let now = now_timestamp_as_millis_u64();
    for time in (60 * 1000..DEFAULT_BIMODAL_DECAY_TIME * 2).step_by(1 * 60 * 60 * 1000) {
        history.reset();
        let result = TimedResult {
            success_time: now,
            success_amount: 10000000,
            fail_time: now - time,
            fail_amount: 50000000,
        };
        history.add_result(from, target, result);
        let prob = history.eval_probability(from, target, 50000000 - 10, 100000000);
        assert!(prob > prev_prob);
        prev_prob = prob;
    }
    assert!(prev_prob > 0.0 && prev_prob < 0.55);
}

#[test]
fn test_history_load_store() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test_history_load_store");
    let store = Store::new(path).expect("created store failed");
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

#[test]
fn test_history_can_send_with_time() {
    use crate::fiber::history::DEFAULT_BIMODAL_DECAY_TIME;

    let history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let now = now_timestamp_as_millis_u64();
    let res = history.can_send(100, now);
    assert_eq!(res, 100);

    let before = now - DEFAULT_BIMODAL_DECAY_TIME / 3;
    let res = history.can_send(100, before);
    assert_eq!(res, 71);

    let before = now - DEFAULT_BIMODAL_DECAY_TIME;
    let res = history.can_send(100, before);
    assert_eq!(res, 36);

    let before = now - DEFAULT_BIMODAL_DECAY_TIME * 3;
    let res = history.can_send(100, before);
    assert_eq!(res, 4);
}

#[test]
fn test_history_can_not_send_with_time() {
    use crate::fiber::history::DEFAULT_BIMODAL_DECAY_TIME;

    let history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let now = now_timestamp_as_millis_u64();
    let res = history.cannot_send(90, now, 100);
    assert_eq!(res, 90);

    let before = now - DEFAULT_BIMODAL_DECAY_TIME / 3;
    let res = history.cannot_send(90, before, 100);
    assert_eq!(res, 93);

    let before = now - DEFAULT_BIMODAL_DECAY_TIME;
    let res = history.cannot_send(90, before, 100);
    assert_eq!(res, 97);

    let before = now - DEFAULT_BIMODAL_DECAY_TIME * 3;
    let res = history.cannot_send(90, before, 100);
    assert_eq!(res, 100);
}
