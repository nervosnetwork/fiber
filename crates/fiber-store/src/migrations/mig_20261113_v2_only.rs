//! Admit existing V2 channels without retaining a V1 runtime.
use std::collections::HashSet;

use bincode::Options;
use ckb_types::{packed::OutPoint, prelude::*};
use fiber_types::channel_v2_validation::{
    decode_and_validate_channel_actor_data_parts, validate_channel_v2,
};
use fiber_types::schema::{
    ARCHIVED_V1_PREFIX, ATTEMPT_PREFIX, CHANNEL_ACTOR_STATE_PREFIX, CHANNEL_IMMUTABLE_STATE_PREFIX,
    CHANNEL_OPEN_RECORD_PREFIX, CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX,
    CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX, PAYMENT_SESSION_PREFIX, PUBKEY_CHANNEL_ID_PREFIX,
    WATCHTOWER_CHANNEL_PREFIX, WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX,
};
use fiber_types::{
    ChannelActorData, ChannelData, ChannelFeatures, ChannelImmutableData, ChannelOpenRecord,
    ChannelSessionV2, Hash256, ShutdownSettlementRecord,
};
use fiber_types_0100::schema::PENDING_COMMIT_DIFF_PREFIX;
use fiber_types_0100::{
    ChannelActorData as OldChannelActorData, ChannelData as OldChannelData,
    ChannelOpenRecord as OldChannelOpenRecord, ChannelOpeningStatus as OldChannelOpeningStatus,
    ChannelState as OldChannelState, CloseFlags as OldCloseFlags, Hash256 as OldHash256,
    ShutdownSettlementRecord as OldShutdownSettlementRecord,
};
use serde::de::DeserializeOwned;

use crate::migration::{Migration, MigrationStore, LATEST_DB_VERSION, MIGRATION_VERSION_KEY};

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

