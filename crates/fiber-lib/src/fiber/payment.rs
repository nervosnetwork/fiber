use super::network::{SendPaymentData, SendPaymentResponse};
use super::types::Hash256;
use super::types::Pubkey;
use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::network::DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT;
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::U128Hex;
use crate::fiber::types::PaymentHopData;
use crate::now_timestamp_as_millis_u64;
use ckb_types::packed::OutPoint;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum MppMode {
    BasicMpp,
    AtomicMpp,
}

/// The status of a payment, will update as the payment progresses.
/// The transfer path for payment status is `Created -> Inflight -> Success | Failed`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum PaymentStatus {
    /// initial status, a payment session is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// related HTLC is successfully settled
    Success,
    /// related HTLC is failed
    Failed,
}

impl PaymentStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, PaymentStatus::Success | PaymentStatus::Failed)
    }
}

/// The node and channel information in a payment route hop
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    /// the public key of the node
    pub pubkey: Pubkey,
    /// the amount for this hop
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// the channel outpoint for this hop
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

/// The router is a list of nodes that the payment will go through.
/// We store in the payment session and then will use it to track the payment history.
/// The router is a list of nodes that the payment will go through.
/// For example:
///    `A(amount, channel) -> B -> C -> D`
/// means A will send `amount` with `channel` to B.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    /// the nodes in the route
    pub nodes: Vec<SessionRouteNode>,
}

impl SessionRoute {
    // Create a new route from the source to the target with the given payment hops.
    // The payment hops are the hops that the payment will go through.
    // for a payment route A -> B -> C -> D
    // the `payment_hops` is [B, C, D], which is a convenient way for onion routing.
    // here we need to create a session route with source, which is A -> B -> C -> D
    pub fn new(source: Pubkey, target: Pubkey, payment_hops: &[PaymentHopData]) -> Self {
        //dbg!(payment_hops);
        let nodes = std::iter::once(source)
            .chain(
                payment_hops
                    .iter()
                    .map(|hop| hop.next_hop.unwrap_or(target)),
            )
            .zip(payment_hops)
            .map(|(pubkey, hop)| SessionRouteNode {
                pubkey,
                channel_outpoint: OutPoint::new(
                    if hop.funding_tx_hash != Hash256::default() {
                        hop.funding_tx_hash.into()
                    } else {
                        Hash256::default().into()
                    },
                    0,
                ),
                amount: hop.amount,
            })
            .collect();
        Self { nodes }
    }

    pub fn receiver_amount(&self) -> u128 {
        self.nodes.last().map_or(0, |s| s.amount)
    }

    pub fn fee(&self) -> u128 {
        let first_amount = self.nodes.first().map_or(0, |s| s.amount);
        let last_amount = self.receiver_amount();
        debug_assert!(first_amount >= last_amount);
        first_amount - last_amount
    }

