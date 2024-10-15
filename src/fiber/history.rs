use crate::fiber::types::Pubkey;
use std::collections::HashMap;
use tracing::debug;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub(crate) struct PairResult {
    pub(crate) fail_time: u128,
    pub(crate) fail_amount: u128,
    pub(crate) success_time: u128,
    pub(crate) success_amount: u128,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct InternalPairResult {
    pub(crate) success: bool,
    pub(crate) time: u128,
    pub(crate) amount: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InternalResult {
    pub pairs: HashMap<(Pubkey, Pubkey), InternalPairResult>,
}

impl From<InternalPairResult> for PairResult {
    fn from(result: InternalPairResult) -> Self {
        let pair_result = PairResult::default();
        if result.success {
            PairResult {
                success_time: result.time,
                success_amount: result.amount,
                ..pair_result
            }
        } else {
            PairResult {
                fail_time: result.time,
                fail_amount: result.amount,
                ..pair_result
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentResult {
    pub pairs: HashMap<Pubkey, PairResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentHistory {
    pub inner: HashMap<Pubkey, PaymentResult>,
    // The minimum interval between two failed payments in milliseconds
    pub min_fail_relax_interval: u128,
}

impl PaymentHistory {
    pub fn new(min_fail_relax_interval: Option<u128>) -> Self {
        PaymentHistory {
            inner: HashMap::new(),
            min_fail_relax_interval: min_fail_relax_interval.unwrap_or(60 * 1000),
        }
    }

    pub fn add_result(&mut self, source: Pubkey, target: Pubkey, result: PairResult) {
        let payment_result = self.inner.entry(source).or_insert(PaymentResult {
            pairs: HashMap::new(),
        });
        payment_result.pairs.insert(target, result);
    }

    #[allow(dead_code)]
    pub fn apply_internal_result(&mut self, result: InternalResult) {
        for ((source, target), internal_pair) in result.pairs.iter() {
            debug!(
                "apply_result source: {:?}, target: {:?}, internal_pair_result: {:?}",
                source, target, internal_pair
            );
            if let Some(payment_result) = self.inner.get_mut(source) {
                let current = payment_result
                    .pairs
                    .entry(*target)
                    .or_insert(PairResult::default());
                let amount = internal_pair.amount;
                let time = internal_pair.time;
                if internal_pair.success {
                    current.success_time = time;
                    if amount > current.success_amount {
                        current.success_amount = amount;
                    }
                    if current.fail_time != 0 {
                        current.fail_amount = amount + 1;
                    }
                } else {
                    if amount > current.fail_amount
                        && current.fail_time != 0
                        && time.saturating_sub(current.fail_time) < self.min_fail_relax_interval
                    {
                        return;
                    }
                    current.fail_amount = amount;
                    current.fail_time = time;
                    if amount == 0 {
                        current.success_amount = 0;
                    } else if amount <= current.success_amount {
                        current.success_amount = amount - 1;
                    }
                }
            } else {
                let pair_result: PairResult = (*internal_pair).into();
                self.add_result(*source, *target, pair_result);
            }
        }
    }

    #[allow(dead_code)]
    pub fn get_result(&self, pubkey: &Pubkey) -> Option<&PaymentResult> {
        self.inner.get(pubkey)
    }

    #[allow(dead_code)]
    pub fn get_probability(
        &self,
        _from: &Pubkey,
        _to: &Pubkey,
        _amount: u128,
        _capacity: u128,
    ) -> f64 {
        return 0.0;
    }
}
