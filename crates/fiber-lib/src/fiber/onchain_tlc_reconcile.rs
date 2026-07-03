use crate::fiber::channel::{ChannelActorState, ChannelActorStateStore};
use fiber_types::{Hash256, HashAlgorithm, InboundTlcStatus, OutboundTlcStatus, TLCId, TlcInfo};
use tracing::warn;

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

/// A forwarded TLC that expired on a force-closed channel and was consumed on-chain via the
/// timeout path (no preimage revealed), so the upstream TLC must be failed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OnChainExpiredSettledTlc {
    pub forwarding_channel_id: Hash256,
    pub forwarding_tlc_id: u64,
    pub shared_secret: [u8; 32],
}

pub(crate) fn resolve_onchain_tlc(
    channel_id: &Hash256,
    store: &impl ChannelActorStateStore,
    tlc_id: TLCId,
    payment_hash: Hash256,
    hash_algorithm: HashAlgorithm,
) -> OnChainTlcResolution {
    if let Some(preimage) = store.get_on_chain_discovered_preimage(channel_id, &payment_hash) {
        let discovered_payment_hash: Hash256 = hash_algorithm.hash(preimage).into();
        if discovered_payment_hash == payment_hash {
            return OnChainTlcResolution::Fulfilled(preimage);
        }

        warn!(
            "Ignoring on-chain preimage for channel {:?} tlc {:?}: hash {:?} does not match {:?}",
            channel_id, tlc_id, discovered_payment_hash, payment_hash
        );
    }

    if store.is_tlc_settled_on_chain(channel_id, &payment_hash) {
        return OnChainTlcResolution::SettledWithoutPreimage;
    }

    OnChainTlcResolution::Unknown
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

pub(crate) fn collect_onchain_expired_settled_tlcs(
    state: &ChannelActorState,
    store: &impl ChannelActorStateStore,
    expect_expiry: u64,
) -> Vec<OnChainExpiredSettledTlc> {
    let channel_id = state.get_id();
    state
        .tlc_state
        .get_expired_offered_tlcs(expect_expiry)
        .filter_map(|tlc| {
            let (forwarding_channel_id, forwarding_tlc_id) = tlc.forwarding_tlc?;
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
            .then_some(OnChainExpiredSettledTlc {
                forwarding_channel_id,
                forwarding_tlc_id,
                shared_secret: tlc.shared_secret,
            })
        })
        .collect()
}

fn can_reconcile_onchain_fulfillment(tlc: &TlcInfo) -> bool {
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
