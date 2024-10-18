use ckb_types::packed::OutPoint;
use std::collections::HashMap;
use tracing::{debug, error};

use super::types::Pubkey;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub(crate) struct ChannelTimedResult {
    pub(crate) fail_time: u128,
    pub(crate) fail_amount: u128,
    pub(crate) success_time: u128,
    pub(crate) success_amount: u128,
}

const DEFAULT_MIN_FAIL_RELAX_INTERVAL: u128 = 60 * 1000;

// FIXME: this is a magic number from lnd, it's used to scale the amount to calculate the probability
// lnd use 300_000_000 mili satoshis, we use shannons as the unit in fiber
// we need to find a better way to set this value for UDT
const DEFAULT_BIMODAL_SCALE_SHANNONS: f64 = 800_000_000.0;

const DEFAULT_BIMODAL_DECAY_TIME: u64 = 6 * 60 * 60 * 1000; // 6 hours

#[derive(Debug, Clone)]
pub(crate) struct PaymentHistory {
    pub inner: HashMap<OutPoint, ChannelTimedResult>,
    // The minimum interval between two failed payments in milliseconds
    pub min_fail_relax_interval: u128,
    pub bimodal_scale_msat: f64,
    pub source: Pubkey,
}

impl PaymentHistory {
    pub(crate) fn new(source: Pubkey, min_fail_relax_interval: Option<u128>) -> Self {
        PaymentHistory {
            source,
            inner: HashMap::new(),
            min_fail_relax_interval: min_fail_relax_interval
                .unwrap_or(DEFAULT_MIN_FAIL_RELAX_INTERVAL),
            bimodal_scale_msat: DEFAULT_BIMODAL_SCALE_SHANNONS,
        }
    }

    pub(crate) fn add_result(&mut self, outpoint: &OutPoint, result: ChannelTimedResult) {
        self.inner.insert(outpoint.clone(), result);
    }

    pub(crate) fn apply_channel_result(
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

    #[cfg(test)]
    pub(crate) fn get_result(&self, outpoint: &OutPoint) -> Option<&ChannelTimedResult> {
        self.inner.get(outpoint)
    }

    pub(crate) fn eval_probability(
        &self,
        from: Pubkey,
        channel: OutPoint,
        amount: u128,
        capacity: u128,
    ) -> f64 {
        if from == self.source {
            self.get_direct_probability(channel)
        } else {
            self.get_channel_probability(channel, amount, capacity)
        }
    }

    fn get_direct_probability(&self, channel: OutPoint) -> f64 {
        let mut prob = 1.0;
        if let Some(result) = self.inner.get(&channel) {
            if result.fail_time != 0 {
                let time_ago = (std::time::UNIX_EPOCH.elapsed().unwrap().as_millis()
                    - result.fail_time)
                    .min(0);
                let exponent = -(time_ago as f64) / (DEFAULT_BIMODAL_DECAY_TIME as f64);
                prob -= exponent.exp();
            }
        }
        prob
    }

    // Get the probability of a payment success through a channel
    // The probability is calculated based on the history of the channel
    // Suppose the range of amount:
    // ---------------------- success_amount ++++++++++++++++++++++++ fail_amount xxxxxxxxxxxxxxxxxxxxxx
    //        must be succeed       |        may be succeed           |       must be failed
    // The probability is calculated as:
    // 1. If the amount is less than or equal to success_amount, return 1.0
    // 2. If the amount is greater than fail_amount, return 0.0
    // 3. Otherwise, calculate the probability based on the time and capacity of the channel
    pub(crate) fn get_channel_probability(
        &self,
        channel: OutPoint,
        amount: u128,
        capacity: u128,
    ) -> f64 {
        if amount > capacity || amount == 0 {
            return 0.0;
        }
        if let Some(result) = self.inner.get(&channel) {
            // the payment history may have outdated information
            // we need to adjust the amount to the valid range
            let fail_amount = if result.fail_time == 0 {
                capacity
            } else {
                result.fail_amount.min(capacity)
            };
            let success_amount = if result.success_time == 0 {
                0
            } else {
                result.success_amount.min(capacity)
            };

            if fail_amount == success_amount {
                // if the graph has latest information
                // we don't continue to calculate the probability
                if amount <= capacity {
                    return 1.0;
                }
                return 0.0;
            } else if fail_amount < success_amount {
                // suppose a malioucious node report wrong information
                // here we return 0.0 to avoid to choose this channel
                error!(
                    "fail_amount: {} < success_amount: {}",
                    fail_amount, success_amount
                );
                return 0.0;
            }

            if amount > fail_amount {
                return 0.0;
            }

            // safely convert amount, success_amount, fail_amount to f64
            let amount = amount as f64;
            let success_amount = success_amount as f64;
            let fail_amount = fail_amount as f64;

            // f128 is only on nightly, so we use f64 here, we may lose some precision
            // but it's acceptable since all the values are cast to f64
            let mut prob =
                self.integral_probability(capacity as f64, amount as f64, fail_amount as f64);
            if prob.is_nan() {
                error!(
                    "probability is NaN: capacity: {} amount: {} fail_amount: {}",
                    capacity, amount, fail_amount
                );
                return 0.0;
            }
            let re_norm = self.integral_probability(
                capacity as f64,
                success_amount as f64,
                fail_amount as f64,
            );
            if re_norm == 0.0 {
                return 0.0;
            }
            prob /= re_norm;
            prob = prob.max(0.0).min(1.0);
            return prob;
        }
        // if we don't have the history, we assume the probability is 1.0
        1.0
    }

    fn primitive(&self, c: f64, x: f64) -> f64 {
        let s = self.bimodal_scale_msat;

        // The indefinite integral of P(x) is given by
        // Int P(x) dx = H(x) = s * (-e(-x/s) + e((x-c)/s)),
        // and its norm from 0 to c can be computed from it,
        // norm = [H(x)]_0^c = s * (-e(-c/s) + 1 -(1 + e(-c/s))).
        let ecs = (-c / s).exp();
        let exs = (-x / s).exp();

        // It would be possible to split the next term and reuse the factors
        // from before, but this can lead to numerical issues with large
        // numbers.
        let excs = ((x - c) / s).exp();

        // norm can only become zero, if c is zero, which we sorted out before
        // calling this method.
        let norm = -2.0 * ecs + 2.0;

        // We end up with the primitive function of the normalized P(x).
        (-exs + excs) / norm
    }

    fn integral_probability(&self, capacity: f64, lower: f64, upper: f64) -> f64 {
        if lower < 0.0 || lower > upper {
            debug!(
                "probability integral limits nonsensical: capacity: {} lower: {} upper: {}",
                capacity, lower, upper
            );
            return 0.0;
        }

        self.primitive(capacity, upper) - self.primitive(capacity, lower)
    }
}
