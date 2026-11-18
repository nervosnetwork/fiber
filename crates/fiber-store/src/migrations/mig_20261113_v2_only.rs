//! Admit existing V2 channels without retaining a V1 runtime.
use std::collections::HashSet;

use bincode::Options;
use ckb_types::{packed::OutPoint, prelude::*};
use fiber_types::channel_v2_validation::validate_channel_v2;
use fiber_types::schema::{
    ARCHIVED_V1_PREFIX, ATTEMPT_PREFIX, CHANNEL_ACTOR_STATE_PREFIX, CHANNEL_OPEN_RECORD_PREFIX,
    CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX, CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX, PAYMENT_SESSION_PREFIX,
    PUBKEY_CHANNEL_ID_PREFIX, WATCHTOWER_CHANNEL_PREFIX, WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX,
};
use fiber_types::{
    ChannelActorData, ChannelData, ChannelFeatures, ChannelOpenRecord, ChannelSessionV2,
};
use fiber_types_0100::schema::PENDING_COMMIT_DIFF_PREFIX;
use fiber_types_0100::{
    ChannelActorData as OldChannelActorData, ChannelData as OldChannelData,
    ChannelOpenRecord as OldChannelOpenRecord, ChannelOpeningStatus as OldChannelOpeningStatus,
    ChannelState as OldChannelState, CloseFlags as OldCloseFlags, Hash256 as OldHash256,
};
use serde::de::DeserializeOwned;

use crate::migration::{Migration, MigrationStore, MIGRATION_VERSION_KEY};

use super::decode_as_new;

const MIGRATION_DB_VERSION: &str = "20261113000001";
const RELEASED_VERSION: &str = "20260925120000";

#[derive(Default)]
pub struct MigrationObj;

impl MigrationObj {
    pub fn new() -> Self {
        Self
    }
}

impl Migration for MigrationObj {
    fn version(&self) -> &str {
        MIGRATION_DB_VERSION
    }

    fn preflight(&self, store: &dyn MigrationStore) -> Result<(), String> {
        scan(store, false)
    }

    fn migrate(&self, store: &dyn MigrationStore) -> Result<(), String> {
        // Also protect direct callers and recheck after a user confirmation prompt.
        self.preflight(store)?;
        scan(store, true)
    }

    fn is_break_change(&self) -> bool {
        true
    }
}

fn convert_named<T: serde::Serialize, U: DeserializeOwned>(old: T) -> Result<U, String> {
    serde_json::from_value(serde_json::to_value(old).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())
}

fn convert_settlement(
    old: fiber_types_0100::SettlementData,
) -> Result<fiber_types::SettlementData, String> {
    Ok(fiber_types::SettlementData {
        local_amount: old.local_amount,
        remote_amount: old.remote_amount,
        tlcs: old
            .tlcs
            .into_iter()
            .map(|tlc| {
                let key: fiber_types::Privkey = decode_as_new(tlc.local_key)?;
                Ok(fiber_types::SettlementTlc {
                    tlc_id: decode_as_new(tlc.tlc_id)?,
                    hash_algorithm: decode_as_new(tlc.hash_algorithm)?,
                    payment_amount: tlc.payment_amount,
                    payment_hash: decode_as_new(tlc.payment_hash)?,
                    expiry: tlc.expiry,
                    local_key_pubkey: Some(key.pubkey()),
                    local_key: Some(key),
                    local_key_commitment_number: None,
                    remote_key: decode_as_new(tlc.remote_key)?,
                })
            })
            .collect::<Result<_, String>>()?,
    })
}

fn convert_watch_channel(old: OldChannelData) -> Result<ChannelData, String> {
    let key: fiber_types::Privkey = decode_as_new(old.local_settlement_key)?;
    Ok(ChannelData {
        channel_id: decode_as_new(old.channel_id)?,
        funding_udt_type_script: old.funding_udt_type_script,
        local_settlement_key_pubkey: Some(key.pubkey()),
        local_settlement_key: Some(key),
        remote_settlement_key: decode_as_new(old.remote_settlement_key)?,
        local_funding_pubkey: decode_as_new(old.local_funding_pubkey)?,
        remote_funding_pubkey: decode_as_new(old.remote_funding_pubkey)?,
        remote_settlement_data: convert_settlement(old.remote_settlement_data)?,
        pending_remote_settlement_data: convert_settlement(old.pending_remote_settlement_data)?,
        local_settlement_data: convert_settlement(old.local_settlement_data)?,
        revocation_data: decode_as_new(old.revocation_data)?,
        channel_features: ChannelFeatures::V2,
        payout_lock_script: None,
    })
}

