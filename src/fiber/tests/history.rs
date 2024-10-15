use super::test_utils::generate_pubkey;
use crate::fiber::history::InternalPairResult;
use crate::fiber::history::InternalResult;
use crate::fiber::history::PaymentResult;
use crate::fiber::history::{PairResult, PaymentHistory};
use crate::fiber::types::Pubkey;

#[test]
fn test_history() {
    let mut history = PaymentHistory::new(None);
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

#[test]
fn test_history_interal_success_fail() {
    let mut history = PaymentHistory::new(None);
    let source: Pubkey = generate_pubkey().into();
    let target1: Pubkey = generate_pubkey().into();

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: true,
                amount: 10,
                time: 11,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_time: 0,
                    fail_amount: 0,
                    success_time: 11,
                    success_amount: 10,
                }
            ),]
            .into_iter()
            .collect()
        })
    );

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: false,
                amount: 10,
                time: 12,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_time: 12,
                    fail_amount: 10,
                    success_time: 11,
                    success_amount: 9, // success_amount - 1
                }
            ),]
            .into_iter()
            .collect()
        })
    );

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: true,
                amount: 11,
                time: 13,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_time: 12,
                    fail_amount: 12, // success_amount + 1
                    success_time: 13,
                    success_amount: 11
                }
            ),]
            .into_iter()
            .collect()
        })
    );
}

#[test]
fn test_history_internal_result() {
    let mut history = PaymentHistory::new(Some(2));
    let source: Pubkey = generate_pubkey().into();
    let target1: Pubkey = generate_pubkey().into();

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: true,
                amount: 10,
                time: 11,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_time: 0,
                    fail_amount: 0,
                    success_time: 11,
                    success_amount: 10,
                }
            ),]
            .into_iter()
            .collect()
        })
    );

    let target2: Pubkey = generate_pubkey().into();
    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target2.clone()),
            InternalPairResult {
                success: false,
                amount: 1,
                time: 13,
            },
        )]
        .into_iter()
        .collect(),
    };

    history.apply_internal_result(internal_result);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![
                (
                    target1,
                    PairResult {
                        fail_time: 0,
                        fail_amount: 0,
                        success_time: 11,
                        success_amount: 10,
                    }
                ),
                (
                    target2,
                    PairResult {
                        fail_time: 13,
                        fail_amount: 1,
                        success_time: 0,
                        success_amount: 0,
                    }
                )
            ]
            .into_iter()
            .collect()
        })
    );
}

#[test]
fn test_history_internal_fail_interval() {
    let mut history = PaymentHistory::new(Some(2));
    let source: Pubkey = generate_pubkey().into();
    let target1: Pubkey = generate_pubkey().into();

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: false,
                amount: 1,
                time: 13,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_time: 13,
                    fail_amount: 1,
                    success_time: 0,
                    success_amount: 0,
                }
            ),]
            .into_iter()
            .collect()
        })
    );

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: false,
                amount: 2,
                time: 14,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);

    // does not apply since the interval is less than min_fail_relax_interval
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_time: 13,
                    fail_amount: 1,
                    success_time: 0,
                    success_amount: 0,
                }
            ),]
            .into_iter()
            .collect()
        })
    );

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: false,
                amount: 0,
                time: 14,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);
    // will apply since amount is less
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_time: 14,
                    fail_amount: 0,
                    success_time: 0,
                    success_amount: 0,
                }
            ),]
            .into_iter()
            .collect()
        })
    );

    let internal_result = InternalResult {
        pairs: vec![(
            (source.clone(), target1.clone()),
            InternalPairResult {
                success: false,
                amount: 10,
                time: 20,
            },
        )]
        .into_iter()
        .collect(),
    };
    history.apply_internal_result(internal_result);
    // will apply since interval is large enough
    assert_eq!(
        history.get_result(&source),
        Some(&PaymentResult {
            pairs: vec![(
                target1,
                PairResult {
                    fail_amount: 10,
                    fail_time: 20,
                    success_time: 0,
                    success_amount: 0,
                }
            ),]
            .into_iter()
            .collect()
        })
    );
}
