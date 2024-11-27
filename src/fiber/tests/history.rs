use crate::fiber::graph::SessionRouteNode;
use crate::fiber::history::output_direction;
use crate::fiber::history::{Direction, DEFAULT_BIMODAL_DECAY_TIME};
use crate::fiber::history::{InternalPairResult, InternalResult};
use crate::fiber::history::{PaymentHistory, TimedResult};
use crate::fiber::tests::test_utils::{generate_pubkey, generate_store};
use crate::now_timestamp_as_millis_u64;
use crate::store::Store;
use ckb_types::packed::OutPoint;
use molecule::prelude::Entity;
use tempfile::tempdir;

trait Round {
    fn round_to_2(self) -> f64;
}

impl Round for f64 {
    fn round_to_2(self) -> f64 {
        (self * 100.0).round() / 100.0
    }
}

fn gen_rand_outpoint() -> OutPoint {
    let rand_slice = (0..36).map(|_| rand::random::<u8>()).collect::<Vec<u8>>();
    OutPoint::from_slice(&rand_slice).unwrap()
}

#[test]
fn test_history() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let channel_outpoint = OutPoint::default();
    let direction = Direction::Forward;

    let result1 = TimedResult {
        fail_time: 1,
        fail_amount: 2,
        success_time: 3,
        success_amount: 4,
    };
    history.add_result(channel_outpoint.clone(), direction, result1);
    assert_eq!(
        history.get_result(&channel_outpoint.clone(), direction),
        Some(&result1)
    );

    let channel_outpoint2 = gen_rand_outpoint();
    let result2 = TimedResult {
        fail_time: 5,
        fail_amount: 6,
        success_time: 7,
        success_amount: 8,
    };

    history.add_result(channel_outpoint2.clone(), Direction::Backward, result2);
    assert_eq!(
        history.get_result(&channel_outpoint2, Direction::Backward),
        Some(&result2)
    );
    assert_eq!(
        history.get_result(&channel_outpoint2, Direction::Forward),
        None,
    );
}

#[test]
fn test_history_apply_channel_result() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let channel_outpoint = OutPoint::default();
    let direction = Direction::Forward;

    history.apply_pair_result(channel_outpoint.clone(), direction, 10, false, 11);
    assert_eq!(
        history.get_result(&channel_outpoint, direction),
        Some(&TimedResult {
            fail_time: 11,
            fail_amount: 10,
            success_time: 0,
            success_amount: 0,
        })
    );

    let channel_outpoint2 = gen_rand_outpoint();
    let direction_2 = Direction::Backward;
    history.apply_pair_result(channel_outpoint2.clone(), direction_2, 10, true, 12);
    assert_eq!(
        history.get_result(&channel_outpoint2, direction_2),
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
    let mut internal_result = InternalResult::default();
    let from = generate_pubkey();
    let target = generate_pubkey();
    let channel_outpoint = gen_rand_outpoint();
    let (direction, rev_direction) = output_direction(from, target);
    internal_result.add(from, target, channel_outpoint.clone(), 10, 11, true);
    assert_eq!(internal_result.pairs.len(), 1);
    assert_eq!(
        internal_result
            .pairs
            .get(&(channel_outpoint.clone(), direction))
            .unwrap(),
        &InternalPairResult {
            amount: 11,
            success: true,
            time: 10
        }
    );

    assert_eq!(
        internal_result
            .pairs
            .get(&(channel_outpoint.clone(), rev_direction)),
        None,
    );

    assert_eq!(internal_result.pairs.len(), 1);
    internal_result.add_fail_pair(from, target, channel_outpoint.clone());
    assert_eq!(internal_result.pairs.len(), 2);

    let res = internal_result
        .pairs
        .get(&(channel_outpoint.clone(), direction))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    assert_ne!(res.time, 0);

    let res = internal_result
        .pairs
        .get(&(channel_outpoint.clone(), rev_direction))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    assert_ne!(res.time, 0);

    internal_result.add_fail_pair_balanced(from, target, channel_outpoint.clone(), 100);
    assert_eq!(internal_result.pairs.len(), 2);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint, direction))
        .unwrap();
    assert_eq!(res.amount, 100);
    assert_eq!(res.success, false);
}

