use std::collections::{HashMap, HashSet, VecDeque};

use fiber_types::{
    Hash256, PersistedJit, PersistedJitOpen, PersistedJitQueue, PersistedJitReservation, Pubkey,
};

use super::config::JitChannelConfig;
use super::types::{JitChannelRegister, OpenChannelV2};

/// Upper bound for one CKB block when budgeting JIT funding confirmation.
const ASSUMED_BLOCK_INTERVAL_MS: u64 = 30_000;
/// Time to negotiate, build, and broadcast a JIT funding transaction before confirmations start.
const FUNDING_SETUP_SLACK_MS: u64 = 3 * 60 * 1000;
/// Time after the funding transaction is confirmed for the payment to be forwarded and settled.
const POST_READY_SLACK_MS: u64 = 2 * 60 * 1000;

pub(crate) fn channel_confirmation_slack_ms(confirmations: u64) -> u64 {
    FUNDING_SETUP_SLACK_MS
        .saturating_add(confirmations.saturating_mul(ASSUMED_BLOCK_INTERVAL_MS))
        .saturating_add(POST_READY_SLACK_MS)
}

pub(crate) enum JitInboundOpen {
    Accept,
    Abort(&'static str),
    Pass,
}

#[derive(Debug, Clone)]
pub(crate) struct JitSession {
    config: JitChannelConfig,
    registrations: HashMap<Pubkey, JitChannelRegister>,
    pending_registers: HashMap<Pubkey, JitChannelRegister>,
    registered_lsp: Option<Pubkey>,
    lsp_open_fee: Option<u128>,
    opens_in_flight: HashMap<Pubkey, Hash256>,
    open_for: HashMap<Pubkey, Hash256>,
    queued: HashMap<Pubkey, VecDeque<Hash256>>,
    routing_peer: HashMap<Hash256, Pubkey>,
    pending_pumps: VecDeque<Pubkey>,
    draining: bool,
    direct_route_retried: HashSet<Hash256>,
    reserved: HashMap<Hash256, (Pubkey, Hash256)>,
}

impl JitSession {
    pub(crate) fn new(config: JitChannelConfig) -> Self {
        Self {
            config,
            registrations: HashMap::new(),
            pending_registers: HashMap::new(),
            registered_lsp: None,
            lsp_open_fee: None,
            opens_in_flight: HashMap::new(),
            open_for: HashMap::new(),
            queued: HashMap::new(),
            routing_peer: HashMap::new(),
            pending_pumps: VecDeque::new(),
            draining: false,
            direct_route_retried: HashSet::new(),
            reserved: HashMap::new(),
        }
    }

    pub(crate) fn config(&self) -> &JitChannelConfig {
        &self.config
    }

    pub(crate) fn lsp_pubkey(&self) -> Option<Pubkey> {
        self.config.lsp_pubkey
    }

    pub(crate) fn service_enabled(&self) -> bool {
        self.config.service_enabled
    }

    pub(crate) fn outbound_register(&self) -> JitChannelRegister {
        JitChannelRegister {
            funding_udt_type_script: None,
            max_amount: self.config.max_amount.unwrap_or(0),
            open_fee: 0,
        }
    }

    pub(crate) fn quoted_open_fee(&self) -> Result<u128, &'static str> {
        if self.config.open_fee_required() {
            Err("JIT open fee is not configured")
        } else {
            Ok(self.config.open_fee_amount())
        }
    }

    pub(crate) fn lsp_open_fee(&self) -> Option<u128> {
        self.lsp_open_fee
    }

    pub(crate) fn queue_register(&mut self, peer: Pubkey, register: JitChannelRegister) {
        self.pending_registers.insert(peer, register);
    }

    pub(crate) fn take_pending_register(&mut self, peer: &Pubkey) -> Option<JitChannelRegister> {
        self.pending_registers.remove(peer)
    }

    pub(crate) fn accept_lsp(&mut self, peer: Pubkey, register: &JitChannelRegister) -> bool {
        let expected = self.config.max_amount.unwrap_or(0);
        if register.funding_udt_type_script.is_some()
            || register.max_amount != expected
            || register.open_fee > self.config.max_open_fee.unwrap_or(0)
        {
            return false;
        }
        // The first acknowledgement sets the fee quoted on invoices. A later
        // message can repeat that fee. A different fee waits until the session
        // is cleared, so a live session cannot reprice invoices already being built.
        if let Some(registered) = self.registered_lsp {
            return registered == peer && self.lsp_open_fee == Some(register.open_fee);
        }
        self.registered_lsp = Some(peer);
        self.lsp_open_fee = Some(register.open_fee);
        true
    }

