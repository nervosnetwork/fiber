use crate::fiber::types::Pubkey;
use std::collections::HashMap;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct PairResult {
    pub(crate) fail_time: u128,
    pub(crate) fail_amount: u128,
    pub(crate) success_time: u128,
    pub(crate) success_amount: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentResult {
    pub pairs: HashMap<Pubkey, PairResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentHistory {
    pub inner: HashMap<Pubkey, PaymentResult>,
}

impl PaymentHistory {
    pub fn new() -> Self {
        PaymentHistory {
            inner: HashMap::new(),
        }
    }

    pub fn add_result(&mut self, source: Pubkey, target: Pubkey, result: PairResult) {
        let payment_result = self.inner.entry(source).or_insert(PaymentResult {
            pairs: HashMap::new(),
        });
        payment_result.pairs.insert(target, result);
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