fn convert_shutdown_settlement_record(
    old: OldShutdownSettlementRecord,
) -> Result<ShutdownSettlementRecord, String> {
    Ok(ShutdownSettlementRecord {
        shutdown_tx_hash: old.shutdown_tx_hash,
        for_remote: old.for_remote,
        commitment_number: old.commitment_number,
        settlement_data: convert_settlement(old.settlement_data)?,
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

/// Released v0.10.0-rc2 ..= v0.10.1 wrote a session marker in front of the session, which this
/// layout no longer stores. Its nested `SettlementData` and `TlcInfo` also used the released
/// v0.10.1 binary layout (untagged `SettlementTlc::local_key` and no `TlcInfo::upstream_tlc_key`).
const RELEASED_SESSION_MARKER: u32 = 0x56320001;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct OutgoingCommitmentV2Wire<S> {
    pub peer_wait_started_at: u64,
    pub request: String,
    pub transaction: String,
    pub settlement: S,
    pub updates: Vec<fiber_types::TlcReplayUpdate>,
    pub funding: fiber_types::SigningNonceV2,
    pub revocation: Option<fiber_types::SigningNonceV2>,
    pub revocation_context: Option<fiber_types::RevocationContextV2>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct IncomingCommitmentV2Wire<S> {
    pub request: String,
    pub funding: fiber_types::SigningNonceV2,
    pub revocation: Option<fiber_types::SigningNonceV2>,
    pub revocation_context: Option<fiber_types::RevocationContextV2>,
    pub transaction: String,
    pub settlement: S,
    pub response: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ChannelSessionV2Wire<S, T> {
    pub own: fiber_types::SigningNonceV2,
    pub own_retired: Vec<fiber_types::SigningNonceV2>,
    pub remote_nonce: Option<Vec<u8>>,
    pub remote_number: u64,
    pub bootstrap_remote_nonce: Option<Vec<u8>>,
    pub remote_ready: Option<String>,
    pub outgoing: Option<OutgoingCommitmentV2Wire<S>>,
    pub incoming: Option<IncomingCommitmentV2Wire<S>>,
    pub pending_incoming: Option<IncomingCommitmentV2Wire<S>>,
    pub pending_ack: Option<String>,
    pub last_ack: Option<String>,
    pub revocation_effect: Option<(fiber_types::RevocationData, S)>,
    pub bootstrap_settlement: Option<S>,
    pub remove_effects: Vec<(T, Option<u32>)>,
    pub closing: Option<fiber_types::ClosingSessionV2>,
}

pub(crate) type ReleasedChannelSessionV2Wire =
    ChannelSessionV2Wire<fiber_types_0100::SettlementData, fiber_types_0100::TlcInfo>;
pub(crate) type CurrentChannelSessionV2Wire =
    ChannelSessionV2Wire<fiber_types::SettlementData, fiber_types::TlcInfo>;

struct ReleasedSessionV2(ReleasedChannelSessionV2Wire);

impl<'de> serde::Deserialize<'de> for ReleasedSessionV2 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (marker, session) =
            <(u32, ReleasedChannelSessionV2Wire)>::deserialize(deserializer)?;
        if marker != RELEASED_SESSION_MARKER {
            return Err(serde::de::Error::custom("Invalid V2 session marker"));
        }
        Ok(Self(session))
    }
}

fn convert_outgoing_commitment(
    old: OutgoingCommitmentV2Wire<fiber_types_0100::SettlementData>,
) -> Result<OutgoingCommitmentV2Wire<fiber_types::SettlementData>, String> {
    Ok(OutgoingCommitmentV2Wire {
        peer_wait_started_at: old.peer_wait_started_at,
        request: old.request,
        transaction: old.transaction,
        settlement: convert_settlement(old.settlement)?,
        updates: old.updates,
        funding: old.funding,
        revocation: old.revocation,
        revocation_context: old.revocation_context,
    })
}

fn convert_incoming_commitment(
    old: IncomingCommitmentV2Wire<fiber_types_0100::SettlementData>,
) -> Result<IncomingCommitmentV2Wire<fiber_types::SettlementData>, String> {
    Ok(IncomingCommitmentV2Wire {
        request: old.request,
        funding: old.funding,
        revocation: old.revocation,
        revocation_context: old.revocation_context,
        transaction: old.transaction,
        settlement: convert_settlement(old.settlement)?,
        response: old.response,
    })
}

fn convert_released_session(
    old: ReleasedChannelSessionV2Wire,
) -> Result<ChannelSessionV2, String> {
    let current = CurrentChannelSessionV2Wire {
        own: old.own,
        own_retired: old.own_retired,
        remote_nonce: old.remote_nonce,
        remote_number: old.remote_number,
        bootstrap_remote_nonce: old.bootstrap_remote_nonce,
        remote_ready: old.remote_ready,
        outgoing: old.outgoing.map(convert_outgoing_commitment).transpose()?,
        incoming: old.incoming.map(convert_incoming_commitment).transpose()?,
        pending_incoming: old
            .pending_incoming
            .map(convert_incoming_commitment)
            .transpose()?,
        pending_ack: old.pending_ack,
        last_ack: old.last_ack,
        revocation_effect: old
            .revocation_effect
            .map(|(revocation, settlement)| Ok::<_, String>((revocation, convert_settlement(settlement)?)))
            .transpose()?,
        bootstrap_settlement: old.bootstrap_settlement.map(convert_settlement).transpose()?,
        remove_effects: old
            .remove_effects
            .into_iter()
            .map(|(tlc, execution)| Ok::<_, String>((convert_named(tlc)?, execution)))
            .collect::<Result<Vec<_>, _>>()?,
        closing: old.closing,
    };
    // Bincode tuples append fields without changing the released session prefix.
    decode_as_new((current, Vec::<(u64, fiber_types::SettlementData)>::new()))
}

fn released_channel(
    value: &[u8],
) -> Result<(OldChannelActorData, Option<ChannelSessionV2>), String> {
    // develop appended session_v2 to rc1's unchanged binary prefix. Tuples and
    // structs have no framing bytes in bincode; published rc1 has no suffix.
    if let Ok(channel) = decode_exact::<OldChannelActorData>(value) {
        return Ok((channel, None));
    }
    // Everything with a suffix was written by v0.10.0-rc2 ..= v0.10.1, so its error is the one
    // that explains the record (an invalid marker, a broken session) rather than "trailing bytes".
    let (channel, session) =
        decode_exact::<(OldChannelActorData, Option<ReleasedSessionV2>)>(value)?;
    let session = session
        .map(|session| convert_released_session(session.0))
        .transpose()?;
    Ok((channel, session))
}

/// Read a channel record in whichever layout the database is in: an already split pair, or a whole
/// record written before the split.
fn read_channel_record(
    store: &dyn MigrationStore,
    key: &[u8],
    version: &str,
) -> Result<Option<ChannelRecord>, String> {
    let Some(value) = store.get(key) else {
        return Ok(None);
    };
    match store.get(&immutable_key(key)?) {
        Some(immutable) => Ok(Some(ChannelRecord::Current(Box::new(
            decode_and_validate_channel_actor_data_parts(
                &stored_channel_id(key)?,
                &value,
                &immutable,
            )?,
        )))),
        None => channel_record(&value, version).map(Some),
    }
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

/// Summary of a V1 channel selected for the pre-migration archive step.
#[derive(Debug, Clone, serde::Serialize)]
pub struct V1ChannelArchiveSummary {
    /// Channel being archived.
    pub channel_id: Hash256,
    /// Debug representation of the V1 channel state.
    pub state: String,
    /// Funding transaction hash as `0x`-prefixed hex, when known.
    pub funding_tx_hash: Option<String>,
    /// Funding outpoint as `0x<hash>:<index>`, when known.
    pub funding_outpoint: Option<String>,
    /// Total number of active keys that will be archived.
    pub archived_keys: usize,
    /// Watchtower keys included in [`Self::archived_keys`].
    pub watchtower_keys: usize,
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Archive the listed V1 channels so the V2-only migration can proceed.
///
/// This is an explicit, offline pre-migration step: it does not close the funding
/// transaction and does not recover funds. Every removed key is copied under
/// `ARCHIVED_V1_PREFIX` first. The database version must still be
/// `RELEASED_VERSION`; run the normal migration afterwards.
pub fn archive_v1_channels(
    store: &dyn MigrationStore,
    channel_ids: &[Hash256],
    apply: bool,
) -> Result<Vec<V1ChannelArchiveSummary>, String> {
    let version = store
        .get(MIGRATION_VERSION_KEY)
        .ok_or_else(|| "database has no version; refusing to archive V1 channels".to_owned())?;
    let version =
        std::str::from_utf8(&version).map_err(|_| "Invalid database version".to_owned())?;
    if version != RELEASED_VERSION {
        return Err(format!(
            "V1 archive must run before migration: expected database version {RELEASED_VERSION}, found {version}"
        ));
    }

    let mut all_keys: Vec<Vec<u8>> = Vec::new();
    let mut seen = HashSet::new();
    let mut summaries = Vec::with_capacity(channel_ids.len());

    for channel_id in channel_ids {
        let actor = actor_key(channel_id);
        let Some(record) = read_channel_record(store, &actor, version)? else {
            return Err(format!("V1 channel {channel_id} not found"));
        };
        let ChannelRecord::Legacy(channel) = record else {
            return Err(format!(
                "Channel {channel_id} is not a V1 record; only V1 channels can be archived"
            ));
        };
        if channel.id.as_ref() != channel_id.as_ref() {
            return Err(format!(
                "Channel record key does not match channel {channel_id}"
            ));
        }
        if legacy_closed(&channel) {
            return Err(format!(
                "Channel {channel_id} is already safely closed; let the normal migration archive it"
            ));
        }

        let mut keys = Vec::new();
        let mut watchtower_keys = 0;
        let mut add_key = |key: Vec<u8>| {
            if store.get(&key).is_some() && seen.insert(key.clone()) {
                keys.push(key);
            }
        };

        add_key(actor.clone());
        add_key(
            [
                &[PUBKEY_CHANNEL_ID_PREFIX],
                channel.remote_pubkey.serialize().as_slice(),
                channel.id.as_ref(),
            ]
            .concat(),
        );
        add_key([&[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX], channel.id.as_ref()].concat());
        add_key([&[PENDING_COMMIT_DIFF_PREFIX], channel.id.as_ref()].concat());
        add_key([&[CHANNEL_OPEN_RECORD_PREFIX], channel.id.as_ref()].concat());
        let funding_outpoint = channel.funding_tx.as_ref().map(|tx| {
            let hash = tx.calc_tx_hash();
            let outpoint = OutPoint::new(hash.clone(), 0);
            let key = [&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat();
            (hash, outpoint, key)
        });
        if let Some((_, _, key)) = &funding_outpoint {
            if let Some(value) = store.get(key) {
                let stored: OldHash256 = decode_exact(&value)?;
                if stored != channel.id {
                    return Err(format!(
                        "Funding outpoint index for channel {channel_id} points to another channel"
                    ));
                }
                add_key(key.clone());
            }
        }

        for prefix in [
            WATCHTOWER_CHANNEL_PREFIX,
            WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX,
        ] {
            // Only the local watchtower row has the released `[prefix, channel_id]`
            // shape. Client-owned rows add a node id and must stay with the original
            // watchtower service.
            let key = [&[prefix], channel.id.as_ref()].concat();
            if store.get(&key).is_some() && seen.insert(key.clone()) {
                keys.push(key);
                watchtower_keys += 1;
            }
        }

        let archived_keys = keys.len();
        all_keys.extend(keys);
        summaries.push(V1ChannelArchiveSummary {
            channel_id: *channel_id,
            state: format!("{:?}", channel.state),
            funding_tx_hash: funding_outpoint
                .as_ref()
                .map(|(hash, _, _)| format!("0x{}", hex_bytes(hash.as_slice()))),
            funding_outpoint: funding_outpoint.as_ref().map(|(hash, outpoint, _)| {
                let index: u32 = outpoint.index().unpack();
                format!("0x{}:{}", hex_bytes(hash.as_slice()), index)
            }),
            archived_keys,
            watchtower_keys,
        });
    }

    archive(store, &all_keys, apply)?;
    Ok(summaries)
}

fn immutable_key(key: &[u8]) -> Result<Vec<u8>, String> {
    let id = key
        .get(1..)
        .ok_or_else(|| format!("Invalid channel key {key:?}"))?;
    Ok([&[CHANNEL_IMMUTABLE_STATE_PREFIX], id].concat())
}

fn stored_channel_id(key: &[u8]) -> Result<Hash256, String> {
    let bytes: [u8; 32] = key
        .get(1..)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| format!("Invalid channel key {key:?}"))?;
    Ok(bytes.into())
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
    let record = if let Some(record) = read_channel_record(store, &key, version)? {
        record
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
    // Later migrations keep channel records in this layout, so a database at a later known
    // version is still validated as the current epoch on every open.
    let version = if (MIGRATION_DB_VERSION..=LATEST_DB_VERSION).contains(&version) {
        MIGRATION_DB_VERSION
    } else {
        version
    };
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
    let mut mutable_records: HashSet<Hash256> = HashSet::new();
    let mut retired_v1_channels: HashSet<Hash256> = HashSet::new();
    for (key, value) in store.iter_prefix(&[CHANNEL_ACTOR_STATE_PREFIX]) {
        let immutable_key = immutable_key(&key)?;
        // Remembered for the reverse check below, without reading the mutable record again.
        if let Ok(id) = stored_channel_id(&key) {
            mutable_records.insert(id);
        }
        if !upgrading {
            let id = stored_channel_id(&key)?;
            // This database is already split, so prefix 0 is a mutable record and its fixed half
            // has to be there: a channel is only complete with both records.
            let Some(immutable) = store.get(&immutable_key) else {
                return Err(format!(
                    "Channel {id}: mutable record without its fixed record"
                ));
            };
            decode_and_validate_channel_actor_data_parts(&id, &value, &immutable)
                .map_err(|error| format!("Invalid split channel record {id}: {error}"))?;
            continue;
        }
        if let Some(immutable) = store.get(&immutable_key) {
            // Already converted by an interrupted run: both blocks must still validate.
            let id = stored_channel_id(&key)?;
            decode_and_validate_channel_actor_data_parts(&id, &value, &immutable)
                .map_err(|error| format!("Invalid split channel record {id}: {error}"))?;
            continue;
        }
        let record = channel_record(&value, version)
            .map_err(|error| format!("Invalid channel record; database preserved: {error}"))?;
        match record {
            ChannelRecord::Current(channel) => {
                check_id(&key, &channel.id)?;
                if apply {
                    // The fixed fields move to their own record, written in the same batch.
                    let (immutable, mutable) = channel.into_parts();
                    store.write_batch(
                        &[
                            (
                                immutable_key,
                                bincode::serialize(&immutable)
                                    .map_err(|error| error.to_string())?,
                            ),
                            (
                                key,
                                bincode::serialize(&mutable).map_err(|error| error.to_string())?,
                            ),
                        ],
                        &[],
                    );
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
                let id = stored_channel_id(&key)?;
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
                retired_v1_channels.insert(id);
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
    // A fixed record nobody can read a channel from, or one that does not decode, never reaches a
    // getter (they iterate prefix 0), so check the other direction here.
    for (key, value) in store.iter_prefix(&[CHANNEL_IMMUTABLE_STATE_PREFIX]) {
        let id = stored_channel_id(&key)?;
        let _: ChannelImmutableData = decode_exact(&value)
            .map_err(|error| format!("Invalid fixed record of channel {id}: {error}"))?;
        if !mutable_records.contains(&id) {
            return Err(format!(
                "Channel {id}: fixed record without its mutable record"
            ));
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
        // The prefix-0 loop above may already have split this channel's record, so read it in
        // whichever layout it is in now.
        let actor = read_channel_record(store, &actor_key(&record.channel_id), version)?;
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
        let record: OldChannelData = match decode_exact(&value) {
            Ok(old) => old,
            Err(old_error) => match decode_exact::<ChannelData>(&value) {
                Ok(current) => {
                    // Already converted by an interrupted run before db-version advanced.
                    if key.len() < 33 || key[key.len() - 32..] != current.channel_id.as_ref()[..] {
                        return Err("Invalid watchtower channel key".to_owned());
                    }
                    if current.channel_features != ChannelFeatures::V2 {
                        return Err("Unsupported watchtower channel features".to_owned());
                    }
                    continue;
                }
                Err(_) => return Err(old_error),
            },
        };
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
            let old: fiber_types_0100::PaymentSession = match decode_exact(&value) {
                Ok(old) => old,
                Err(old_error) => match decode_exact::<fiber_types::PaymentSession>(&value) {
                    Ok(current) => {
                        if current.request.upstream_tlc_key.is_some()
                            || key
                                != [
                                    &[PAYMENT_SESSION_PREFIX],
                                    current.request.payment_hash.as_ref(),
                                ]
                                .concat()
                        {
                            return Err("Invalid released payment session key".to_owned());
                        }
                        continue;
                    }
                    Err(_) => return Err(old_error),
                },
            };
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
            let old: fiber_types_0100::Attempt = match decode_exact(&value) {
                Ok(old) => old,
                Err(old_error) => match decode_exact::<fiber_types::Attempt>(&value) {
                    Ok(current) => {
                        if current.upstream_tlc_key.is_some()
                            || key
                                != [
                                    &[ATTEMPT_PREFIX],
                                    current.payment_hash.as_ref(),
                                    &current.id.to_le_bytes(),
                                ]
                                .concat()
                        {
                            return Err("Invalid released payment attempt key".to_owned());
                        }
                        continue;
                    }
                    Err(_) => return Err(old_error),
                },
            };
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
            let snapshots: Vec<fiber_types_0100::SettlementData> = match decode_exact(&value) {
                Ok(old) => old,
                Err(old_error) => match decode_exact::<Vec<fiber_types::SettlementData>>(&value) {
                    Ok(_) => continue,
                    Err(_) => return Err(old_error),
                },
            };
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
    for (key, value) in store.iter_prefix(&[CHANNEL_SHUTDOWN_SETTLEMENT_PREFIX]) {
        let id = stored_channel_id(&key)?;
        if !upgrading {
            let _: ShutdownSettlementRecord = decode_exact(&value)
                .map_err(|error| format!("Invalid shutdown settlement record {id}: {error}"))?;
            continue;
        }
        if retired_v1_channels.contains(&id) {
            // During preflight (apply == false), the closed V1 channel's prefix-49 record is
            // still present and will be archived verbatim when apply == true.
            continue;
        }
        let old_id: OldHash256 = decode_as_new(id)?;
        if archived_or_closed_channel(store, &old_id, version)?.is_some() {
            archive(store, &[key], apply)?;
            continue;
        }
        let old: OldShutdownSettlementRecord = match decode_exact(&value) {
            Ok(old) => old,
            Err(old_error) => match decode_exact::<ShutdownSettlementRecord>(&value) {
                Ok(current) => {
                    // Already converted by an interrupted run before db-version advanced.
                    if current
                        .settlement_data
                        .tlcs
                        .iter()
                        .any(|tlc| tlc.local_key.is_none() && tlc.local_key_pubkey.is_none())
                    {
                        return Err(format!(
                            "Invalid shutdown settlement record {id}: TLC missing local key"
                        ));
                    }
                    continue;
                }
                Err(_) => {
                    return Err(format!(
                        "Invalid shutdown settlement record {id}: {old_error}"
                    ));
                }
            },
        };
        let converted = convert_shutdown_settlement_record(old)?;
        if apply {
            let encoded = bincode::serialize(&converted).map_err(|error| error.to_string())?;
            if encoded != value {
                store.put(&key, &encoded);
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
