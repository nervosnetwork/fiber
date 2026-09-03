//! On-chain identity invariant: a settlement witness only exposes a 20-byte payment-hash
//! prefix and an unlock index. The watchtower must resolve that index against the immutable
//! settlement snapshot committed by the force-closed commitment transaction before persisting
//! a settlement proof for an exact local TLC id and full 32-byte payment hash.

use crate::fiber::channel::{ChannelActorState, ChannelActorStateStore};
use ckb_types::prelude::Unpack;
use fiber_types::{
    ChannelState, CloseFlags, Hash256, HashAlgorithm, InboundTlcStatus, OutboundTlcStatus,
    RemoveTlcReason, SettlementData, TLCId, TlcInfo,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnChainTlcSettlement {
    /// The full payment hash from the commitment's immutable settlement snapshot.
    pub payment_hash: Hash256,
    /// The hash algorithm committed for this TLC.
    pub hash_algorithm: HashAlgorithm,
    /// Preimage revealed by the settlement witness, when the TLC was claimed with one.
    pub preimage: Option<Hash256>,
    /// The settlement transaction that consumed this TLC's output.
    pub tx_hash: Hash256,
    /// The pending-HTLC index inside that settlement witness.
    pub tlc_index: u8,
}

/// Prefix-keyed settlement records written by older versions.
///
/// These records are retained for database compatibility. A matching preimage can still safely
/// prove fulfillment after validating the complete hash, but a no-preimage record cannot prove
/// which TLC sharing the prefix was consumed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyOnChainTlcSettlement {
    /// Preimage observed in the old prefix-keyed settlement record.
    pub preimage: Option<Hash256>,
    /// Settlement transaction hash when recorded by a newer legacy writer.
    pub tx_hash: Option<Hash256>,
    /// Witness index when recorded by a newer legacy writer.
    pub tlc_index: Option<u8>,
}

/// A settlement record decoded from either the exact current key format or the legacy
/// payment-hash-prefix key format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredOnChainTlcSettlement {
    /// An exact record keyed by `(channel_id, TLCId)`.
    Exact(OnChainTlcSettlement),
    /// A prefix-keyed record written by an older version.
    Legacy(LegacyOnChainTlcSettlement),
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

/// An origin-payer TLC whose fulfill was learned off-chain before the channel closed, and whose
/// preimage is now independently confirmed by this channel's on-chain settlement record.
///
/// Such a TLC may already be `RemoteRemoved`, while the corresponding payment attempt is still
/// inflight because the remove commitment handshake never reached `apply_remove_tlc_operation`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OnChainConfirmedPayerTlc {
    pub tlc_id: TLCId,
    pub payment_hash: Hash256,
    pub attempt_id: u64,
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
    let Some(settlement) = store.get_onchain_tlc_settlement(channel_id, tlc_id, &payment_hash)
    else {
        return OnChainTlcResolution::Unknown;
    };
    match settlement {
        StoredOnChainTlcSettlement::Exact(settlement) => {
            if settlement.payment_hash != payment_hash
                || settlement.hash_algorithm != hash_algorithm
            {
                warn!(
                    "Ignoring mismatched on-chain settlement identity for channel {:?} tlc {:?} tx {:?}: stored hash {:?}/{:?}, expected {:?}/{:?}",
                    channel_id,
                    tlc_id,
                    settlement.tx_hash,
                    settlement.payment_hash,
                    settlement.hash_algorithm,
                    payment_hash,
                    hash_algorithm
                );
                return OnChainTlcResolution::Unknown;
            }
            let Some(preimage) = settlement.preimage else {
                return OnChainTlcResolution::SettledWithoutPreimage;
            };
            let discovered_payment_hash: Hash256 = hash_algorithm.hash(preimage).into();
            if discovered_payment_hash == payment_hash {
                return OnChainTlcResolution::Fulfilled(preimage);
            }
            warn!(
                "Ignoring invalid on-chain preimage for channel {:?} tlc {:?} tx {:?}: derived hash {:?}, expected {:?}",
                channel_id, tlc_id, settlement.tx_hash, discovered_payment_hash, payment_hash
            );
            OnChainTlcResolution::Unknown
        }
        StoredOnChainTlcSettlement::Legacy(legacy) => {
            let Some(preimage) = legacy.preimage else {
                return OnChainTlcResolution::Unknown;
            };
            let discovered_payment_hash: Hash256 = hash_algorithm.hash(preimage).into();
            if discovered_payment_hash == payment_hash {
                return OnChainTlcResolution::Fulfilled(preimage);
            }
            warn!(
                "Ignoring legacy prefix-keyed settlement for channel {:?} tlc {:?} tx {:?}: preimage hash {:?}, expected {:?}",
                channel_id, tlc_id, legacy.tx_hash, discovered_payment_hash, payment_hash
            );
            OnChainTlcResolution::Unknown
        }
    }
}