    pub(crate) fn insert_registration(&mut self, peer: Pubkey, register: JitChannelRegister) {
        self.registrations.insert(peer, register);
    }

    pub(crate) fn is_registered(&self, peer: &Pubkey) -> bool {
        self.registrations.contains_key(peer)
    }

    pub(crate) fn allows_zero_open_fee(&self, next: &Pubkey) -> bool {
        self.config.service_enabled && self.config.open_fee == Some(0) && self.is_registered(next)
    }

    pub(crate) fn amount_exceeds_registration(&self, peer: &Pubkey, amount: u128) -> bool {
        self.registrations
            .get(peer)
            .is_some_and(|register| amount > register.max_amount)
    }

    pub(crate) fn clear_session(&mut self, peer: Pubkey) -> bool {
        self.pending_registers.remove(&peer);
        if self.registered_lsp == Some(peer) {
            self.registered_lsp = None;
            self.lsp_open_fee = None;
        }
        self.registrations.remove(&peer).is_some()
    }

    pub(crate) fn classify_open(
        &self,
        peer: Pubkey,
        open_channel: &OpenChannelV2,
        another_open_in_progress: bool,
    ) -> JitInboundOpen {
        if self.config.lsp_pubkey != Some(peer) || self.registered_lsp != Some(peer) {
            return JitInboundOpen::Pass;
        }
        if open_channel.funding_udt_type_script.is_some()
            || open_channel.is_public()
            || open_channel.is_one_way()
            || open_channel.is_external_funding()
        {
            return JitInboundOpen::Abort(
                "JIT receiver only accepts a two-way private CKB channel",
            );
        }
        if another_open_in_progress {
            return JitInboundOpen::Abort(
                "JIT receiver already has a channel opening with its liquidity node",
            );
        }
        let Some(max_amount) = self.config.max_amount else {
            return JitInboundOpen::Abort("JIT receiver has no funding maximum");
        };
        if open_channel.funding_amount > max_amount {
            return JitInboundOpen::Abort("JIT channel funding exceeds receiver maximum");
        }
        JitInboundOpen::Accept
    }

    pub(crate) fn open_id(&self, peer: &Pubkey) -> Option<Hash256> {
        self.opens_in_flight.get(peer).copied()
    }

    pub(crate) fn has_open(&self, peer: &Pubkey) -> bool {
        self.opens_in_flight.contains_key(peer)
    }

    pub(crate) fn opens_in_flight_count(&self) -> usize {
        self.opens_in_flight.len()
    }

    pub(crate) fn replace_open_id(&mut self, old: Hash256, new: Hash256) {
        if let Some(peer) = self
            .opens_in_flight
            .iter()
            .find_map(|(peer, id)| (*id == old).then_some(*peer))
        {
            self.opens_in_flight.insert(peer, new);
        }
        if let Some(reservation) = self.reserved.remove(&old) {
            self.reserved.insert(new, reservation);
        }
    }

    pub(crate) fn reserve_channel(
        &mut self,
        peer: Pubkey,
        channel_id: Hash256,
        payment_hash: Hash256,
    ) {
        self.reserved.insert(channel_id, (peer, payment_hash));
        self.routing_peer.entry(payment_hash).or_insert(peer);
    }

    pub(crate) fn release_reservation(&mut self, payment_hash: Hash256) {
        self.reserved.retain(|_, (_, owner)| *owner != payment_hash);
    }

    pub(crate) fn allows_channel_for_payment(
        &self,
        channel_id: Hash256,
        payment_hash: Hash256,
    ) -> bool {
        self.reserved
            .get(&channel_id)
            .is_none_or(|(_, owner)| *owner == payment_hash)
    }

