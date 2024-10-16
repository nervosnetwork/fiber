use crate::fiber::types::Pubkey;
use ckb_types::packed::OutPoint;
use std::collections::HashMap;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub(crate) struct ChannelTimedResult {
    pub(crate) fail_time: u128,
    pub(crate) fail_amount: u128,
    pub(crate) success_time: u128,
    pub(crate) success_amount: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentHistory {
    pub inner: HashMap<OutPoint, ChannelTimedResult>,
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

    pub fn add_result(&mut self, outpoint: &OutPoint, result: ChannelTimedResult) {
        self.inner.insert(outpoint.clone(), result);
    }

    pub fn apply_channel_result(
        &mut self,
        outpoint: &OutPoint,
        amount: u128,
        success: bool,
        time: u128,
    ) {
        if let Some(current) = self.inner.get_mut(outpoint) {
            if success {
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
            assert!(current.fail_time == 0 || current.success_amount <= current.fail_amount);
        } else {
            let result = ChannelTimedResult {
                fail_time: if success { 0 } else { time },
                fail_amount: if success { 0 } else { amount },
                success_time: if success { time } else { 0 },
                success_amount: if success { amount } else { 0 },
            };
            self.add_result(outpoint, result);
        }
    }

    #[allow(dead_code)]
    pub fn get_result(&self, outpoint: &OutPoint) -> Option<&ChannelTimedResult> {
        self.inner.get(outpoint)
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