pub(crate) fn onchain_fulfilled_preimage(
    channel_id: &Hash256,
    store: &impl ChannelActorStateStore,
    tlc: &TlcInfo,
) -> Option<Hash256> {
    match resolve_onchain_tlc(
        channel_id,
        store,
        tlc.tlc_id,
        tlc.payment_hash,
        tlc.hash_algorithm,
    ) {
        OnChainTlcResolution::Fulfilled(preimage) => Some(preimage),
        OnChainTlcResolution::Unknown | OnChainTlcResolution::SettledWithoutPreimage => None,
    }
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

/// Collect already-fulfilled first-hop TLCs that still need their payer-side attempt outcome
/// reconciled. The `attempt_id` is local metadata persisted on the offered TLC; forwarded TLCs do
/// not carry one and are deliberately excluded.
pub(crate) fn collect_onchain_confirmed_payer_tlcs(
    state: &ChannelActorState,
    store: &impl ChannelActorStateStore,
) -> Vec<OnChainConfirmedPayerTlc> {
    let channel_id = state.get_id();
    state
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .filter(|tlc| tlc.forwarding_tlc.is_none())
        .filter_map(|tlc| {
            let attempt_id = tlc.attempt_id?;
            let Some(RemoveTlcReason::RemoveTlcFulfill(fulfill)) = &tlc.removed_reason else {
                return None;
            };
            let preimage = onchain_fulfilled_preimage(&channel_id, store, tlc)?;
            if preimage != fulfill.payment_preimage {
                warn!(
                    "Skipping payer TLC {:?} in channel {:?}: local fulfill preimage does not match on-chain preimage",
                    tlc.tlc_id, channel_id
                );
                return None;
            }
            Some(OnChainConfirmedPayerTlc {
                tlc_id: tlc.tlc_id,
                payment_hash: tlc.payment_hash,
                attempt_id,
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
    // Live-channel expiry handling still uses `get_expired_offered_tlcs`, which omits
    // LocalAnnounced TLCs because they are not in the local commitment. On-chain timeout
    // reconciliation must include them: a signed remote commitment already contains the TLC.
    state
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .filter(|tlc| tlc.removed_confirmed_at.is_none() && tlc.expiry < expect_expiry)
        .filter_map(|tlc| {
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
    state
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .filter(|tlc| can_reconcile_onchain_fulfillment(tlc))
        .filter(|tlc| {
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
        })
        .map(|tlc| {
            let TLCId::Received(tlc_id) = tlc.tlc_id else {
                unreachable!("received TLC list contains only received TLCs");
            };
            OnChainReceivedTimeoutSettledTlc { tlc_id }
        })
        .collect()
}

/// Returns true when any TLC included in the active settlement snapshot remains unresolved.
pub(crate) fn has_unresolved_onchain_tlcs_for_snapshot(
    state: &ChannelActorState,
    snapshot: &SettlementData,
    for_remote: bool,
) -> bool {
    snapshot.tlcs.iter().any(|settlement_tlc| {
        let tlc_id = if for_remote {
            settlement_tlc.tlc_id
        } else {
            settlement_tlc.tlc_id.flip()
        };
        let Some(tlc) = state.tlc_state.get(&tlc_id) else {
            return false;
        };
        can_reconcile_onchain_fulfillment(tlc)
    })
}

pub(crate) fn has_unresolved_onchain_tlcs(state: &ChannelActorState) -> bool {
    let local_uncooperative_close = matches!(
        state.state,
        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
    );
    let remote_uncooperative_close = matches!(
        state.state,
        ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
    );

    if local_uncooperative_close {
        // On local force-close, check against the local commitment snapshot which omits uncommitted offered TLCs.
        if let Ok(local_snapshot) = state.build_settlement_data(false) {
            return has_unresolved_onchain_tlcs_for_snapshot(state, &local_snapshot, false);
        }
    } else if remote_uncooperative_close {
        // Deterministically derive the pending remote commitment tx hash to determine whether
        // the remote party published the pending commitment (containing LocalAnnounced TLCs)
        // or the preceding one (omitting them).
        let pending_commitment_hash = state
            .build_commitment_tx_and_settlement_data(true)
            .map(|(tx, _)| tx.hash().unpack())
            .ok();

        return state.tlc_state.all_tlcs().any(|tlc| {
            if !can_reconcile_onchain_fulfillment(tlc) {
                return false;
            }
            if tlc.is_offered()
                && matches!(tlc.outbound_status(), OutboundTlcStatus::LocalAnnounced)
            {
                match (&state.shutdown_transaction_hash, &pending_commitment_hash) {
                    // Confirmed tx matches pending commitment: TLC is on-chain, must wait for settlement.
                    // Different hash (e.g. preceding commitment broadcasted): TLC was never on-chain, do not block.
                    (Some(confirmed_hash), Some(pending_hash)) => confirmed_hash == pending_hash,
                    // Unknown shutdown tx hash: fall back to conservative waiting.
                    _ => true,
                }
            } else {
                // All other active TLCs on remote force-close must be settled on-chain.
                true
            }
        });
    }

    // Fallback: verify all active committed or inbound announced TLCs when snapshot construction is unavailable.

    state.tlc_state.all_tlcs().any(|tlc| {
        if !can_reconcile_onchain_fulfillment(tlc) {
            return false;
        }
        if local_uncooperative_close {
            if tlc.is_offered() {
                matches!(tlc.outbound_status(), OutboundTlcStatus::Committed)
            } else {
                matches!(
                    tlc.inbound_status(),
                    InboundTlcStatus::RemoteAnnounced
                        | InboundTlcStatus::AnnounceWaitPrevAck
                        | InboundTlcStatus::AnnounceWaitAck
                        | InboundTlcStatus::Committed
                )
            }
        } else {
            true
        }
    })
}

pub(crate) fn can_reconcile_onchain_fulfillment(tlc: &TlcInfo) -> bool {
    if tlc.removed_reason.is_some() || tlc.removed_confirmed_at.is_some() {
        return false;
    }

    if tlc.is_offered() {
        matches!(
            tlc.outbound_status(),
            OutboundTlcStatus::LocalAnnounced | OutboundTlcStatus::Committed
        )
    } else {
        matches!(
            tlc.inbound_status(),
            InboundTlcStatus::AnnounceWaitPrevAck
                | InboundTlcStatus::AnnounceWaitAck
                | InboundTlcStatus::Committed
        )
    }
}