#[test]
fn test_history_internal_result_fail_pair() {
    let mut internal_result = InternalResult::default();
    let from = generate_pubkey();
    let target = generate_pubkey();

    let channel_outpoint = gen_rand_outpoint();
    let route = vec![
        SessionRouteNode {
            pubkey: from,
            amount: 10,
            channel_outpoint: channel_outpoint.clone(),
        },
        SessionRouteNode {
            pubkey: target,
            amount: 5,
            channel_outpoint: gen_rand_outpoint(),
        },
    ];

    internal_result.fail_pair(&route, 0);
    assert_eq!(internal_result.pairs.len(), 0);

    internal_result.fail_pair(&route, 1);
    assert_eq!(internal_result.pairs.len(), 2);
    let (direction, rev_direction) = output_direction(from, target);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint.clone(), direction))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);

    let res = internal_result
        .pairs
        .get(&(channel_outpoint, rev_direction))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
}

#[test]
fn test_history_internal_result_success_range_pair() {
    let mut internal_result = InternalResult::default();
    let node1 = generate_pubkey();
    let node2 = generate_pubkey();
    let node3 = generate_pubkey();

    let channel_outpoint1 = gen_rand_outpoint();
    let channel_outpoint2 = gen_rand_outpoint();
    let (direction1, _) = output_direction(node1, node2);
    let (direction2, _) = output_direction(node2, node3);
    let route = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: channel_outpoint1.clone(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: channel_outpoint2.clone(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    internal_result.succeed_range_pairs(&route, 0, 2);
    assert_eq!(internal_result.pairs.len(), 2);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint1, direction1))
        .unwrap();
    assert_eq!(res.amount, 10);
    assert_eq!(res.success, true);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint2, direction2))
        .unwrap();
    assert_eq!(res.amount, 5);
    assert_eq!(res.success, true);
}

#[test]
fn test_history_internal_result_fail_range_pair() {
    let mut internal_result = InternalResult::default();
    let node1 = generate_pubkey();
    let node2 = generate_pubkey();
    let node3 = generate_pubkey();
    let channel_outpoint1 = gen_rand_outpoint();
    let channel_outpoint2 = gen_rand_outpoint();

    let route = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: channel_outpoint1.clone(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: channel_outpoint2.clone(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    internal_result.fail_range_pairs(&route, 0, 2);
    assert_eq!(internal_result.pairs.len(), 4);

    let (direction1, rev_direction1) = output_direction(node1, node2);
    let (direction2, rev_direction2) = output_direction(node2, node3);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint1.clone(), direction1))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint1.clone(), rev_direction1))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint2.clone(), direction2))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);
    let res = internal_result
        .pairs
        .get(&(channel_outpoint2.clone(), rev_direction2))
        .unwrap();
    assert_eq!(res.amount, 0);
    assert_eq!(res.success, false);

    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    history.apply_internal_result(internal_result);

    assert!(matches!(
        history.get_result(&channel_outpoint1, direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint1, rev_direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint2, direction2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint2, rev_direction2),
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
    let channel_outpoint1 = gen_rand_outpoint();
    let channel_outpoint2 = gen_rand_outpoint();

    let route = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: channel_outpoint1.clone(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: channel_outpoint2.clone(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    internal_result.fail_node(&route, 1);
    assert_eq!(internal_result.pairs.len(), 4);

    let (direction1, rev_direction1) = output_direction(node1, node2);
    let (direction2, rev_direction2) = output_direction(node2, node3);

    history.apply_pair_result(channel_outpoint1.clone(), direction1, 10, true, 1);
    history.apply_pair_result(channel_outpoint2.clone(), direction2, 11, true, 2);
    assert!(matches!(
        history.get_result(&channel_outpoint1, direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 10,
            success_time: 1,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint2, direction2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 11,
            success_time: 2,
            ..
        })
    ));

    history.apply_internal_result(internal_result);
    assert!(matches!(
        history.get_result(&channel_outpoint1, direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 1,
            ..
        })
    ));
    assert!(matches!(
        history.get_result(&channel_outpoint1, rev_direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint2, direction2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 2,
            ..
        })
    ));
    assert!(matches!(
        history.get_result(&channel_outpoint2, rev_direction2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            success_time: 0,
            ..
        })
    ));
}