enum ChannelRecord {
    Current(Box<ChannelActorData>),
    Legacy(Box<OldChannelActorData>),
}

fn decode_exact<T: DeserializeOwned>(value: &[u8]) -> Result<T, String> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(value.len() as u64)
        .reject_trailing_bytes()
        .deserialize(value)
        .map_err(|error| error.to_string())
}

fn convert_channel_actor_data(
    old: OldChannelActorData,
    session: ChannelSessionV2,
) -> Result<ChannelActorData, String> {
    if old.commitment_contract_features.bits() != 1 {
        return Err("A V2 signing session requires V2 channel features".to_owned());
    }
    let channel = ChannelActorData {
        state: decode_as_new(old.state)?,
        public_channel_info: decode_as_new(old.public_channel_info)?,
        local_tlc_info: decode_as_new(old.local_tlc_info)?,
        remote_tlc_info: decode_as_new(old.remote_tlc_info)?,
        local_pubkey: decode_as_new(old.local_pubkey)?,
        remote_pubkey: decode_as_new(old.remote_pubkey)?,
        id: decode_as_new(old.id)?,
        funding_tx: old.funding_tx,
        funding_tx_confirmed_at: old.funding_tx_confirmed_at,
        funding_udt_type_script: old.funding_udt_type_script,
        is_acceptor: old.is_acceptor,
        is_one_way: old.is_one_way,
        to_local_amount: old.to_local_amount,
        to_remote_amount: old.to_remote_amount,
        local_reserved_ckb_amount: old.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: old.remote_reserved_ckb_amount,
        commitment_fee_rate: old.commitment_fee_rate,
        commitment_delay_epoch: old.commitment_delay_epoch,
        funding_fee_rate: old.funding_fee_rate,
        signer: Some(decode_as_new(old.signer)?),
        signing_context: Default::default(),
        local_channel_public_keys: decode_as_new(old.local_channel_public_keys)?,
        commitment_numbers: decode_as_new(old.commitment_numbers)?,
        local_constraints: decode_as_new(old.local_constraints)?,
        remote_constraints: decode_as_new(old.remote_constraints)?,
        tlc_state: convert_named(old.tlc_state)?,
        retryable_tlc_operations: convert_named(old.retryable_tlc_operations)?,
        waiting_forward_tlc_tasks: decode_as_new(old.waiting_forward_tlc_tasks)?,
        remote_shutdown_script: old.remote_shutdown_script,
        local_shutdown_script: old.local_shutdown_script,
        latest_commitment_transaction: old.latest_commitment_transaction,
        remote_commitment_points: decode_as_new(old.remote_commitment_points)?,
        remote_channel_public_keys: decode_as_new(old.remote_channel_public_keys)?,
        local_shutdown_info: decode_as_new(old.local_shutdown_info)?,
        remote_shutdown_info: decode_as_new(old.remote_shutdown_info)?,
        shutdown_transaction_hash: old.shutdown_transaction_hash,
        reestablishing: old.reestablishing,
        created_at: old.created_at,
        pending_replay_updates: decode_as_new(old.pending_replay_updates)?,
        last_was_revoke: old.last_was_revoke,
        connectivity_state: decode_as_new(old.connectivity_state)?,
        external_funding: decode_as_new(old.external_funding)?,
        channel_features: ChannelFeatures::V2,
        session,
    };
    validate_channel_v2(&channel)?;
    Ok(channel)
}

fn released_channel(
    value: &[u8],
) -> Result<(OldChannelActorData, Option<ChannelSessionV2>), String> {
    // develop appended session_v2 to rc1's unchanged binary prefix. Tuples and
    // structs have no framing bytes in bincode; published rc1 has no suffix.
    decode_exact(value)
        .or_else(|_| decode_exact::<OldChannelActorData>(value).map(|channel| (channel, None)))
}

