//! On-chain identity invariant: a settlement witness only exposes a 20-byte payment-hash
//! prefix and an unlock index, so on-chain resolution maps to TLCs via
//! `(channel_id, payment_hash[0..20])`. Fulfillment with a preimage is additionally
//! validated against the full 32-byte payment hash; timeout/no-preimage resolution is
//! only sound while a channel has at most one pending TLC per prefix.

use std::collections::{HashMap, HashSet};

use crate::fiber::channel::{ChannelActorState, ChannelActorStateStore};
use fiber_types::{Hash256, HashAlgorithm, InboundTlcStatus, OutboundTlcStatus, TLCId, TlcInfo};
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnChainTlcSettlement {
    /// Preimage revealed by the settlement witness, when the TLC was claimed with one.
    pub preimage: Option<Hash256>,
    /// The settlement transaction that consumed this TLC's output. `None` for legacy
    /// empty-value markers that carry no audit evidence.
    pub tx_hash: Option<Hash256>,
    /// The pending-HTLC index inside the settlement witness. `None` for legacy markers.
    pub tlc_index: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnChainTlcResolution {
    Unknown,
    Fulfilled(Hash256),
    SettledWithoutPreimage,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OnChainFulfilledTlc {
    pub tlc_id: TLCId,
    pub forwarding_tlc: Option<(Hash256, u64)>,
    pub payment_hash: Hash256,
    pub attempt_id: Option<u64>,
    pub preimage: Hash256,
}

/// An offered TLC that expired on a force-closed channel and was consumed on-chain via the
/// timeout path (no preimage revealed).
#[derive(Debug, Clone, Copy)]
pub(crate) struct OnChainTimeoutSettledTlc {
    /// The downstream TLC on the force-closed channel that must be marked removed locally.
    pub tlc_id: TLCId,
    pub payment_hash: Hash256,
    pub shared_secret: [u8; 32],
    pub role: OnChainTimeoutTlcRole,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OnChainReceivedTimeoutSettledTlc {
    pub tlc_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnChainTimeoutTlcRole {
    /// This node forwarded the TLC: fail the upstream received TLC.
    Forwarded {
        forwarding_channel_id: Hash256,
        forwarding_tlc_id: u64,
    },
    /// This node is the origin payer: notify the payment session of final failure.
    OriginPayer { attempt_id: Option<u64> },
}

pub(crate) fn resolve_onchain_tlc(
    channel_id: &Hash256,
    store: &impl ChannelActorStateStore,
    tlc_id: TLCId,
    payment_hash: Hash256,
    hash_algorithm: HashAlgorithm,
) -> OnChainTlcResolution {
    let Some(settlement) = store.get_onchain_tlc_settlement(channel_id, &payment_hash) else {
        return OnChainTlcResolution::Unknown;
    };

    let Some(preimage) = settlement.preimage else {
        return OnChainTlcResolution::SettledWithoutPreimage;
    };

    let discovered_payment_hash: Hash256 = hash_algorithm.hash(preimage).into();
    if discovered_payment_hash == payment_hash {
        return OnChainTlcResolution::Fulfilled(preimage);
    }

    // The channel-scoped settlement record proves the TLC output was consumed on-chain.
    // If the stored preimage does not validate against the full hash, do not fulfill
    // upstream, but still finalize this local TLC as settled without a usable preimage.
    warn!(
        "On-chain settlement record for channel {:?} tlc {:?} tx {:?} has preimage hash {:?}, expected {:?}; treating as settled without preimage",
        channel_id, tlc_id, settlement.tx_hash, discovered_payment_hash, payment_hash
    );
    OnChainTlcResolution::SettledWithoutPreimage
}

pub(crate) fn collect_onchain_fulfilled_tlcs(
    state: &ChannelActorState,
    store: &impl ChannelActorStateStore,
) -> Vec<OnChainFulfilledTlc> {
    let channel_id = state.get_id();
    state
        .tlc_state
        .all_tlcs()
        .filter(|tlc| can_reconcile_onchain_fulfillment(tlc))
        .filter_map(|tlc| {
            let OnChainTlcResolution::Fulfilled(preimage) = resolve_onchain_tlc(
                &channel_id,
                store,
                tlc.tlc_id,
                tlc.payment_hash,
                tlc.hash_algorithm,
            ) else {
                return None;
            };
            Some(OnChainFulfilledTlc {
                tlc_id: tlc.tlc_id,
                forwarding_tlc: tlc.forwarding_tlc,
                payment_hash: tlc.payment_hash,
                attempt_id: tlc.attempt_id,
                preimage,
            })
        })
        .collect()
}

pub(crate) fn collect_onchain_timeout_settled_tlcs(
    state: &ChannelActorState,
    store: &impl ChannelActorStateStore,
    expect_expiry: u64,
) -> Vec<OnChainTimeoutSettledTlc> {
    let channel_id = state.get_id();
    let non_unique_prefixes = non_unique_onchain_settlement_prefixes(state);
    state
        .tlc_state
        .get_expired_offered_tlcs(expect_expiry)
        .filter(|tlc| tlc.removed_reason.is_none())
        .filter_map(|tlc| {
            if has_non_unique_onchain_settlement_key(&channel_id, &non_unique_prefixes, tlc) {
                return None;
            }
            if !matches!(
                resolve_onchain_tlc(
                    &channel_id,
                    store,
                    tlc.tlc_id,
                    tlc.payment_hash,
                    tlc.hash_algorithm,
                ),
                OnChainTlcResolution::SettledWithoutPreimage
            ) {
                return None;
            }

            let role = match tlc.forwarding_tlc {
                Some((forwarding_channel_id, forwarding_tlc_id)) => {
                    OnChainTimeoutTlcRole::Forwarded {
                        forwarding_channel_id,
                        forwarding_tlc_id,
                    }
                }
                None => OnChainTimeoutTlcRole::OriginPayer {
                    attempt_id: tlc.attempt_id,
                },
            };

            Some(OnChainTimeoutSettledTlc {
                tlc_id: tlc.tlc_id,
                payment_hash: tlc.payment_hash,
                shared_secret: tlc.shared_secret,
                role,
            })
        })
        .collect()
}

pub(crate) fn collect_onchain_received_timeout_settled_tlcs(
    state: &ChannelActorState,
    store: &impl ChannelActorStateStore,
) -> Vec<OnChainReceivedTimeoutSettledTlc> {
    let channel_id = state.get_id();
    let non_unique_prefixes = non_unique_onchain_settlement_prefixes(state);
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .filter(|tlc| tlc.removed_reason.is_none())
        .filter_map(|tlc| {
            if has_non_unique_onchain_settlement_key(&channel_id, &non_unique_prefixes, tlc) {
                return None;
            }
            matches!(
                resolve_onchain_tlc(
                    &channel_id,
                    store,
                    tlc.tlc_id,
                    tlc.payment_hash,
                    tlc.hash_algorithm,
                ),
                OnChainTlcResolution::SettledWithoutPreimage
            )
            .then(|| {
                let TLCId::Received(tlc_id) = tlc.tlc_id else {
                    unreachable!("received TLC list contains only received TLCs");
                };
                OnChainReceivedTimeoutSettledTlc { tlc_id }
            })
        })
        .collect()
}

pub(crate) fn has_unresolved_onchain_tlcs(state: &ChannelActorState) -> bool {
    state
        .tlc_state
        .all_tlcs()
        .any(can_reconcile_onchain_fulfillment)
}

pub(crate) fn can_reconcile_onchain_fulfillment(tlc: &TlcInfo) -> bool {
    if tlc.removed_reason.is_some() || tlc.removed_confirmed_at.is_some() {
        return false;
    }

    if tlc.is_offered() {
        matches!(tlc.outbound_status(), OutboundTlcStatus::Committed)
    } else {
        matches!(
            tlc.inbound_status(),
            InboundTlcStatus::AnnounceWaitAck | InboundTlcStatus::Committed
        )
    }
}

/// Returns payment-hash prefixes whose current on-chain settlement key is shared by more than one
/// reconcilable TLC on this channel.
///
/// Settlement records are currently looked up by `(channel_id, payment_hash[0..20])`, not by full
/// payment hash or witness TLC index. When multiple pending TLCs share that lookup key, the record
/// cannot be safely attributed to exactly one local TLC.
pub(crate) fn non_unique_onchain_settlement_prefixes(
    state: &ChannelActorState,
) -> HashSet<[u8; 20]> {
    let mut counts_by_prefix: HashMap<[u8; 20], u32> = HashMap::new();
    for tlc in state
        .tlc_state
        .all_tlcs()
        .filter(|tlc| can_reconcile_onchain_fulfillment(tlc))
    {
        *counts_by_prefix
            .entry(payment_hash_prefix(&tlc.payment_hash))
            .or_default() += 1;
    }
    counts_by_prefix
        .into_iter()
        .filter_map(|(prefix, count)| (count > 1).then_some(prefix))
        .collect()
}

pub(crate) fn payment_hash_prefix(payment_hash: &Hash256) -> [u8; 20] {
    payment_hash.as_ref()[0..20]
        .try_into()
        .expect("payment hash prefix")
}

/// Returns true when this TLC's current settlement lookup key is not unique within the channel.
///
/// No-preimage callers skip such TLCs because applying a prefix-keyed settlement record could
/// otherwise mutate the wrong TLC, relay the wrong upstream remove, or complete the wrong
/// payment/invoice state.
pub(crate) fn has_non_unique_onchain_settlement_key(
    channel_id: &Hash256,
    non_unique_prefixes: &HashSet<[u8; 20]>,
    tlc: &TlcInfo,
) -> bool {
    let prefix = payment_hash_prefix(&tlc.payment_hash);
    if non_unique_prefixes.contains(&prefix) {
        warn!(
            "Skipping on-chain reconciliation for channel {:?} tlc {:?}: on-chain settlement key is shared by multiple pending TLCs",
            channel_id, tlc.tlc_id
        );
        return true;
    }
    false
}