    pub(crate) fn channel_outpoints(&self) -> impl Iterator<Item = (Pubkey, &OutPoint, u128)> {
        self.nodes
            .iter()
            .map(|x| (x.pubkey, &x.channel_outpoint, x.amount))
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentSession {
    pub request: SendPaymentData,
    pub last_error: Option<String>,
    // For non-MPP, this is the maximum number of single attempt retry limit
    // For MPP, this is the sum limit of all parts' retry times.
    pub try_limit: u32,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
    #[serde(skip)]
    pub cached_attempts: Vec<Attempt>, // Add a cache for attempts
}

impl PaymentSession {
    pub fn new(
        store: &impl NetworkGraphStateStore,
        request: SendPaymentData,
        try_limit: u32,
    ) -> Self {
        let now = now_timestamp_as_millis_u64();
        Self {
            request,
            last_error: None,
            try_limit,
            status: PaymentStatus::Created,
            created_at: now,
            last_updated_at: now,
            cached_attempts: vec![],
        }
        .init_attempts(store)
    }

    pub fn init_attempts(mut self, store: &impl NetworkGraphStateStore) -> Self {
        self.flush_attempts(store);
        self
    }

    pub fn flush_attempts(&mut self, store: &impl NetworkGraphStateStore) {
        self.cached_attempts = store.get_attempts(self.request.payment_hash);
        self.status = self.calc_payment_status();
    }

    pub fn update_with_attempt(&mut self, attempt: Attempt) {
        if let Some(a) = self.cached_attempts.iter_mut().find(|a| a.id == attempt.id) {
            *a = attempt;
        }
        self.status = self.calc_payment_status();
    }

    pub fn retry_times(&self) -> u32 {
        self.attempts().map(|a| a.tried_times).sum()
    }

    pub fn allow_mpp(&self) -> bool {
        self.request.allow_mpp()
    }

    pub fn is_atomic_mpp(&self) -> bool {
        self.request.is_atomic_mpp()
    }

    pub fn payment_hash(&self) -> Hash256 {
        self.request.payment_hash
    }

    pub fn is_payment_with_router(&self) -> bool {
        !self.request.router.is_empty()
    }

    pub fn is_dry_run(&self) -> bool {
        self.request.dry_run
    }

    pub fn attempts(&self) -> impl Iterator<Item = &Attempt> {
        self.cached_attempts.iter()
    }

    #[cfg(test)]
    pub fn all_attempts_with_status(&self) -> Vec<(u64, AttemptStatus, Option<String>, u32, u128)> {
        self.cached_attempts
            .iter()
            .map(|a| {
                (
                    a.id,
                    a.status,
                    a.last_error.clone(),
                    a.tried_times,
                    a.route.receiver_amount(),
                )
            })
            .collect()
    }

    pub fn attempts_count(&self) -> usize {
        self.cached_attempts.len()
    }

    pub fn max_parts(&self) -> usize {
        if self.allow_mpp() {
            self.request.max_parts()
        } else {
            1
        }
    }

    pub fn active_attempts(&self) -> impl Iterator<Item = &Attempt> {
        self.attempts().filter(|a| a.is_active())
    }

    pub fn fee_paid(&self) -> u128 {
        self.active_attempts().map(|a| a.route.fee()).sum()
    }

    fn success_attempts_amount_is_enough(&self) -> bool {
        let success_amount: u128 = self
            .attempts()
            .filter_map(|a| {
                if a.is_success() {
                    Some(a.route.receiver_amount())
                } else {
                    None
                }
            })
            .sum();
        success_amount >= self.request.amount
    }

    pub fn remain_fee_amount(&self) -> Option<u128> {
        let max_fee_amount = self.request.max_fee_amount?;
        let remain_fee = max_fee_amount.saturating_sub(self.fee_paid());
        Some(remain_fee)
    }

    pub fn remain_amount(&self) -> u128 {
        let sent_amount = self
            .active_attempts()
            .map(|a| a.route.receiver_amount())
            .sum::<u128>();
        self.request.amount.saturating_sub(sent_amount)
    }

    pub fn new_attempt(&self, attempt_id: u64, route: SessionRoute) -> Attempt {
        let now = now_timestamp_as_millis_u64();
        let payment_hash = self.payment_hash();
        // For HTLC, the attempt hash is the payment hash
        let hash = payment_hash;
        let try_limit = if self.allow_mpp() {
            DEFAULT_PAYMENT_MPP_ATTEMPT_TRY_LIMIT
        } else {
            self.try_limit
        };

        Attempt {
            id: attempt_id,
            hash,
            try_limit,
            tried_times: 1,
            payment_hash,
            route,
            session_key: [0; 32],
            preimage: None,
            created_at: now,
            last_updated_at: now,
            last_error: None,
            status: AttemptStatus::Created,
        }
    }

    pub fn append_attempt(&mut self, attempt: Attempt) {
        self.cached_attempts.push(attempt);
    }

    pub fn allow_more_attempts(&self) -> bool {
        if self.status.is_final() {
            return false;
        }

        if self.remain_amount() == 0 {
            // no remaining amount, imply no need to retry
            return false;
        }

        if self.retry_times() >= self.try_limit {
            // already reached the retry limit, no more attempts allowed
            return false;
        }

        if self.active_attempts().count() >= self.max_parts() {
            return false;
        }

        // otherwise, should continue retry
        true
    }

    pub fn calc_payment_status(&self) -> PaymentStatus {
        if self.cached_attempts.is_empty() || self.status.is_final() {
            return self.status;
        }

        if self.attempts().any(|a| a.is_inflight()) {
            // if any attempt is created or inflight, the payment is inflight
            return PaymentStatus::Inflight;
        }

        if self.attempts().all(|a| a.is_failed()) && !self.allow_more_attempts() {
            return PaymentStatus::Failed;
        }

        if self.success_attempts_amount_is_enough() {
            return PaymentStatus::Success;
        }

        return PaymentStatus::Created;
    }

    fn set_status(&mut self, status: PaymentStatus) {
        self.status = status;
        self.last_updated_at = now_timestamp_as_millis_u64();
    }

    pub fn set_inflight_status(&mut self) {
        self.set_status(PaymentStatus::Inflight);
    }

    pub fn set_success_status(&mut self) {
        self.set_status(PaymentStatus::Success);
        self.last_error = None;
    }

    pub fn set_failed_status(&mut self, error: &str) {
        self.set_status(PaymentStatus::Failed);
        self.last_error = Some(error.to_string());
    }
}

impl From<PaymentSession> for SendPaymentResponse {
    fn from(session: PaymentSession) -> Self {
        let status = session.status;
        let fee = session.fee_paid();
        let mut all_attempts = session
            .attempts()
            .map(|a| {
                (
                    a.id,
                    a.status,
                    a.last_error.clone(),
                    a.tried_times,
                    a.route.receiver_amount(),
                )
            })
            .collect::<Vec<_>>();
        all_attempts.sort_by_key(|a| a.0);

        #[cfg(debug_assertions)]
        {
            dbg!(&all_attempts);
            dbg!(
                fee,
                &status,
                all_attempts.len(),
                session.try_limit,
                &session.last_error
            );

            let active_count = session.active_attempts().count();
            let active_amount = session
                .active_attempts()
                .map(|a| a.route.receiver_amount())
                .sum::<u128>();
            dbg!(active_amount, session.request.amount, active_count);
        }

        #[cfg(any(debug_assertions, test, feature = "bench"))]
        let attempts = session
            .attempts()
            .filter(|a| !a.is_failed())
            .collect::<Vec<_>>();

        Self {
            payment_hash: session.request.payment_hash,
            status,
            failed_error: session.last_error.clone(),
            created_at: session.created_at,
            last_updated_at: session.last_updated_at,
            custom_records: session.request.custom_records.clone(),
            fee,
            #[cfg(any(debug_assertions, test, feature = "bench"))]
            routers: attempts.iter().map(|a| a.route.clone()).collect::<Vec<_>>(),
        }
    }
}

/// The status of a payment attempt, will update as the payment progresses.
/// The transfer path for attempt status is:
///
///    Created --> Inflight ----> Success
//                 /   |
///               /    |
///              |     | ----- no retry ----> Failed
///               \    |
///                \  retry
///                 \  |
///                  \ |
///                  Retrying
///
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AttemptStatus {
    /// initial status, a payment attempt is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// the attempt is retrying after failed
    Retrying,
    /// related HTLC is successfully settled
    Success,
    /// related HTLC is failed
    Failed,
}

impl AttemptStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, AttemptStatus::Success | AttemptStatus::Failed)
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Attempt {
    pub id: u64,
    pub try_limit: u32,
    pub tried_times: u32,
    pub hash: Hash256,
    pub status: AttemptStatus,
    pub payment_hash: Hash256,
    pub route: SessionRoute,
    pub session_key: [u8; 32],
    pub preimage: Option<Hash256>,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub last_error: Option<String>,
}

impl Attempt {
    pub fn set_inflight_status(&mut self) {
        self.status = AttemptStatus::Inflight;
        self.last_error = None;
    }

