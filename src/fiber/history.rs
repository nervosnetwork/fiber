// The probability calculation is based on lnd's implementation,
// https://github.com/lightningnetwork/lnd/blob/b7c59b36a74975c4e710a02ea42959053735402e/routing/probability_bimodal.go
// we only use direct channel probability now.

use super::{
    graph::{NetworkGraphStateStore, SessionRouteNode},
    types::{Pubkey, TlcErr},
};
use crate::{fiber::types::TlcErrorCode, now_timestamp_as_millis_u64};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, error};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedResult {
    pub fail_time: u64,
    pub fail_amount: u128,
    pub success_time: u64,
    pub success_amount: u128,
}

const DEFAULT_MIN_FAIL_RELAX_INTERVAL: u64 = 60 * 1000;

// FIXME: this is a magic number from lnd, it's used to scale the amount to calculate the probability
// lnd use 300_000_000 mili satoshis, we use shannons as the unit in fiber
// we need to find a better way to set this value for UDT
const DEFAULT_BIMODAL_SCALE_SHANNONS: f64 = 800_000_000.0;
pub(crate) const DEFAULT_BIMODAL_DECAY_TIME: u64 = 6 * 60 * 60 * 1000; // 6 hours
pub(crate) type PairTimedResult = HashMap<Pubkey, TimedResult>;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct InternalPairResult {
    pub(crate) success: bool,
    pub(crate) time: u64,
    pub(crate) amount: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct InternalResult {
    pub pairs: HashMap<(Pubkey, Pubkey), InternalPairResult>,
    pub fail_node: Option<Pubkey>,
}

impl InternalResult {
    pub fn add(&mut self, from: Pubkey, target: Pubkey, time: u64, amount: u128, success: bool) {
        let pair = InternalPairResult {
            success,
            time,
            amount,
        };
        self.pairs.insert((from, target), pair);
    }

    pub fn add_fail_pair(&mut self, from: Pubkey, target: Pubkey) {
        self.add(from, target, now_timestamp_as_millis_u64(), 0, false);
        self.add(target, from, now_timestamp_as_millis_u64(), 0, false)
    }

    pub fn add_fail_pair_balanced(&mut self, from: Pubkey, target: Pubkey, amount: u128) {
        self.add(from, target, now_timestamp_as_millis_u64(), amount, false);
    }

    pub fn fail_node(&mut self, nodes: &[SessionRouteNode], index: usize) {
        self.fail_node = Some(nodes[index].pubkey);
        if index > 0 {
            self.fail_pair(nodes, index);
        }
        if index + 1 < nodes.len() {
            self.fail_pair(nodes, index + 1);
        }
    }

    pub fn fail_pair(&mut self, route: &[SessionRouteNode], index: usize) {
        if index > 0 {
            let a = route[index - 1].pubkey;
            let b = route[index].pubkey;
            self.add_fail_pair(a, b);
        }
    }

    pub fn fail_pair_balanced(&mut self, nodes: &[SessionRouteNode], index: usize) {
        if index > 0 {
            let a = nodes[index - 1].pubkey;
            let b = nodes[index].pubkey;
            let amount = nodes[index].amount;
            self.add_fail_pair_balanced(a, b, amount);
        }
    }

    pub fn succeed_range_pairs(&mut self, nodes: &[SessionRouteNode], start: usize, end: usize) {
        for i in start..end {
            self.add(
                nodes[i].pubkey,
                nodes[i + 1].pubkey,
                now_timestamp_as_millis_u64(),
                nodes[i].amount,
                true,
            );
        }
    }
    pub fn fail_range_pairs(&mut self, nodes: &[SessionRouteNode], start: usize, end: usize) {
        for index in start.max(1)..=end {
            self.fail_pair(nodes, index);
        }
    }

    pub fn record_payment_fail(&mut self, nodes: &[SessionRouteNode], tlc_err: TlcErr) -> bool {
        let mut need_to_retry = true;

        let error_index = nodes.iter().position(|s| {
            Some(s.channel_outpoint.clone()) == tlc_err.error_channel_outpoint()
                || Some(s.pubkey) == tlc_err.error_node_id()
        });

        let Some(index) = error_index else {
            error!("Error index not found in the route: {:?}", tlc_err);
            return need_to_retry;
        };

        let len = nodes.len();
        assert!(len >= 2);
        let error_code = tlc_err.error_code;
        if index == 0 {
            // we get error from the source node
            match error_code {
                TlcErrorCode::InvalidOnionVersion
                | TlcErrorCode::InvalidOnionHmac
                | TlcErrorCode::InvalidOnionKey
                | TlcErrorCode::InvalidOnionPayload => need_to_retry = false,
                TlcErrorCode::IncorrectOrUnknownPaymentDetails | TlcErrorCode::InvoiceExpired => {
                    need_to_retry = false;
                }
                _ => {
                    // we can not penalize our own node, the whole payment session need to retry
                }
            }
        } else if index == 1 {
            match error_code {
                // we received an error from the first node, we trust our own node
                // so we need to penalize the first node
                TlcErrorCode::InvalidOnionVersion
                | TlcErrorCode::InvalidOnionHmac
                | TlcErrorCode::InvalidOnionKey
                | TlcErrorCode::InvalidOnionPayload => {
                    self.fail_node(nodes, 1);
                }
                _ => {
                    // we can not penalize our own node, the whole payment session need to retry
                    debug!("first hop failed with error: {:?}", tlc_err);
                }
            }
        } else if index == len - 1 {
            match error_code {
                TlcErrorCode::FinalIncorrectExpiryDelta | TlcErrorCode::FinalIncorrectTlcAmount => {
                    if len == 2 {
                        need_to_retry = false;
                        self.fail_node(nodes, len - 1);
                    } else {
                        self.fail_pair(nodes, index - 1);
                        self.succeed_range_pairs(nodes, 0, index - 2);
                    }
                }
                TlcErrorCode::IncorrectOrUnknownPaymentDetails | TlcErrorCode::InvoiceExpired => {
                    need_to_retry = false;
                    self.succeed_range_pairs(nodes, 0, len - 1);
                }
                TlcErrorCode::ExpiryTooSoon => {
                    need_to_retry = false;
                }
                TlcErrorCode::MppTimeout | TlcErrorCode::InvalidOnionBlinding => {
                    unimplemented!("not implemented");
                }
                _ => {
                    self.fail_node(nodes, len - 1);
                    if len > 1 {
                        self.succeed_range_pairs(nodes, 0, len - 2);
                    }
                }
            }
        } else {
            match error_code {
                TlcErrorCode::InvalidOnionVersion
                | TlcErrorCode::InvalidOnionHmac
                | TlcErrorCode::InvalidOnionKey => {
                    self.fail_pair(nodes, index);
                }
                TlcErrorCode::InvalidOnionPayload => {
                    self.fail_node(nodes, index);
                    if index > 1 {
                        self.succeed_range_pairs(nodes, 0, index - 1);
                    }
                }
                TlcErrorCode::UnknownNextPeer => {
                    self.fail_pair(nodes, index);
                }
                TlcErrorCode::PermanentChannelFailure => {
                    self.fail_pair(nodes, index);
                }
                TlcErrorCode::FeeInsufficient | TlcErrorCode::IncorrectTlcExpiry => {
                    need_to_retry = false;
                    if index == 1 {
                        self.fail_node(nodes, 1);
                    } else {
                        self.fail_pair(nodes, index - 1);
                        if index > 1 {
                            self.succeed_range_pairs(nodes, 0, index - 2);
                        }
                    }
                }
                TlcErrorCode::TemporaryChannelFailure => {
                    self.fail_pair_balanced(nodes, index);
                    self.succeed_range_pairs(nodes, 0, index - 1);
                }
                TlcErrorCode::ExpiryTooSoon => {
                    if index == 1 {
                        self.fail_node(nodes, 1);
                    } else {
                        self.fail_range_pairs(nodes, 0, index - 1);
                    }
                }
                _ => {
                    self.fail_node(nodes, index);
                }
            }
        }
        need_to_retry
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PaymentHistory<S> {
    pub inner: HashMap<Pubkey, PairTimedResult>,
    // The minimum interval between two failed payments in milliseconds
    pub min_fail_relax_interval: u64,
    pub bimodal_scale_msat: f64,
    // this filed is used to check whether from is the source Node
    // will be used after enabling the direct channel related logic
    #[allow(dead_code)]
    pub source: Pubkey,
    store: S,
}

impl<S> PaymentHistory<S>
where
    S: NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    pub(crate) fn new(source: Pubkey, min_fail_relax_interval: Option<u64>, store: S) -> Self {
        let mut s = PaymentHistory {
            source,
            inner: HashMap::new(),
            min_fail_relax_interval: min_fail_relax_interval
                .unwrap_or(DEFAULT_MIN_FAIL_RELAX_INTERVAL),
            bimodal_scale_msat: DEFAULT_BIMODAL_SCALE_SHANNONS,
            store,
        };
        s.load_from_store();
        s
    }

    #[cfg(test)]
    pub(crate) fn reset(&mut self) {
        self.inner.clear();
    }

    pub(crate) fn add_result(&mut self, from: Pubkey, target: Pubkey, result: TimedResult) {
        self.inner.entry(from).or_default().insert(target, result);
        self.save_result(from, target, result);
    }

    fn save_result(&mut self, from: Pubkey, target: Pubkey, result: TimedResult) {
        self.store
            .insert_payment_history_result(from, target, result);
    }

    pub(crate) fn load_from_store(&mut self) {
        let results = self.store.get_payment_history_results();
        for (from, target, result) in results.into_iter() {
            self.inner.entry(from).or_default().insert(target, result);
        }
    }

    pub(crate) fn apply_pair_result(
        &mut self,
        from: Pubkey,
        target: Pubkey,
        amount: u128,
        success: bool,
        time: u64,
    ) {
        let min_fail_relax_interval = self.min_fail_relax_interval;
        let result = if let Some(current) = self.get_mut_result(&from, &target) {
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
                    && time.saturating_sub(current.fail_time) < min_fail_relax_interval
                {
                    return;
                }
                current.fail_amount = amount;
                current.fail_time = time;
                if amount <= current.success_amount {
                    current.success_amount = amount.saturating_sub(1);
                }
            }
            assert!(current.fail_time == 0 || current.success_amount <= current.fail_amount);
            *current
        } else {
            TimedResult {
                fail_time: if success { 0 } else { time },
                fail_amount: if success { 0 } else { amount },
                success_time: if success { time } else { 0 },
                success_amount: if success { amount } else { 0 },
            }
        };
        self.add_result(from, target, result);
    }

    pub(crate) fn apply_internal_result(&mut self, result: InternalResult) {
        let InternalResult { pairs, fail_node } = result;
        for ((from, target), pair_result) in pairs.into_iter() {
            self.apply_pair_result(
                from,
                target,
                pair_result.amount,
                pair_result.success,
                pair_result.time,
            );
        }
        if let Some(fail_node) = fail_node {
            let pairs: Vec<(Pubkey, Pubkey)> = self
                .inner
                .iter()
                .flat_map(|(from, targets)| {
                    targets.keys().filter_map(move |target| {
                        (*from == fail_node || *target == fail_node)
                            .then_some((from.clone(), target.clone()))
                    })
                })
                .collect();
            for (from, target) in pairs {
                self.apply_pair_result(from, target, 0, false, now_timestamp_as_millis_u64());
            }
        }
    }

    pub(crate) fn get_result(&self, from: &Pubkey, target: &Pubkey) -> Option<&TimedResult> {
        self.inner.get(from).and_then(|h| h.get(target))
    }

    pub(crate) fn get_mut_result(
        &mut self,
        from: &Pubkey,
        target: &Pubkey,
    ) -> Option<&mut TimedResult> {
        self.inner.get_mut(from).and_then(|h| h.get_mut(target))
    }

    pub(crate) fn eval_probability(
        &self,
        from: Pubkey,
        target: Pubkey,
        amount: u128,
        capacity: u128,
    ) -> f64 {
        let mut success_amount = 0;
        let mut fail_amount = capacity;
        if let Some(result) = self.get_result(&from, &target) {
            if result.fail_time != 0 {
                fail_amount = self.cannot_send(result.fail_amount, result.fail_time, capacity);
            }
            if result.success_time != 0 {
                success_amount = self.can_send(result.success_amount, result.success_time);
            }
        } else {
            // if we don't have the history, we assume the probability is 1.0
            return 1.0;
        }
        let ret = self.get_channel_probability(capacity, success_amount, fail_amount, amount);
        assert!(ret >= 0.0 && ret <= 1.0);
        ret
    }

    // The factor approaches 0 for success_time a long time in the past,
    // is 1 when the success_time is now.
    fn time_factor(&self, time: u64) -> f64 {
        let time_ago = (now_timestamp_as_millis_u64() - time).max(0);
        let exponent = -(time_ago as f64) / (DEFAULT_BIMODAL_DECAY_TIME as f64);
        let factor = exponent.exp();
        factor
    }

    pub(crate) fn cannot_send(&self, fail_amount: u128, time: u64, capacity: u128) -> u128 {
        let mut fail_amount = fail_amount;

        if fail_amount > capacity {
            fail_amount = capacity;
        }

        let factor = self.time_factor(time);
        let cannot_send = capacity - (factor * (capacity - fail_amount) as f64) as u128;
        cannot_send
    }

    pub(crate) fn can_send(&self, amount: u128, time: u64) -> u128 {
        let factor = self.time_factor(time);
        let can_send = (amount as f64 * factor) as u128;
        can_send
    }

    // Get the probability of a payment success through a direct channel,
    // suppose we know the accurate balance for direct channels, so we don't need to use `get_channel_probability`
    // for the direct channel, this function is used disable the direct channel for a time preiod if it's failed
    // currently we may mark the channel failed on graph level, so this function is not used now.
    // FIXME: reconsider this after we already got the accurate balance of direct channels
    //        related issue: https://github.com/nervosnetwork/fiber/issues/257
    #[allow(dead_code)]
    pub(crate) fn get_direct_probability(&self, from: &Pubkey, target: &Pubkey) -> f64 {
        let mut prob = 1.0;
        if let Some(result) = self.get_result(&from, &target) {
            if result.fail_time != 0 {
                let time_ago = (now_timestamp_as_millis_u64() - result.fail_time).min(0);
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
        capacity: u128,
        success_amount: u128,
        fail_amount: u128,
        amount: u128,
    ) -> f64 {
        if amount > capacity || amount == 0 || capacity == 0 {
            return 0.0;
        }

        let fail_amount = fail_amount.min(capacity);
        let success_amount = success_amount.min(capacity);

        if fail_amount == success_amount {
            // if the graph has latest information
            // we don't continue to calculate the probability
            if amount < capacity {
                return 1.0;
            }
            return 0.0;
        }

        if fail_amount < success_amount {
            // suppose a malioucious node report wrong information
            // here we return 0.0 to avoid to choose this channel
            error!(
                "fail_amount: {} < success_amount: {}",
                fail_amount, success_amount
            );
            return 0.0;
        }

        if amount >= fail_amount {
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
        let re_norm =
            self.integral_probability(capacity as f64, success_amount as f64, fail_amount as f64);
        if re_norm == 0.0 {
            return 0.0;
        }
        prob /= re_norm;
        prob = prob.max(0.0).min(1.0);
        return prob;
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
