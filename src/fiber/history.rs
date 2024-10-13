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
    pub latest_results: HashMap<Pubkey, PaymentResult>,
}

impl PaymentHistory {
    pub fn new() -> Self {
        PaymentHistory {
            latest_results: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    pub fn add_result(&mut self, source: Pubkey, target: Pubkey, result: PairResult) {
        let payment_result = self.latest_results.entry(source).or_insert(PaymentResult {
            pairs: HashMap::new(),
        });
        payment_result.pairs.insert(target, result);
    }

    #[allow(dead_code)]
    pub fn get_result(&self, pubkey: &Pubkey) -> Option<&PaymentResult> {
        self.latest_results.get(pubkey)
    }
}