    pub(crate) fn snapshot(&self) -> PersistedJit {
        let mut opens = Vec::new();
        for (peer, channel_id) in &self.opens_in_flight {
            let Some(payment_hash) = self.open_for.get(peer) else {
                continue;
            };
            opens.push(PersistedJitOpen {
                peer: *peer,
                channel_id: *channel_id,
                payment_hash: *payment_hash,
            });
        }
        opens.sort_by(|left, right| {
            left.peer
                .cmp(&right.peer)
                .then_with(|| left.payment_hash.as_ref().cmp(right.payment_hash.as_ref()))
        });
        let mut queued: Vec<_> = self
            .queued
            .iter()
            .map(|(peer, queue)| PersistedJitQueue {
                peer: *peer,
                payments: queue.iter().copied().collect(),
            })
            .collect();
        queued.sort_by_key(|queue| queue.peer);
        let mut reserved: Vec<_> = self
            .reserved
            .iter()
            .map(
                |(channel_id, (peer, payment_hash))| PersistedJitReservation {
                    peer: *peer,
                    channel_id: *channel_id,
                    payment_hash: *payment_hash,
                },
            )
            .collect();
        reserved.sort_by(|left, right| left.channel_id.as_ref().cmp(right.channel_id.as_ref()));
        PersistedJit {
            opens,
            queued,
            reserved,
        }
    }

    pub(crate) fn record_open(&mut self, peer: Pubkey, channel_id: Hash256, payment_hash: Hash256) {
        self.opens_in_flight.insert(peer, channel_id);
        self.open_for.insert(peer, payment_hash);
        self.routing_peer.insert(payment_hash, peer);
        self.remove_queued(peer, payment_hash);
    }

    pub(crate) fn enqueue(&mut self, peer: Pubkey, payment_hash: Hash256) {
        if self.open_for.get(&peer) == Some(&payment_hash) {
            return;
        }
        let queue = self.queued.entry(peer).or_default();
        if queue.contains(&payment_hash) {
            return;
        }
        queue.push_back(payment_hash);
        self.routing_peer.insert(payment_hash, peer);
    }

    pub(crate) fn front_queued(&self, peer: &Pubkey) -> Option<Hash256> {
        self.queued
            .get(peer)
            .and_then(|queue| queue.front().copied())
    }

    pub(crate) fn release_queued(&mut self, peer: &Pubkey, payment_hash: Hash256) {
        self.remove_queued(*peer, payment_hash);
    }

    pub(crate) fn take_queued(&mut self, peer: &Pubkey) -> Vec<Hash256> {
        self.queued
            .remove(peer)
            .map(|queue| queue.into_iter().collect())
            .unwrap_or_default()
    }

    pub(crate) fn take_funded_payment(&mut self, peer: &Pubkey) -> Option<Hash256> {
        self.opens_in_flight.remove(peer);
        self.open_for.remove(peer)
    }

    pub(crate) fn is_waiting(&self, payment_hash: Hash256) -> bool {
        self.open_for
            .values()
            .any(|payment| *payment == payment_hash)
            || self
                .queued
                .values()
                .any(|queue| queue.contains(&payment_hash))
    }

    pub(crate) fn peer_for_payment(&self, payment_hash: Hash256) -> Option<Pubkey> {
        self.routing_peer.get(&payment_hash).copied()
    }

    pub(crate) fn tracks_payment(&self, payment_hash: Hash256) -> bool {
        self.is_waiting(payment_hash)
            || self
                .reserved
                .values()
                .any(|(_, owner)| *owner == payment_hash)
    }

    pub(crate) fn detach(&mut self, payment_hash: Hash256) -> (Option<Pubkey>, Vec<Hash256>) {
        let Some(peer) = self.routing_peer.remove(&payment_hash) else {
            return (None, Vec::new());
        };
        self.remove_queued(peer, payment_hash);
        let mut abandon = Vec::new();
        if self.open_for.get(&peer) == Some(&payment_hash) {
            self.open_for.remove(&peer);
            if let Some(channel_id) = self.opens_in_flight.remove(&peer) {
                abandon.push(channel_id);
            }
        }
        (Some(peer), abandon)
    }

    pub(crate) fn take_failed_open(&mut self, channel_id: Hash256) -> Option<Hash256> {
        let peer = self
            .opens_in_flight
            .iter()
            .find_map(|(peer, id)| (*id == channel_id).then_some(*peer))?;
        self.opens_in_flight.remove(&peer);
        self.open_for.remove(&peer)
    }

    pub(crate) fn take_peer(&mut self, peer: &Pubkey) -> (Option<Hash256>, Vec<Hash256>) {
        let channel_id = self.opens_in_flight.remove(peer);
        let mut payments = Vec::new();
        if let Some(payment) = self.open_for.remove(peer) {
            self.routing_peer.remove(&payment);
            payments.push(payment);
        }
        if let Some(queue) = self.queued.remove(peer) {
            for payment in queue {
                self.routing_peer.remove(&payment);
                if !payments.contains(&payment) {
                    payments.push(payment);
                }
            }
        }
        (channel_id, payments)
    }

