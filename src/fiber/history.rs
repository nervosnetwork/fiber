use crate::fiber::types::Pubkey;
use std::collections::HashMap;
use tracing::debug;

use super::graph::SessionRouteNode;

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct InternalResult {
    pub pairs: HashMap<(Pubkey, Pubkey), InternalPairResult>,
}

impl InternalResult {
    pub fn add(&mut self, from: Pubkey, target: Pubkey, time: u128, amount: u128, success: bool) {
        let pair = InternalPairResult {
            success,
            time,
            amount,
        };
        self.pairs.insert((from, target), pair);
    }

    pub fn add_fail_pair(&mut self, from: Pubkey, target: Pubkey) {
        self.add(
            from,
            target,
            std::time::UNIX_EPOCH.elapsed().unwrap().as_millis(),
            0,
            false,
        );
        self.add(
            target,
            from,
            std::time::UNIX_EPOCH.elapsed().unwrap().as_millis(),
            0,
            false,
        )
    }

    pub fn fail_node(&mut self, route: &Vec<SessionRouteNode>, index: usize) {
        if index > 0 {
            self.add_fail_pair(route[index - 1].pubkey, route[index].pubkey);
        }
        if index + 1 < route.len() {
            self.add_fail_pair(route[index].pubkey, route[index + 1].pubkey);
        }
    }

    pub fn fail_pair(&mut self, route: &Vec<SessionRouteNode>, index: usize) {
        if index > 0 {
            self.add_fail_pair(route[index - 1].pubkey, route[index].pubkey);
        }
    }

    pub fn succeed_range_pairs(&mut self, route: &Vec<SessionRouteNode>, start: usize, end: usize) {
        for i in start..=end {
            if i > 0 {
                self.add(
                    route[i - 1].pubkey,
                    route[i].pubkey,
                    std::time::UNIX_EPOCH.elapsed().unwrap().as_millis(),
                    route[i].amount,
                    true,
                );
            }
        }
    }
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
    pub fn set_node_fail(&mut self, node: Pubkey, time: u128) {
        for (from, result) in self.inner.iter_mut() {
            for (target, pairs) in result.pairs.iter_mut() {
                if *from == node || *target == node {
                    *pairs = PairResult {
                        fail_time: time,
                        fail_amount: 0,
                        success_time: 0,
                        success_amount: 0,
                    };
                }
            }
        }
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