    pub fn set_success_status(&mut self) {
        self.status = AttemptStatus::Success;
        self.last_error = None;
    }

    pub fn set_failed_status(&mut self, error: &str, retryable: bool) {
        self.last_error = Some(error.to_string());

        if retryable {
            self.status = AttemptStatus::Retrying;
            if self.tried_times < self.try_limit && !error.to_string().contains("WaitingTlcAck") {
                self.tried_times += 1;
                self.last_updated_at = now_timestamp_as_millis_u64();
            }
        } else {
            self.status = AttemptStatus::Failed;
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == AttemptStatus::Success
    }

    pub fn is_inflight(&self) -> bool {
        self.status == AttemptStatus::Inflight
    }

    pub fn is_failed(&self) -> bool {
        self.status == AttemptStatus::Failed
    }

    pub fn is_active(&self) -> bool {
        self.status != AttemptStatus::Failed
    }

    pub fn is_retrying(&self) -> bool {
        self.status == AttemptStatus::Retrying
    }

    pub fn first_hop_channel_outpoint_eq(&self, out_point: &OutPoint) -> bool {
        self.route
            .nodes
            .first()
            .map(|x| x.channel_outpoint.eq(out_point))
            .unwrap_or_default()
    }

    pub(crate) fn channel_outpoints(&self) -> impl Iterator<Item = (Pubkey, &OutPoint, u128)> {
        self.route.channel_outpoints()
    }

    pub fn hops_public_keys(&self) -> Vec<Pubkey> {
        // Skip the first node, which is the sender.
        self.route.nodes.iter().skip(1).map(|x| x.pubkey).collect()
    }
}
