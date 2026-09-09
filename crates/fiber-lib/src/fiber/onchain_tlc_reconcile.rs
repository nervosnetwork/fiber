//! On-chain identity invariant: a settlement witness only exposes a 20-byte payment-hash
//! prefix and an unlock index. The watchtower must resolve that index against the immutable
//! settlement snapshot committed by the force-closed commitment transaction before persisting
//! a settlement proof for an exact local TLC id and full 32-byte payment hash.

use crate::fiber::channel::{
    settlement_data_to_witness, settlement_tlc_to_witness, ChannelActorState,
    ChannelActorStateStore,
};
use ckb_hash::blake2b_256;
use ckb_sdk::util::blake160;
use ckb_types::packed::Script;
use fiber_types::{
    ChannelData, ChannelState, CloseFlags, Hash256, HashAlgorithm, InboundTlcStatus,
    OutboundTlcStatus, Pubkey, RemoveTlcReason, SettlementData, TLCId, TlcInfo,
};
use musig2::{secp::Point, KeyAggContext};
use serde::{Deserialize, Serialize};
use tracing::warn;

// Used by Watchtower scanning; builds without the watchtower feature may leave it unused.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedSettlementTlc {
    pub tlc_id: TLCId,
    pub payment_hash: Hash256,
    pub hash_algorithm: HashAlgorithm,
    pub witness: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCommitmentLock {
    pub for_remote: bool,
    pub commitment_number: u64,
    pub witness_hash: [u8; 20],
}

/// Validate the script and funding keys, then extract the commitment direction, number and witness hash.
pub fn parse_commitment_lock(
    commitment_lock: &Script,
    local_funding: &Pubkey,
    remote_funding: &Pubkey,
) -> Option<ParsedCommitmentLock> {
    let expected_commitment_lock = crate::ckb::contracts::get_script_by_contract(
        crate::ckb::contracts::Contract::CommitmentLock,
        &[],
    );
    if commitment_lock.code_hash() != expected_commitment_lock.code_hash()
        || commitment_lock.hash_type() != expected_commitment_lock.hash_type()
    {
        return None;
    }
    let lock_args = commitment_lock.args().raw_data();
    if lock_args.len() < 56 {
        return None;
    }
    let pubkey_hash = &lock_args[0..20];

    // Aggregated pubkey for remote commitment: [local_funding, remote_funding]
    let remote_ctx = KeyAggContext::new([*local_funding, *remote_funding]).ok()?;
    let remote_xonly = remote_ctx.aggregated_pubkey::<Point>().serialize_xonly();
    let expected_remote_pubkey_hash = &blake2b_256(remote_xonly)[0..20];

    // Aggregated pubkey for local commitment: [remote_funding, local_funding]
    let local_ctx = KeyAggContext::new([*remote_funding, *local_funding]).ok()?;
    let local_xonly = local_ctx.aggregated_pubkey::<Point>().serialize_xonly();
    let expected_local_pubkey_hash = &blake2b_256(local_xonly)[0..20];

    let for_remote = if pubkey_hash == expected_remote_pubkey_hash {
        true
    } else if pubkey_hash == expected_local_pubkey_hash {
        false
    } else {
        return None;
    };

    let commitment_number = u64::from_be_bytes(lock_args[28..36].try_into().ok()?);
    let witness_hash: [u8; 20] = lock_args[36..56].try_into().ok()?;

    Some(ParsedCommitmentLock {
        for_remote,
        commitment_number,
        witness_hash,
    })
}

/// Select a candidate local, preceding remote or pending remote snapshot without verifying its hash.
pub fn settlement_data_for_commitment(
    channel_data: &ChannelData,
    for_remote: bool,
    commitment_number: u64,
) -> &SettlementData {
    if for_remote {
        if channel_data
            .revocation_data
            .as_ref()
            .and_then(|revocation| {
                commitment_number
                    .checked_sub(1)
                    .map(|previous| revocation.commitment_number == previous)
            })
            .unwrap_or(false)
        {
            &channel_data.remote_settlement_data
        } else {
            &channel_data.pending_remote_settlement_data
        }
    } else {
        &channel_data.local_settlement_data
    }
}

/// Wrap the candidate snapshot in Some; this adds no validation and currently has no callers.
#[allow(dead_code)]
pub fn select_commitment_settlement_data(
    channel_data: &ChannelData,
    for_remote: bool,
    commitment_number: u64,
) -> Option<&SettlementData> {
    Some(settlement_data_for_commitment(
        channel_data,
        for_remote,
        commitment_number,
    ))
}

