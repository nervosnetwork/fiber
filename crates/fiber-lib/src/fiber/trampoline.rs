use std::collections::{hash_map::Entry, HashMap};

use ckb_types::packed::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::config::FiberConfig;
use super::payment::{SendPaymentData, SendPaymentDataBuilder};
use fiber_types::{EntityHex, Hash256, HashAlgorithm, PrevTlcInfo, Pubkey, TrampolineContext};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrampolineForwardingRejection {
    DuplicatePayment,
    GlobalLimit,
    ChannelLimit,
    ExpiryTooFar,
}

impl TrampolineForwardingRejection {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DuplicatePayment => "duplicate_payment",
            Self::GlobalLimit => "global_limit",
            Self::ChannelLimit => "channel_limit",
            Self::ExpiryTooFar => "expiry_too_far",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TrampolineForwardingLimits {
    max_concurrent_payments: usize,
    max_concurrent_payments_per_channel: usize,
    max_expiry_delta: u64,
}

impl From<&FiberConfig> for TrampolineForwardingLimits {
    fn from(config: &FiberConfig) -> Self {
        Self {
            max_concurrent_payments: config.trampoline_forwarding_max_concurrent_payments(),
            max_concurrent_payments_per_channel: config
                .trampoline_forwarding_max_concurrent_payments_per_channel(),
            max_expiry_delta: config.trampoline_forwarding_max_expiry_delta(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct TrampolineForwardingTracker {
    active_payments: HashMap<Hash256, Hash256>,
    active_payments_per_channel: HashMap<Hash256, usize>,
}

impl TrampolineForwardingTracker {
    pub(crate) fn try_reserve(
        &mut self,
        payment_hash: Hash256,
        previous_channel_id: Hash256,
        max_outgoing_tlc_expiry: u64,
        now: u64,
        limits: TrampolineForwardingLimits,
    ) -> Result<(), TrampolineForwardingRejection> {
        if self.active_payments.contains_key(&payment_hash) {
            return Err(TrampolineForwardingRejection::DuplicatePayment);
        }
        if max_outgoing_tlc_expiry.saturating_sub(now) > limits.max_expiry_delta {
            return Err(TrampolineForwardingRejection::ExpiryTooFar);
        }
        if self.active_payments.len() >= limits.max_concurrent_payments {
            return Err(TrampolineForwardingRejection::GlobalLimit);
        }
        if self
            .active_payments_per_channel
            .get(&previous_channel_id)
            .copied()
            .unwrap_or_default()
            >= limits.max_concurrent_payments_per_channel
        {
            return Err(TrampolineForwardingRejection::ChannelLimit);
        }

        self.track(payment_hash, previous_channel_id);
        Ok(())
    }

    pub(crate) fn track(&mut self, payment_hash: Hash256, previous_channel_id: Hash256) {
        let Entry::Vacant(entry) = self.active_payments.entry(payment_hash) else {
            return;
        };
        entry.insert(previous_channel_id);
        *self
            .active_payments_per_channel
            .entry(previous_channel_id)
            .or_default() += 1;
    }

    pub(crate) fn release(&mut self, payment_hash: &Hash256) -> bool {
        let Some(previous_channel_id) = self.active_payments.remove(payment_hash) else {
            return false;
        };
        if let Some(count) = self
            .active_payments_per_channel
            .get_mut(&previous_channel_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.active_payments_per_channel
                    .remove(&previous_channel_id);
            }
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.active_payments.len()
    }
}

/// A validated trampoline forwarding request ready for immediate dispatch.
///
/// Keeping this separate from `PaymentActor` construction gives the hosted LSP
/// integration a narrow point where it can later choose to defer delivery.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrampolineForwardingRequest {
    pub payment_hash: Hash256,
    pub next_node_id: Pubkey,
    pub amount_to_forward: u128,
    pub hash_algorithm: HashAlgorithm,
    pub build_max_fee_amount: u128,
    pub tlc_expiry_delta: u64,
    pub tlc_expiry_limit: u64,
    pub max_parts: Option<u64>,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub remaining_trampoline_onion: Vec<u8>,
    pub previous_tlc: PrevTlcInfo,
    pub max_outgoing_tlc_expiry: u64,
}

impl TrampolineForwardingRequest {
    pub(crate) fn into_send_payment_data(self) -> Result<SendPaymentData, String> {
        SendPaymentDataBuilder::new(self.next_node_id, self.amount_to_forward, self.payment_hash)
            .final_tlc_expiry_delta(self.tlc_expiry_delta)
            .tlc_expiry_limit(self.tlc_expiry_limit)
            .max_fee_amount(Some(self.build_max_fee_amount))
            .max_parts(self.max_parts)
            .udt_type_script(self.udt_type_script)
            .trampoline_context(Some(TrampolineContext {
                remaining_trampoline_onion: self.remaining_trampoline_onion,
                // The current trampoline forwarding flow supports one upstream TLC.
                previous_tlcs: vec![self.previous_tlc],
                hash_algorithm: self.hash_algorithm,
                max_outgoing_tlc_expiry: Some(self.max_outgoing_tlc_expiry),
            }))
            .allow_mpp(self.max_parts.is_some_and(|value| value > 1))
            .build()
    }
}