#[test]
fn test_history_fail_node_with_multiple_channels() {
    let mut internal_result = InternalResult::default();
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let node1 = generate_pubkey();
    let node2 = generate_pubkey();
    let node3 = generate_pubkey();
    let channel_outpoint1 = gen_rand_outpoint();
    let channel_outpoint2 = gen_rand_outpoint();
    let channel_outpoint3 = gen_rand_outpoint();
    let channel_outpoint4 = gen_rand_outpoint();

    let route1 = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: channel_outpoint1.clone(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: channel_outpoint2.clone(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    let route2 = vec![
        SessionRouteNode {
            pubkey: node1,
            amount: 10,
            channel_outpoint: channel_outpoint3.clone(),
        },
        SessionRouteNode {
            pubkey: node2,
            amount: 5,
            channel_outpoint: channel_outpoint4.clone(),
        },
        SessionRouteNode {
            pubkey: node3,
            amount: 3,
            channel_outpoint: OutPoint::default(),
        },
    ];

    let (direction1, rev_direction1) = output_direction(node1, node2);
    let (direction2, rev_direction2) = output_direction(node2, node3);

    internal_result.succeed_range_pairs(&route1, 0, 2);
    history.apply_internal_result(internal_result.clone());

    assert!(matches!(
        history.get_result(&channel_outpoint1, direction1),
        Some(&TimedResult {
            fail_amount: 0,
            fail_time: 0,
            success_amount: 10,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint2, direction2),
        Some(&TimedResult {
            fail_amount: 0,
            fail_time: 0,
            success_amount: 5,
            ..
        })
    ));

    internal_result.fail_node(&route2, 1);
    assert_eq!(internal_result.pairs.len(), 6);
    history.apply_internal_result(internal_result);

    assert!(matches!(
        history.get_result(&channel_outpoint1, direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint2, direction2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint1, rev_direction1),
        None,
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint2, rev_direction2),
        None,
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint3, direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint4, direction2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint3, rev_direction1),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            ..
        })
    ));

    assert!(matches!(
        history.get_result(&channel_outpoint4, rev_direction2),
        Some(&TimedResult {
            fail_amount: 0,
            success_amount: 0,
            ..
        })
    ));
}