/// Accept a candidate snapshot only when its witness hash matches the on-chain commitment lock.
pub fn verify_and_select_settlement_data<'a>(
    channel_data: &'a ChannelData,
    commitment_lock: &Script,
) -> Option<(bool, u64, &'a SettlementData)> {
    let parsed = parse_commitment_lock(
        commitment_lock,
        &channel_data.local_funding_pubkey,
        &channel_data.remote_funding_pubkey,
    )?;
    let settlement_data =
        settlement_data_for_commitment(channel_data, parsed.for_remote, parsed.commitment_number);
    let settlement_witness = settlement_data_to_witness(
        settlement_data,
        parsed.for_remote,
        channel_data.local_settlement_key.clone(),
        channel_data.remote_settlement_key,
    );
    if blake160(&settlement_witness).as_ref() != parsed.witness_hash {
        warn!(
            "Settlement snapshot hash does not match commitment lock for channel {:?}, commitment {}",
            channel_data.channel_id, parsed.commitment_number
        );
        return None;
    }
    Some((parsed.for_remote, parsed.commitment_number, settlement_data))
}

/// Recover the TLC reconciliation scope for a shutdown transaction.
/// Revoked remote commitments are resolved by revocation rather than individual TLC claims.
/// Represent their scope as empty; this is not a witness snapshot and must never be used by
/// the watchtower to decode settlement witnesses. Completion still requires the chain signal
/// that all commitment cells have been spent.
pub(crate) fn recover_shutdown_settlement_data(
    channel_data: &ChannelData,
    commitment_lock: &Script,
) -> Option<(bool, u64, SettlementData)> {
    let parsed = parse_commitment_lock(
        commitment_lock,
        &channel_data.local_funding_pubkey,
        &channel_data.remote_funding_pubkey,
    )?;
    if parsed.for_remote
        && channel_data
            .revocation_data
            .as_ref()
            .is_some_and(|revocation| parsed.commitment_number <= revocation.commitment_number)
    {
        return Some((
            true,
            parsed.commitment_number,
            SettlementData {
                local_amount: 0,
                remote_amount: 0,
                tlcs: vec![],
            },
        ));
    }
    verify_and_select_settlement_data(channel_data, commitment_lock)
        .map(|(for_remote, number, data)| (for_remote, number, data.clone()))
}

/// Extract TLC identities and witnesses from a verified snapshot for Watchtower scanning,
/// converting TLC IDs to the local channel's direction.
#[allow(dead_code)]
pub fn tracked_settlement_tlcs(
    commitment_lock: &Script,
    channel_data: &ChannelData,
    for_remote: bool,
) -> Option<Vec<TrackedSettlementTlc>> {
    let (detected_for_remote, _commitment_number, settlement_data) =
        verify_and_select_settlement_data(channel_data, commitment_lock)?;
    if detected_for_remote != for_remote {
        warn!(
            "Commitment lock direction mismatch: detected for_remote={}, expected for_remote={}",
            detected_for_remote, for_remote
        );
        return None;
    }
    Some(
        settlement_data
            .tlcs
            .iter()
            .map(|tlc| TrackedSettlementTlc {
                tlc_id: if for_remote {
                    tlc.tlc_id
                } else {
                    tlc.tlc_id.flip()
                },
                payment_hash: tlc.payment_hash,
                hash_algorithm: tlc.hash_algorithm,
                witness: settlement_tlc_to_witness(tlc, for_remote),
            })
            .collect(),
    )
}

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
            // A TLC present in the active settlement snapshot that is missing from local state
            // must conservatively block settlement.
            return true;
        };
        can_reconcile_onchain_fulfillment(tlc)
    })
}

/// Check the matching shutdown snapshot; a force-close without a valid snapshot must keep waiting.
pub(crate) fn has_unresolved_onchain_tlcs(
    state: &ChannelActorState,
    store: &impl ChannelActorStateStore,
) -> bool {
    if let Some(record) = state.load_shutdown_settlement_record(store) {
        return has_unresolved_onchain_tlcs_for_snapshot(
            state,
            &record.settlement_data,
            record.for_remote,
        );
    }
    if matches!(state.state, ChannelState::Closed(flags)
        if flags.intersects(CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::UNCOOPERATIVE_REMOTE))
    {
        // An empty current TLC list is not evidence about the historical commitment.
        // Keep recovery scheduled until its snapshot is known and verified.
        return true;
    }
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