    pub(crate) fn schedule_pump(&mut self, peer: Pubkey) {
        if !self.pending_pumps.contains(&peer) {
            self.pending_pumps.push_back(peer);
        }
    }

    pub(crate) fn unschedule_pump(&mut self, peer: &Pubkey) {
        self.pending_pumps.retain(|queued| queued != peer);
    }

    pub(crate) fn pop_pump(&mut self) -> Option<Pubkey> {
        self.pending_pumps.pop_front()
    }

    pub(crate) fn try_begin_drain(&mut self) -> bool {
        if self.draining {
            return false;
        }
        self.draining = true;
        true
    }

    pub(crate) fn end_drain(&mut self) {
        self.draining = false;
    }

    fn remove_queued(&mut self, peer: Pubkey, payment_hash: Hash256) {
        let Some(queue) = self.queued.get_mut(&peer) else {
            return;
        };
        queue.retain(|payment| *payment != payment_hash);
        if queue.is_empty() {
            self.queued.remove(&peer);
        }
    }

    pub(crate) fn forget_direct_retry(&mut self, payment_hash: Hash256) {
        self.direct_route_retried.remove(&payment_hash);
    }

    pub(crate) fn begin_direct_retry(&mut self, payment_hash: Hash256) -> bool {
        self.direct_route_retried.insert(payment_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fiber::config::JitChannelConfig;
    use crate::fiber::types::{JitChannelRegister, OpenChannelV2};
    use crate::{gen_rand_fiber_private_key, gen_rand_fiber_public_key};
    use ckb_types::packed::Script;
    use fiber_types::{ChannelFlags, Hash256, Pubkey};
    use musig2::SecNonce;

    fn register(open_fee: u128) -> JitChannelRegister {
        JitChannelRegister {
            funding_udt_type_script: None,
            max_amount: u128::MAX,
            open_fee,
        }
    }

    fn capped_session(peer: Pubkey) -> JitSession {
        JitSession::new(JitChannelConfig {
            lsp_pubkey: Some(peer),
            max_amount: Some(u128::MAX),
            max_open_fee: Some(u128::MAX),
            ..JitChannelConfig::default()
        })
    }

    #[test]
    fn lsp_ack_keeps_the_first_open_fee() {
        let peer = gen_rand_fiber_public_key();
        let mut session = capped_session(peer);
        assert!(session.accept_lsp(peer, &register(100)));
        assert_eq!(session.lsp_open_fee(), Some(100));
        assert!(!session.accept_lsp(peer, &register(500)));
        assert_eq!(session.lsp_open_fee(), Some(100));
        assert!(session.accept_lsp(peer, &register(100)));
        session.clear_session(peer);
        assert!(session.lsp_open_fee().is_none());
        assert!(session.accept_lsp(peer, &register(500)));
        assert_eq!(session.lsp_open_fee(), Some(500));
    }

    #[test]
    fn lsp_ack_rejects_an_open_fee_above_the_receiver_maximum() {
        let peer = gen_rand_fiber_public_key();
        let mut session = JitSession::new(JitChannelConfig {
            lsp_pubkey: Some(peer),
            max_amount: Some(u128::MAX),
            max_open_fee: Some(100),
            ..JitChannelConfig::default()
        });
        assert!(!session.accept_lsp(peer, &register(101)));
        assert!(session.lsp_open_fee().is_none());
        assert!(session.accept_lsp(peer, &register(100)));
        assert_eq!(session.lsp_open_fee(), Some(100));
    }

    fn inbound_open(amount: u128, flags: ChannelFlags, udt: bool) -> OpenChannelV2 {
        let nonce = SecNonce::build(gen_rand_fiber_private_key().as_ref())
            .build()
            .public_nonce();
        OpenChannelV2 {
            chain_hash: Hash256::default(),
            channel_id: Hash256::default(),
            funding_udt_type_script: udt.then(Script::default),
            funding_amount: amount,
            shutdown_script: Script::default(),
            reserved_ckb_amount: 0,
            funding_fee_rate: 0,
            commitment_fee_rate: 0,
            commitment_delay_epoch: 0,
            max_tlc_value_in_flight: 0,
            max_tlc_number_in_flight: 0,
            funding_pubkey: gen_rand_fiber_public_key(),
            tlc_basepoint: gen_rand_fiber_public_key(),
            first_per_commitment_point: gen_rand_fiber_public_key(),
            second_per_commitment_point: gen_rand_fiber_public_key(),
            channel_announcement_nonce: None,
            initial_commitment_nonce: nonce,
            channel_flags: flags,
            channel_features: 1,
        }
    }

    #[test]
    fn classify_open_aborts_public_udt_and_uncapped_opens() {
        let peer = gen_rand_fiber_public_key();
        let mut session = capped_session(peer);
        assert!(session.accept_lsp(peer, &register(0)));
        let private = inbound_open(50, ChannelFlags::empty(), false);
        assert!(matches!(
            session.classify_open(peer, &private, false),
            JitInboundOpen::Accept
        ));

        let public = inbound_open(50, ChannelFlags::PUBLIC, false);
        assert!(matches!(
            session.classify_open(peer, &public, false),
            JitInboundOpen::Abort(_)
        ));
        let udt = inbound_open(50, ChannelFlags::empty(), true);
        assert!(matches!(
            session.classify_open(peer, &udt, false),
            JitInboundOpen::Abort(_)
        ));
        let one_way = inbound_open(50, ChannelFlags::ONE_WAY, false);
        assert!(matches!(
            session.classify_open(peer, &one_way, false),
            JitInboundOpen::Abort(_)
        ));
        let external = inbound_open(50, ChannelFlags::EXTERNAL_FUNDING, false);
        assert!(matches!(
            session.classify_open(peer, &external, false),
            JitInboundOpen::Abort(_)
        ));

        let mut uncapped = JitSession::new(JitChannelConfig {
            lsp_pubkey: Some(peer),
            max_open_fee: Some(0),
            ..JitChannelConfig::default()
        });
        assert!(uncapped.accept_lsp(
            peer,
            &JitChannelRegister {
                funding_udt_type_script: None,
                max_amount: 0,
                open_fee: 0,
            }
        ));
        assert!(matches!(
            uncapped.classify_open(peer, &private, false),
            JitInboundOpen::Abort(_)
        ));
    }

    #[test]
    fn one_open_does_not_cover_a_later_payment() {
        let peer = gen_rand_fiber_public_key();
        let mut session = JitSession::new(JitChannelConfig::default());
        let first = Hash256::default();
        let second = Hash256::from([1u8; 32]);
        let channel_id = Hash256::from([2u8; 32]);

        session.record_open(peer, channel_id, first);
        session.enqueue(peer, second);
        assert!(session.is_waiting(first));
        assert!(session.is_waiting(second));
        assert_eq!(session.front_queued(&peer), Some(second));

        assert_eq!(session.take_funded_payment(&peer), Some(first));
        assert!(!session.has_open(&peer));
        assert!(!session.is_waiting(first));
        assert_eq!(session.front_queued(&peer), Some(second));

        session.record_open(peer, channel_id, first);
        session.enqueue(peer, second);
        let (detached_peer, abandon) = session.detach(first);
        assert_eq!(detached_peer, Some(peer));
        assert_eq!(abandon, vec![channel_id]);
        assert!(!session.has_open(&peer));
        assert_eq!(session.front_queued(&peer), Some(second));
        assert!(session.is_waiting(second));
    }

    #[test]
    fn reserved_channel_stays_with_its_payment_across_snapshot() {
        let peer = gen_rand_fiber_public_key();
        let mut session = JitSession::new(JitChannelConfig::default());
        let first = Hash256::default();
        let second = Hash256::from([1u8; 32]);
        let channel_id = Hash256::from([2u8; 32]);

        session.record_open(peer, channel_id, first);
        session.enqueue(peer, second);
        let saved = session.snapshot();
        assert_eq!(saved.opens.len(), 1);
        assert_eq!(saved.queued.len(), 1);
        assert_eq!(saved.queued[0].payments, vec![second]);

        session.reserve_channel(peer, channel_id, first);
        assert!(session.allows_channel_for_payment(channel_id, first));
        assert!(!session.allows_channel_for_payment(channel_id, second));
        let queued = session.take_queued(&peer);
        assert_eq!(queued, vec![second]);
        assert!(session.has_open(&peer));

        let mut restored = JitSession::new(JitChannelConfig::default());
        let saved = session.snapshot();
        for open in saved.opens {
            restored.record_open(open.peer, open.channel_id, open.payment_hash);
        }
        for reservation in &saved.reserved {
            restored.reserve_channel(
                reservation.peer,
                reservation.channel_id,
                reservation.payment_hash,
            );
        }
        assert!(restored.has_open(&peer));
        assert!(restored.allows_channel_for_payment(channel_id, first));
        assert!(!restored.allows_channel_for_payment(channel_id, second));
    }
}