#[test]
fn test_history_interal_success_fail() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let from = generate_pubkey();
    let target = generate_pubkey();
    let channel_outpoint = OutPoint::default();
    let (direction, _) = output_direction(from, target);

    let result = TimedResult {
        fail_time: 1,
        fail_amount: 2,
        success_time: 3,
        success_amount: 4,
    };

    history.add_result(channel_outpoint.clone(), direction, result);

    history.apply_pair_result(channel_outpoint.clone(), direction, 10, true, 11);
    assert_eq!(
        history.get_result(&channel_outpoint, direction),
        Some(&TimedResult {
            fail_time: 1,
            fail_amount: 11, // amount + 1
            success_time: 11,
            success_amount: 10,
        })
    );

    // time is too short
    history.apply_pair_result(channel_outpoint.clone(), direction, 12, false, 13);
    assert_eq!(
        history.get_result(&channel_outpoint, direction),
        Some(&TimedResult {
            fail_time: 1,
            fail_amount: 11,
            success_time: 11,
            success_amount: 10,
        })
    );

    history.apply_pair_result(channel_outpoint.clone(), direction, 12, false, 61 * 1000);
    assert_eq!(
        history.get_result(&channel_outpoint, direction),
        Some(&TimedResult {
            fail_time: 61 * 1000,
            fail_amount: 12,
            success_time: 11,   // will not update
            success_amount: 10, // will not update
        })
    );

    history.apply_pair_result(channel_outpoint.clone(), direction, 9, false, 61 * 1000 * 2);
    assert_eq!(
        history.get_result(&channel_outpoint, direction),
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
    let from = generate_pubkey();
    let target = generate_pubkey();
    let channel_outpoint = OutPoint::default();
    let (direction, _) = output_direction(from, target);

    let prob = history.eval_probability(from, target, &channel_outpoint, 10, 100);
    assert_eq!(prob, 1.0);

    let now = now_timestamp_as_millis_u64();
    let result = TimedResult {
        success_time: now,
        success_amount: 5,
        fail_time: now,
        fail_amount: 10,
    };
    history.add_result(channel_outpoint.clone(), direction, result);

    assert_eq!(
        history.eval_probability(from, target, &channel_outpoint, 1, 10),
        1.0
    );
    assert_eq!(
        history.eval_probability(from, target, &channel_outpoint, 1, 8),
        1.0
    );

    // graph of amount is less than history's success_amount and fail_amount
    assert_eq!(
        history.eval_probability(from, target, &channel_outpoint, 1, 4),
        1.0
    );

    let p1 = history
        .eval_probability(from, target, &channel_outpoint, 5, 9)
        .round_to_2();
    assert!(p1 <= 1.0);

    let p2 = history
        .eval_probability(from, target, &channel_outpoint, 6, 9)
        .round_to_2();
    assert!(p2 <= 0.75);
    assert!(p2 < p1);

    let p3 = history
        .eval_probability(from, target, &channel_outpoint, 7, 9)
        .round_to_2();
    assert!(p3 <= 0.50 && p3 < p2);

    let p4 = history
        .eval_probability(from, target, &channel_outpoint, 8, 9)
        .round_to_2();
    assert!(p4 <= 0.25 && p4 < p3);

    let p1 = history
        .eval_probability(from, target, &channel_outpoint, 5, 10)
        .round_to_2();
    assert!(p1 <= 1.0);

    let p2 = history
        .eval_probability(from, target, &channel_outpoint, 6, 10)
        .round_to_2();
    assert!(p2 <= 0.80 && p2 < p1);

    let p3 = history
        .eval_probability(from, target, &channel_outpoint, 7, 10)
        .round_to_2();
    assert!(p3 <= 0.60 && p3 < p2);

    let p4 = history
        .eval_probability(from, target, &channel_outpoint, 8, 10)
        .round_to_2();
    assert!(p4 <= 0.40 && p4 < p3);

    let p5 = history
        .eval_probability(from, target, &channel_outpoint, 9, 10)
        .round_to_2();
    assert!(p5 <= 0.20 && p5 < p4);

    assert_eq!(
        history
            .eval_probability(from, target, &channel_outpoint, 10, 10)
            .round_to_2(),
        0.0
    );
}

#[test]
fn test_history_direct_probability() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let from = generate_pubkey();
    let target = generate_pubkey();
    let channel_outpoint = OutPoint::default();
    let (direction, _) = output_direction(from, target);

    let prob = history.get_direct_probability(&channel_outpoint, direction);
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 0,
        fail_amount: 0,
    };
    history.add_result(channel_outpoint.clone(), direction, result);
    assert_eq!(
        history.get_direct_probability(&channel_outpoint, direction),
        1.0
    );

    let result = TimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 10,
        fail_amount: 10,
    };
    history.add_result(channel_outpoint.clone(), direction, result);
    let prob = history.get_direct_probability(&channel_outpoint, direction);
    assert_eq!(prob, 0.0);
}

