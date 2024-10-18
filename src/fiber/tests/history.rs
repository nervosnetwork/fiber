use crate::fiber::history::{ChannelTimedResult, PaymentHistory};
use crate::fiber::tests::test_utils::generate_pubkey;
use ckb_types::packed::{OutPoint, OutPointBuilder};
use ckb_types::prelude::Builder;
use ckb_types::prelude::Pack;

#[test]
fn test_history() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None);
    let outpoint = OutPoint::default();

    let result1 = ChannelTimedResult {
        fail_time: 1,
        fail_amount: 2,
        success_time: 3,
        success_amount: 4,
    };
    history.add_result(&outpoint, result1);
    assert_eq!(history.get_result(&outpoint), Some(&result1));

    let outpoint2 = OutPointBuilder::default().tx_hash([1u8; 32].pack()).build();
    let result2 = ChannelTimedResult {
        fail_time: 5,
        fail_amount: 6,
        success_time: 7,
        success_amount: 8,
    };

    history.add_result(&outpoint2, result2);
    assert_eq!(history.get_result(&outpoint2), Some(&result2));
}

#[test]
fn test_history_apply_channel_result() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None);
    let outpoint = OutPoint::default();

    history.apply_channel_result(&outpoint, 10, false, 11);
    assert_eq!(
        history.get_result(&outpoint),
        Some(&ChannelTimedResult {
            fail_time: 11,
            fail_amount: 10,
            success_time: 0,
            success_amount: 0,
        })
    );

    let outpoint2 = OutPointBuilder::default().tx_hash([1u8; 32].pack()).build();
    history.apply_channel_result(&outpoint2, 10, true, 12);
    assert_eq!(
        history.get_result(&outpoint2),
        Some(&ChannelTimedResult {
            fail_time: 0,
            fail_amount: 0,
            success_time: 12,
            success_amount: 10,
        })
    );
}

#[test]
fn test_history_interal_success_fail() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None);
    let outpoint = OutPoint::default();

    let result = ChannelTimedResult {
        fail_time: 1,
        fail_amount: 2,
        success_time: 3,
        success_amount: 4,
    };
    history.add_result(&outpoint, result);

    history.apply_channel_result(&outpoint, 10, true, 11);
    assert_eq!(
        history.get_result(&outpoint),
        Some(&ChannelTimedResult {
            fail_time: 1,
            fail_amount: 11, // amount + 1
            success_time: 11,
            success_amount: 10,
        })
    );

    // time is too short
    history.apply_channel_result(&outpoint, 12, false, 13);
    assert_eq!(
        history.get_result(&outpoint),
        Some(&ChannelTimedResult {
            fail_time: 1,
            fail_amount: 11,
            success_time: 11,
            success_amount: 10,
        })
    );

    history.apply_channel_result(&outpoint, 12, false, 61 * 1000);
    assert_eq!(
        history.get_result(&outpoint),
        Some(&ChannelTimedResult {
            fail_time: 61 * 1000,
            fail_amount: 12,
            success_time: 11,   // will not update
            success_amount: 10, // will not update
        })
    );

    history.apply_channel_result(&outpoint, 9, false, 61 * 1000 * 2);
    assert_eq!(
        history.get_result(&outpoint),
        Some(&ChannelTimedResult {
            fail_time: 61 * 1000 * 2,
            fail_amount: 9,
            success_time: 11,
            success_amount: 8, // amount - 1
        })
    );
}

trait Round {
    fn to_2_decimal(self) -> f64;
}

impl Round for f64 {
    fn to_2_decimal(self) -> f64 {
        (self * 100.0).round() / 100.0
    }
}

#[test]
fn test_history_probability() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None);
    let outpoint = OutPoint::default();

    let prob = history.get_channel_probability(outpoint.clone(), 10, 100);
    assert_eq!(prob, 1.0);

    let result = ChannelTimedResult {
        success_time: 3,
        success_amount: 5,
        fail_time: 10,
        fail_amount: 10,
    };
    history.add_result(&outpoint, result);
    assert_eq!(
        history.get_channel_probability(outpoint.clone(), 1, 10),
        1.0
    );
    assert_eq!(history.get_channel_probability(outpoint.clone(), 1, 8), 1.0);

    // graph of amount is less than history's success_amount and fail_amount
    assert_eq!(history.get_channel_probability(outpoint.clone(), 1, 4), 1.0);

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 5, 9)
            .to_2_decimal(),
        1.0
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 6, 9)
            .to_2_decimal(),
        0.75
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 7, 9)
            .to_2_decimal(),
        0.50
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 8, 9)
            .to_2_decimal(),
        0.25
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 5, 10)
            .to_2_decimal(),
        1.0
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 6, 10)
            .to_2_decimal(),
        0.80
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 7, 10)
            .to_2_decimal(),
        0.60
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 8, 10)
            .to_2_decimal(),
        0.40
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 9, 10)
            .to_2_decimal(),
        0.20
    );

    assert_eq!(
        history
            .get_channel_probability(outpoint.clone(), 10, 10)
            .to_2_decimal(),
        0.0
    );
}

#[test]
fn test_history_probability_small_fail_amount() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None);
    let outpoint = OutPoint::default();

    let prob = history.get_channel_probability(outpoint.clone(), 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let result = ChannelTimedResult {
        success_time: 3,
        success_amount: 50000000,
        fail_time: 10,
        fail_amount: 10,
    };
    history.add_result(&outpoint, result);
    assert_eq!(
        history.get_channel_probability(outpoint.clone(), 50000000, 100000000),
        0.0
    );
}

#[test]
fn test_history_probability_range() {
    let mut history = PaymentHistory::new(generate_pubkey().into(), None);
    let outpoint = OutPoint::default();

    let prob = history.get_channel_probability(outpoint.clone(), 50000000, 100000000);
    assert_eq!(prob, 1.0);

    let result = ChannelTimedResult {
        success_time: 3,
        success_amount: 10000000,
        fail_time: 10,
        fail_amount: 50000000,
    };
    history.add_result(&outpoint, result);

    for amount in (1..10000000).step_by(100000) {
        let prob = history.get_channel_probability(outpoint.clone(), amount, 100000000);
        assert_eq!(prob, 1.0);
    }

    let mut prev_prob = history.get_channel_probability(outpoint.clone(), 10000000, 100000000);
    for amount in (10000005..50000000).step_by(10000) {
        let prob = history.get_channel_probability(outpoint.clone(), amount, 100000000);
        eprintln!(
            "amount: {}, prob: {}, prev_prob: {}",
            amount, prob, prev_prob
        );
        assert!(prob < prev_prob);
        prev_prob = prob;
    }

    for amount in (50000001..100000000).step_by(100000) {
        let prob = history.get_channel_probability(outpoint.clone(), amount, 100000000);
        assert_eq!(prob, 0.0);
    }
}
