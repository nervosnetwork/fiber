use std::collections::{HashMap, HashSet};

use fiber_types::{Hash256, Pubkey};

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
    opens_in_flight: HashMap<Pubkey, Hash256>,
    waiting_payments: HashMap<Pubkey, HashSet<Hash256>>,
    direct_route_retried: HashSet<Hash256>,
}

impl JitSession {
    pub(crate) fn new(config: JitChannelConfig) -> Self {
        Self {
            config,
            registrations: HashMap::new(),
            pending_registers: HashMap::new(),
            registered_lsp: None,
            opens_in_flight: HashMap::new(),
            waiting_payments: HashMap::new(),
            direct_route_retried: HashSet::new(),
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
            max_amount: self.config.max_amount.unwrap_or(u128::MAX),
        }
    }

    pub(crate) fn queue_register(&mut self, peer: Pubkey, register: JitChannelRegister) {
        self.pending_registers.insert(peer, register);
    }

    pub(crate) fn take_pending_register(&mut self, peer: &Pubkey) -> Option<JitChannelRegister> {
        self.pending_registers.remove(peer)
    }

    pub(crate) fn accept_lsp(&mut self, peer: Pubkey, register: &JitChannelRegister) -> bool {
        let expected = self.config.max_amount.unwrap_or(u128::MAX);
        if register.funding_udt_type_script.is_some() || register.max_amount != expected {
            return false;
        }
        self.registered_lsp = Some(peer);
        true
    }

    pub(crate) fn insert_registration(&mut self, peer: Pubkey, register: JitChannelRegister) {
        self.registrations.insert(peer, register);
    }

    pub(crate) fn is_registered(&self, peer: &Pubkey) -> bool {
        self.registrations.contains_key(peer)
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
        }
        self.registrations.remove(&peer).is_some()
    }

    pub(crate) fn classify_open(
        &self,
        peer: Pubkey,
        open_channel: &OpenChannelV2,
        another_open_in_progress: bool,
    ) -> JitInboundOpen {
        if open_channel.funding_udt_type_script.is_some()
            || open_channel.is_public()
            || self.config.lsp_pubkey != Some(peer)
            || self.registered_lsp != Some(peer)
        {
            return JitInboundOpen::Pass;
        }
        if open_channel.is_one_way() || open_channel.is_external_funding() {
            return JitInboundOpen::Abort(
                "JIT receiver only accepts a two-way private CKB channel",
            );
        }
        if another_open_in_progress {
            return JitInboundOpen::Abort(
                "JIT receiver already has a channel opening with its liquidity node",
            );
        }
        if self
            .config
            .max_amount
            .is_some_and(|max_amount| open_channel.funding_amount > max_amount)
        {
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

    pub(crate) fn clear_open(&mut self, peer: &Pubkey) {
        self.opens_in_flight.remove(peer);
    }

    pub(crate) fn replace_open_id(&mut self, old: Hash256, new: Hash256) {
        if let Some(peer) = self
            .opens_in_flight
            .iter()
            .find_map(|(peer, id)| (*id == old).then_some(*peer))
        {
            self.opens_in_flight.insert(peer, new);
        }
    }

    pub(crate) fn record_open(&mut self, peer: Pubkey, channel_id: Hash256, payment_hash: Hash256) {
        self.opens_in_flight.insert(peer, channel_id);
        self.park(peer, payment_hash);
    }

    pub(crate) fn park(&mut self, peer: Pubkey, payment_hash: Hash256) {
        self.waiting_payments
            .entry(peer)
            .or_default()
            .insert(payment_hash);
    }

    pub(crate) fn take_waiting(&mut self, peer: &Pubkey) -> HashSet<Hash256> {
        self.waiting_payments.remove(peer).unwrap_or_default()
    }

    pub(crate) fn waiting(&self, peer: &Pubkey) -> Vec<Hash256> {
        self.waiting_payments
            .get(peer)
            .map(|payments| payments.iter().copied().collect())
            .unwrap_or_default()
    }

    pub(crate) fn is_waiting(&self, payment_hash: Hash256) -> bool {
        self.waiting_payments
            .values()
            .any(|payments| payments.contains(&payment_hash))
    }

    pub(crate) fn detach(&mut self, payment_hash: Hash256) -> Vec<Hash256> {
        let mut abandon = Vec::new();
        let peers: Vec<Pubkey> = self.waiting_payments.keys().copied().collect();
        for peer in peers {
            let Some(payments) = self.waiting_payments.get_mut(&peer) else {
                continue;
            };
            if !payments.remove(&payment_hash) {
                continue;
            }
            if payments.is_empty() {
                self.waiting_payments.remove(&peer);
                if let Some(channel_id) = self.opens_in_flight.get(&peer).copied() {
                    abandon.push(channel_id);
                }
            }
        }
        abandon
    }

    pub(crate) fn take_failed_open(&mut self, channel_id: Hash256) -> Option<HashSet<Hash256>> {
        let peer = self
            .opens_in_flight
            .iter()
            .find_map(|(peer, id)| (*id == channel_id).then_some(*peer))?;
        self.opens_in_flight.remove(&peer);
        Some(self.waiting_payments.remove(&peer).unwrap_or_default())
    }

    pub(crate) fn forget_direct_retry(&mut self, payment_hash: Hash256) {
        self.direct_route_retried.remove(&payment_hash);
    }

    pub(crate) fn begin_direct_retry(&mut self, payment_hash: Hash256) -> bool {
        self.direct_route_retried.insert(payment_hash)
    }
}