#[test]
fn test_history_small_fail_amount_probability() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let from = generate_pubkey();
    let target = generate_pubkey();
    let channel_outpoint = OutPoint::default();
    let (direction, _) = output_direction(from, target);

    let prob = history.eval_probability(from, target, &channel_outpoint, 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let result = TimedResult {
        success_time: 3,
        success_amount: 50000000,
        fail_time: now_timestamp_as_millis_u64(),
        fail_amount: 10,
    };
    history.add_result(channel_outpoint.clone(), direction, result);
    assert_eq!(
        history.eval_probability(from, target, &channel_outpoint, 50000000, 100000000),
        0.0
    );
}

#[test]
fn test_history_channel_probability_range() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let from = generate_pubkey();
    let target = generate_pubkey();
    let channel_outpoint = OutPoint::default();
    let (direction, _) = output_direction(from, target);

    let prob = history.eval_probability(from, target, &channel_outpoint, 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let now = now_timestamp_as_millis_u64();
    let result = TimedResult {
        success_time: now,
        success_amount: 10000000,
        fail_time: now,
        fail_amount: 50000000,
    };

    history.add_result(channel_outpoint.clone(), direction, result);

    for amount in (1..10000000).step_by(100000) {
        let prob = history.eval_probability(from, target, &channel_outpoint, amount, 100000000);
        assert_eq!(prob, 1.0);
    }

    let mut prev_prob =
        history.eval_probability(from, target, &channel_outpoint, 10000000, 100000000);
    for amount in (10000005..50000000).step_by(10000) {
        let prob = history.eval_probability(from, target, &channel_outpoint, amount, 100000000);
        assert!(prob < prev_prob);
        prev_prob = prob;
    }

    for amount in (50000001..100000000).step_by(100000) {
        let prob = history.eval_probability(from, target, &channel_outpoint, amount, 100000000);
        assert!(prob < 0.0001);
    }
}

#[test]
fn test_history_eval_probability_range() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None, generate_store());
    let from = generate_pubkey();
    let target = generate_pubkey();
    let channel_outpoint = OutPoint::default();
    let (direction, _) = output_direction(from, target);

    let prob = history.eval_probability(from, target, &channel_outpoint, 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let now = now_timestamp_as_millis_u64();
    let result = TimedResult {
        success_time: now,
        success_amount: 10000000,
        fail_time: now,
        fail_amount: 50000000,
    };

    history.add_result(channel_outpoint.clone(), direction, result);
    let prob1 = history.eval_probability(from, target, &channel_outpoint, 50000000, 100000000);
    assert!(0.0 <= prob1 && prob1 < 0.001);
    let prob2 = history.eval_probability(from, target, &channel_outpoint, 50000000 - 10, 100000000);
    assert!(0.0 < prob2 && prob2 < 0.001);
    assert!(prob2 > prob1);

    let mut prev_prob = prob2;
    for _i in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let prob =
            history.eval_probability(from, target, &channel_outpoint, 50000000 - 10, 100000000);
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
    history.add_result(channel_outpoint.clone(), direction, result);
    prev_prob = 0.0;
    for gap in (10..10000000).step_by(100000) {
        let prob =
            history.eval_probability(from, target, &channel_outpoint, 50000000 - gap, 100000000);
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
        history.add_result(channel_outpoint.clone(), direction, result);
        let prob =
            history.eval_probability(from, target, &channel_outpoint, 50000000 - 10, 100000000);
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
    let channel_outpoint = OutPoint::default();
    let (direction, _) = output_direction(from, target);

    let result = TimedResult {
        success_time: 3,
        success_amount: 10000000,
        fail_time: 10,
        fail_amount: 50000000,
    };

    history.add_result(channel_outpoint.clone(), direction, result);
    let result = history
        .get_result(&channel_outpoint, direction)
        .unwrap()
        .clone();
    history.reset();
    assert_eq!(history.get_result(&channel_outpoint, direction), None);
    history.load_from_store();
    assert_eq!(
        history.get_result(&channel_outpoint, direction),
        Some(&result)
    );

    history.apply_pair_result(channel_outpoint.clone(), direction, 1, false, 11);
    history.reset();
    history.load_from_store();
    assert_eq!(
        history.get_result(&channel_outpoint, direction),
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