fn channel_record(value: &[u8], version: &str) -> Result<ChannelRecord, String> {
    match decode_exact::<ChannelActorData>(value) {
        Ok(channel) => {
            validate_channel_v2(&channel)?;
            return Ok(ChannelRecord::Current(Box::new(channel)));
        }
        Err(error) if version == MIGRATION_DB_VERSION => return Err(error),
        Err(_) => {}
    }
    let (channel, session) = released_channel(value)?;
    match session {
        Some(session) => convert_channel_actor_data(channel, session)
            .map(|channel| ChannelRecord::Current(Box::new(channel))),
        None => Ok(ChannelRecord::Legacy(Box::new(channel))),
    }
}

fn legacy_closed(channel: &OldChannelActorData) -> bool {
    matches!(channel.state, OldChannelState::Closed(flags)
        if !flags.is_empty()
            && OldCloseFlags::all().contains(flags)
            && !flags.intersects(OldCloseFlags::WAITING_ONCHAIN_SETTLEMENT | OldCloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED))
        && channel.retryable_tlc_operations.is_empty()
        && channel.waiting_forward_tlc_tasks.is_empty()
}

fn actor_key(channel_id: &impl AsRef<[u8]>) -> Vec<u8> {
    [&[CHANNEL_ACTOR_STATE_PREFIX], channel_id.as_ref()].concat()
}

fn archive_key(key: &[u8]) -> Vec<u8> {
    [&[ARCHIVED_V1_PREFIX], key].concat()
}

fn archived_or_closed_channel(
    store: &dyn MigrationStore,
    channel_id: &OldHash256,
    version: &str,
) -> Result<Option<Box<OldChannelActorData>>, String> {
    let key = actor_key(channel_id);
    let record = if let Some(value) = store.get(&key) {
        channel_record(&value, version)?
    } else if let Some(value) = store.get(&archive_key(&key)) {
        let (channel, session) = released_channel(&value)?;
        if session.is_some() {
            return Err("Invalid V1 archive: contains a V2 session".to_owned());
        }
        ChannelRecord::Legacy(Box::new(channel))
    } else {
        return Ok(None);
    };
    Ok(match record {
        ChannelRecord::Legacy(channel) if channel.id == *channel_id && legacy_closed(&channel) => {
            Some(channel)
        }
        _ => None,
    })
}

fn archive(store: &dyn MigrationStore, keys: &[Vec<u8>], apply: bool) -> Result<(), String> {
    let mut puts = Vec::new();
    let mut deletes = Vec::new();
    for key in keys {
        let Some(value) = store.get(key) else {
            continue;
        };
        let target = archive_key(key);
        if store.get(&target).is_some_and(|existing| existing != value) {
            return Err("Conflicting V1 archive record; database preserved".to_owned());
        }
        puts.push((target, value));
        deletes.push(key.clone());
    }
    if apply && !puts.is_empty() {
        store.write_batch(&puts, &deletes);
    }
    Ok(())
}

fn check_id(key: &[u8], channel_id: &(impl AsRef<[u8]> + std::fmt::Display)) -> Result<(), String> {
    if key.get(1..) != Some(channel_id.as_ref()) {
        return Err(format!(
            "Channel record key does not match channel {channel_id}"
        ));
    }
    Ok(())
}

fn scan(store: &dyn MigrationStore, apply: bool) -> Result<(), String> {
    let Some(version) = store.get(MIGRATION_VERSION_KEY) else {
        return Ok(());
    };
    let version = std::str::from_utf8(&version).map_err(|_| "Invalid database version")?;
    if !(RELEASED_VERSION..=MIGRATION_DB_VERSION).contains(&version) {
        return Ok(());
    }
    if ![RELEASED_VERSION, MIGRATION_DB_VERSION].contains(&version) {
        return Err(format!(
            "Unsupported database layout version {version}; use the matching old binary first"
        ));
    }
    let upgrading = version != MIGRATION_DB_VERSION;
    let mut blocked = Vec::new();
    let mut blocked_count = 0;
    let mut block = |id: String, state: String| {
        blocked_count += 1;
        if blocked.len() < 20 {
            blocked.push(format!("{id}: {state}"));
        }
    };
    for (key, value) in store.iter_prefix(&[CHANNEL_ACTOR_STATE_PREFIX]) {
        let record = channel_record(&value, version)
            .map_err(|error| format!("Invalid channel record; database preserved: {error}"))?;
        match record {
            ChannelRecord::Current(channel) => {
                check_id(&key, &channel.id)?;
                if apply {
                    let current =
                        bincode::serialize(&channel).map_err(|error| error.to_string())?;
                    if current != value {
                        store.put(&key, &current);
                    }
                }
            }
            ChannelRecord::Legacy(channel) => {
                check_id(&key, &channel.id)?;
                if !legacy_closed(&channel) {
                    block(
                        channel.id.to_string(),
                        format!("{:?}; pending V1 channel or settlement work", channel.state),
                    );
                    continue;
                }
                let mut keys = vec![
                    key,
                    [
                        &[PUBKEY_CHANNEL_ID_PREFIX],
                        channel.remote_pubkey.serialize().as_slice(),
                        channel.id.as_ref(),
                    ]
                    .concat(),
                    [&[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX], channel.id.as_ref()].concat(),
                    [&[PENDING_COMMIT_DIFF_PREFIX], channel.id.as_ref()].concat(),
                ];
                if let Some(tx) = &channel.funding_tx {
                    let outpoint = OutPoint::new(tx.calc_tx_hash(), 0);
                    let key = [&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat();
                    if let Some(value) = store.get(&key) {
                        if decode_exact::<OldHash256>(&value)? != channel.id {
                            return Err(
                                "V1 funding outpoint index belongs to another channel".to_owned()
                            );
                        }
                    }
                    keys.push(key);
                }
                archive(store, &keys, apply)?;
            }
        }
    }
    // A missing actor is not enough: retain orphan openings backed by funding/watch data.
    let mut funded_channels = HashSet::new();
    if upgrading {
        for (_, value) in store.iter_prefix(&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX]) {
            funded_channels.insert(decode_exact::<OldHash256>(&value)?);
        }
        for prefix in [
            PUBKEY_CHANNEL_ID_PREFIX,
            WATCHTOWER_CHANNEL_PREFIX,
            WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX,
        ] {
            for (key, _) in store.iter_prefix(&[prefix]) {
                if key.len() < 33 {
                    return Err("Invalid channel/watchtower index key".to_owned());
                }
                let mut id = [0; 32];
                id.copy_from_slice(&key[key.len() - 32..]);
                funded_channels.insert(OldHash256::from(id));
            }
        }
    }
    for (key, value) in store.iter_prefix(&[CHANNEL_OPEN_RECORD_PREFIX]) {
        if !upgrading {
            let record: ChannelOpenRecord = decode_exact(&value)?;
            check_id(&key, &record.channel_id)?;
            continue;
        }
        let record: OldChannelOpenRecord = decode_exact(&value)?;
        check_id(&key, &record.channel_id)?;
        if archived_or_closed_channel(store, &record.channel_id, version)?.is_some() {
            archive(store, &[key], apply)?;
            continue;
        }
        let actor = store
            .get(&actor_key(&record.channel_id))
            .map(|bytes| channel_record(&bytes, version))
            .transpose()?;
        let unfunded_orphan = actor.is_none()
            && record.status == OldChannelOpeningStatus::WaitingForPeer
            && !funded_channels.contains(&record.channel_id)
            && [
                [
                    &[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX],
                    record.channel_id.as_ref(),
                ]
                .concat(),
                [&[PENDING_COMMIT_DIFF_PREFIX], record.channel_id.as_ref()].concat(),
            ]
            .iter()
            .all(|key| store.get(key).is_none());
        if matches!(actor, Some(ChannelRecord::Current(_))) {
            let _: ChannelOpenRecord = decode_exact(&value)?;
        } else if matches!(record.status, OldChannelOpeningStatus::Failed) || unfunded_orphan {
            archive(store, &[key], apply)?;
        } else {
            block(
                record.channel_id.to_string(),
                format!(
                    "Unfinished V1 or unverifiable opening record {:?}",
                    record.status
                ),
            );
        }
    }
    // Watchtower protection is relevant even when this binary disables its runtime feature.
    for (key, value) in store.iter_prefix(&[WATCHTOWER_CHANNEL_PREFIX]) {
        if !upgrading {
            let record: ChannelData = decode_exact(&value)?;
            if key.len() < 33 || key[key.len() - 32..] != record.channel_id.as_ref()[..] {
                return Err("Invalid watchtower channel key".to_owned());
            }
            continue;
        }
        let record: OldChannelData = decode_exact(&value)?;
        if key.len() < 33 || key[key.len() - 32..] != record.channel_id.as_ref()[..] {
            return Err("Invalid watchtower channel key".to_owned());
        }
        match record.commitment_contract_features.bits() {
            1 => {
                let converted = convert_watch_channel(record)?;
                if apply {
                    store.put(
                        &key,
                        &bincode::serialize(&converted).map_err(|error| error.to_string())?,
                    );
                }
            }
            0 => {
                if key.len() != 33 {
                    return Err(format!(
                        "Unsupported legacy third-party watchtower record for channel {}. This migration supports only local legacy watchtower entries with a safely closed matching local channel. Keep client-owned legacy protection on the original service; it cannot be automatically retired.",
                        record.channel_id
                    ));
                }
                let closed = archived_or_closed_channel(store, &record.channel_id, version)?;
                let local_closed = closed.is_some_and(|channel| {
                    channel.signer.funding_key.pubkey() == record.local_funding_pubkey
                        && channel
                            .remote_channel_public_keys
                            .as_ref()
                            .is_some_and(|remote| {
                                remote.funding_pubkey == record.remote_funding_pubkey
                            })
                });
                if !local_closed {
                    block(
                        record.channel_id.to_string(),
                        "Local V1 watchtower entry has no safely closed matching local channel and funding keys".to_owned(),
                    );
                } else {
                    archive(
                        store,
                        &[
                            key,
                            [
                                &[WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX],
                                record.channel_id.as_ref(),
                            ]
                            .concat(),
                        ],
                        apply,
                    )?;
                }
            }
            _ => return Err("Unsupported watchtower channel features".to_owned()),
        }
    }
    if upgrading {
        // Released records have no hosted execution scope. Convert fields by name
        // so adding optional local metadata cannot shift bincode's nested layout.
        for (key, value) in store.iter_prefix(&[PAYMENT_SESSION_PREFIX]) {
            let old: fiber_types_0100::PaymentSession = decode_exact(&value)?;
            let converted: fiber_types::PaymentSession = convert_named(old)?;
            if key
                != [
                    &[PAYMENT_SESSION_PREFIX],
                    converted.request.payment_hash.as_ref(),
                ]
                .concat()
            {
                return Err("Invalid released payment session key".to_owned());
            }
            if apply {
                store.put(
                    &key,
                    &bincode::serialize(&converted).map_err(|error| error.to_string())?,
                );
            }
        }
        for (key, value) in store.iter_prefix(&[ATTEMPT_PREFIX]) {
            let old: fiber_types_0100::Attempt = decode_exact(&value)?;
            let converted: fiber_types::Attempt = convert_named(old)?;
            if key
                != [
                    &[ATTEMPT_PREFIX],
                    converted.payment_hash.as_ref(),
                    &converted.id.to_le_bytes(),
                ]
                .concat()
            {
                return Err("Invalid released payment attempt key".to_owned());
            }
            if apply {
                store.put(
                    &key,
                    &bincode::serialize(&converted).map_err(|error| error.to_string())?,
                );
            }
        }
        for (key, value) in store.iter_prefix(&[WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX]) {
            let snapshots: Vec<fiber_types_0100::SettlementData> = decode_exact(&value)?;
            let converted = snapshots
                .into_iter()
                .map(convert_settlement)
                .collect::<Result<Vec<_>, _>>()?;
            if apply {
                store.put(
                    &key,
                    &bincode::serialize(&converted).map_err(|error| error.to_string())?,
                );
            }
        }
    }
    if blocked_count != 0 {
        let preserved = if apply {
            "Source records changed during migration; keep the backup and retry with this binary."
        } else {
            "No records or database version were changed."
        };
        return Err(format!("Found {blocked_count} V1 or unverifiable records that are not safely closed. Close these channels and finish pending settlement/delivery with the old binary, then retry this upgrade. {preserved}\n{}{}",
            blocked.join("\n"), if blocked_count > blocked.len() { "\n(additional records omitted)" } else { "" }));
    }
    Ok(())
}

#[cfg(test)]
#[path = "v2_only/tests.rs"]
mod tests;
