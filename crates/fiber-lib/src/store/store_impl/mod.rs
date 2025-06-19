#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{Batch, DbDirection, IteratorMode, Store};

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
pub use browser::{Batch, DbDirection, IteratorMode, Store};

use std::path::Path;

use super::db_migrate::DbMigrate;
use super::schema::*;
use crate::fiber::types::CURSOR_SIZE;
use crate::fiber::{gossip::GossipMessageStore, graph::HoldTlc};
#[cfg(feature = "watchtower")]
use crate::{
    fiber::channel::{RevocationData, SettlementData},
    watchtower::{ChannelData, WatchtowerStore},
};
use crate::{
    fiber::{
        channel::{ChannelActorState, ChannelActorStateStore, ChannelState},
        graph::{Attempt, NetworkGraphStateStore, PaymentSession, PaymentSessionStatus},
        history::{Direction, TimedResult},
        network::{NetworkActorStateStore, PaymentCustomRecords, PersistentNetworkActorState},
        types::{BroadcastMessage, BroadcastMessageID, Cursor, Hash256},
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceError, InvoiceStore, PreimageStore},
};
use ckb_types::packed::OutPoint;
#[cfg(feature = "watchtower")]
use ckb_types::packed::Script;
use ckb_types::prelude::Entity;

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tentacle::secio::PeerId;
use tracing::{info, warn};

#[derive(Copy, Clone)]
enum ChannelTimestamp {
    ChannelAnnouncement(),
    ChannelUpdateOfNode1(),
    ChannelUpdateOfNode2(),
}

pub(crate) fn serialize_to_vec<T: ?Sized + Serialize>(value: &T, field_name: &str) -> Vec<u8> {
    bincode::serialize(value)
        .unwrap_or_else(|e| panic!("serialization of {} failed: {}", field_name, e))
}

pub(crate) fn deserialize_from<'a, T>(slice: &'a [u8], field_name: &str) -> T
where
    T: serde::Deserialize<'a>,
{
    bincode::deserialize(slice)
        .unwrap_or_else(|e| panic!("deserialization of {} failed: {}", field_name, e))
}

impl Store {
    /// Open or create a rocksdb
    fn check_migrate<P: AsRef<Path>>(path: P, db: &Self) -> Result<(), String> {
        let migrate = DbMigrate::new(db);
        migrate.init_or_check(path)?;
        Ok(())
    }
    pub fn check_validate<P: AsRef<Path>>(path: P) -> Result<(), String> {
        let db = Self::open_db(path.as_ref())?;
        let mut errors = HashSet::new();

        fn check_deserialization<T: serde::de::DeserializeOwned>(
            value: &[u8],
            prefix_name: &str,
            errors: &mut HashSet<String>,
        ) {
            if let Err(e) = bincode::deserialize::<T>(value) {
                errors.insert(format!("Failed to deserialize {}: {:?}", prefix_name, e));
            }
        }

        for (key, value) in db.prefix_iterator_with_skip_while_and_start(
            &[],
            IteratorMode::Start,
            Box::new(|_| false),
        ) {
            if key.is_empty() {
                errors.insert("Encountered empty key".to_string());
                continue;
            }

            match key[0] {
                CHANNEL_ACTOR_STATE_PREFIX => {
                    check_deserialization::<ChannelActorState>(
                        &value,
                        "CHANNEL_ACTOR_STATE_PREFIX",
                        &mut errors,
                    );
                }
                PEER_ID_NETWORK_ACTOR_STATE_PREFIX => {
                    check_deserialization::<PersistentNetworkActorState>(
                        &value,
                        "PEER_ID_NETWORK_ACTOR_STATE_PREFIX",
                        &mut errors,
                    );
                }
                CKB_INVOICE_PREFIX => {
                    check_deserialization::<CkbInvoice>(&value, "CKB_INVOICE_PREFIX", &mut errors);
                }
                PREIMAGE_PREFIX => {
                    check_deserialization::<Hash256>(&value, "PREIMAGE_PREFIX", &mut errors);
                }
                CKB_INVOICE_STATUS_PREFIX => {
                    check_deserialization::<CkbInvoiceStatus>(
                        &value,
                        "CKB_INVOICE_STATUS_PREFIX",
                        &mut errors,
                    );
                }
                PEER_ID_CHANNEL_ID_PREFIX => {}
                CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX => {
                    check_deserialization::<Hash256>(
                        &value,
                        "CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX",
                        &mut errors,
                    );
                }
                BROADCAST_MESSAGE_PREFIX => {
                    check_deserialization::<BroadcastMessage>(
                        &value,
                        "BROADCAST_MESSAGE_PREFIX",
                        &mut errors,
                    );
                }
                BROADCAST_MESSAGE_TIMESTAMP_PREFIX => {}
                PAYMENT_SESSION_PREFIX => {
                    check_deserialization::<PaymentSession>(
                        &value,
                        "PAYMENT_SESSION_PREFIX",
                        &mut errors,
                    );
                }
                PAYMENT_HISTORY_TIMED_RESULT_PREFIX => {
                    check_deserialization::<TimedResult>(
                        &value,
                        "PAYMENT_HISTORY_TIMED_RESULT_PREFIX",
                        &mut errors,
                    );
                }
                PAYMENT_CUSTOM_RECORD_PREFIX => {
                    check_deserialization::<PaymentCustomRecords>(
                        &value,
                        "PAYMENT_CUSTOM_RECORD_PREFIX",
                        &mut errors,
                    );
                }
                #[cfg(feature = "watchtower")]
                WATCHTOWER_CHANNEL_PREFIX => {
                    check_deserialization::<ChannelData>(
                        &value,
                        "WATCHTOWER_CHANNEL_PREFIX",
                        &mut errors,
                    );
                }
                _ => {}
            }
        }

        let mut errors: Vec<String> = errors.into_iter().collect();
        if let Err(version_err) = Self::check_migrate(path, &db) {
            errors.push(version_err);
        }
        if errors.is_empty() {
            info!("All keys and values in the store are valid.");
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }
}

pub enum KeyValue {
    ChannelActorState(Hash256, ChannelActorState),
    CkbInvoice(Hash256, CkbInvoice),
    Preimage(Hash256, Hash256),
    CkbInvoiceStatus(Hash256, CkbInvoiceStatus),
    PeerIdChannelId((PeerId, Hash256), ChannelState),
    OutPointChannelId(OutPoint, Hash256),
    BroadcastMessageTimestamp(BroadcastMessageID, u64),
    BroadcastMessage(Cursor, BroadcastMessage),
    #[cfg(feature = "watchtower")]
    WatchtowerChannel(Hash256, ChannelData),
    PaymentSession(Hash256, PaymentSession),
    PaymentHistoryTimedResult((OutPoint, Direction), TimedResult),
    PaymentCustomRecord(Hash256, PaymentCustomRecords),
    NetworkActorState(PeerId, PersistentNetworkActorState),
    Attempt((Hash256, u64), Attempt),
    NextAttemptId(u64),
    HoldTlcs(Hash256, Vec<HoldTlc>),
}

pub trait StoreKeyValue {
    fn key(&self) -> Vec<u8>;
    fn value(&self) -> Vec<u8>;
}

impl StoreKeyValue for KeyValue {
    fn key(&self) -> Vec<u8> {
        match self {
            KeyValue::ChannelActorState(id, _) => {
                [&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat()
            }
            KeyValue::CkbInvoice(id, _) => [&[CKB_INVOICE_PREFIX], id.as_ref()].concat(),
            KeyValue::Preimage(id, _) => [&[PREIMAGE_PREFIX], id.as_ref()].concat(),
            KeyValue::CkbInvoiceStatus(id, _) => {
                [&[CKB_INVOICE_STATUS_PREFIX], id.as_ref()].concat()
            }
            KeyValue::PeerIdChannelId((peer_id, channel_id), _) => [
                &[PEER_ID_CHANNEL_ID_PREFIX],
                peer_id.as_bytes(),
                channel_id.as_ref(),
            ]
            .concat(),
            KeyValue::OutPointChannelId(outpoint, _) => {
                [&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat()
            }
            KeyValue::PaymentSession(payment_hash, _) => {
                [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat()
            }
            KeyValue::Attempt((payment_hash, attempt_id), _) => [
                &[ATTEMPT_PREFIX],
                payment_hash.as_ref(),
                &attempt_id.to_le_bytes(),
            ]
            .concat(),
            KeyValue::NextAttemptId(_id) => vec![NEXT_ATTEMPT_ID],
            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerChannel(channel_id, _) => {
                [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat()
            }
            KeyValue::NetworkActorState(peer_id, _) => {
                [&[PEER_ID_NETWORK_ACTOR_STATE_PREFIX], peer_id.as_bytes()].concat()
            }
            KeyValue::PaymentHistoryTimedResult((channel_outpoint, direction), _) => [
                &[PAYMENT_HISTORY_TIMED_RESULT_PREFIX],
                channel_outpoint.as_slice(),
                serialize_to_vec(direction, "Direction").as_slice(),
            ]
            .concat(),
            KeyValue::BroadcastMessageTimestamp(broadcast_message_id, _) => [
                &[BROADCAST_MESSAGE_TIMESTAMP_PREFIX],
                broadcast_message_id.to_bytes().as_slice(),
            ]
            .concat(),
            KeyValue::BroadcastMessage(cursor, _) => {
                [&[BROADCAST_MESSAGE_PREFIX], cursor.to_bytes().as_slice()].concat()
            }
            KeyValue::PaymentCustomRecord(payment_hash, _data) => {
                [&[PAYMENT_CUSTOM_RECORD_PREFIX], payment_hash.as_ref()].concat()
            }
            KeyValue::HoldTlcs(payment_hash, _hold_tlc) => {
                [&[HOLD_TLC_PREFIX], payment_hash.as_ref()].concat()
            }
        }
    }

    fn value(&self) -> Vec<u8> {
        match self {
            KeyValue::ChannelActorState(_, state) => serialize_to_vec(state, "ChannelActorState"),
            KeyValue::CkbInvoice(_, invoice) => serialize_to_vec(invoice, "CkbInvoice"),
            KeyValue::Preimage(_, preimage) => serialize_to_vec(preimage, "Hash256"),
            KeyValue::CkbInvoiceStatus(_, status) => serialize_to_vec(status, "CkbInvoiceStatus"),
            KeyValue::PeerIdChannelId(_, state) => serialize_to_vec(state, "ChannelState"),
            KeyValue::OutPointChannelId(_, channel_id) => serialize_to_vec(channel_id, "ChannelId"),
            KeyValue::PaymentSession(_, payment_session) => {
                serialize_to_vec(payment_session, "PaymentSession")
            }
            KeyValue::Attempt(_, attempt) => serialize_to_vec(attempt, "Attempt"),
            KeyValue::NextAttemptId(id) => serialize_to_vec(&id, "u64"),
            #[cfg(feature = "watchtower")]
            KeyValue::WatchtowerChannel(_, channel_data) => {
                serialize_to_vec(channel_data, "ChannelData")
            }
            KeyValue::NetworkActorState(_, persistent_network_actor_state) => serialize_to_vec(
                persistent_network_actor_state,
                "PersistentNetworkActorState",
            ),
            KeyValue::BroadcastMessageTimestamp(_, value) => value.to_be_bytes().into(),
            KeyValue::BroadcastMessage(_cursor, broadcast_message) => {
                serialize_to_vec(broadcast_message, "BroadcastMessage")
            }
            KeyValue::PaymentHistoryTimedResult(_, result) => {
                serialize_to_vec(result, "TimedResult")
            }
            KeyValue::PaymentCustomRecord(_, custom_records) => {
                serialize_to_vec(custom_records, "PaymentCustomRecord")
            }
            KeyValue::HoldTlcs(_payment_hash, hold_tlc) => serialize_to_vec(hold_tlc, "HoldTlc"),
        }
    }
}

impl NetworkActorStateStore for Store {
    fn get_network_actor_state(&self, id: &PeerId) -> Option<PersistentNetworkActorState> {
        let key = [&[PEER_ID_NETWORK_ACTOR_STATE_PREFIX], id.as_bytes()].concat();
        self.get(key)
            .map(|value| deserialize_from(value.as_ref(), "PersistentNetworkActorState"))
    }

    fn insert_network_actor_state(&self, id: &PeerId, state: PersistentNetworkActorState) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::NetworkActorState(id.clone(), state));
        batch.commit();
    }
}

impl ChannelActorStateStore for Store {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState> {
        let key = [&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "ChannelActorState"))
    }

    fn insert_channel_actor_state(&self, state: ChannelActorState) {
        let mut batch = self.batch();

        batch.put_kv(KeyValue::PeerIdChannelId(
            (state.get_remote_peer_id(), state.id),
            state.state,
        ));
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            batch.put_kv(KeyValue::OutPointChannelId(outpoint, state.id));
        }
        batch.put_kv(KeyValue::ChannelActorState(state.id, state));
        batch.commit();
    }

    fn delete_channel_actor_state(&self, id: &Hash256) {
        if let Some(state) = self.get_channel_actor_state(id) {
            let mut batch = self.batch();
            batch.delete([&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat());
            batch.delete(
                [
                    &[PEER_ID_CHANNEL_ID_PREFIX],
                    state.get_remote_peer_id().as_bytes(),
                    id.as_ref(),
                ]
                .concat(),
            );
            if let Some(outpoint) = state.get_funding_transaction_outpoint() {
                batch.delete([&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat());
            }
            batch.commit();
        }
    }

    fn get_channel_ids_by_peer(&self, peer_id: &tentacle::secio::PeerId) -> Vec<Hash256> {
        let prefix = [&[PEER_ID_CHANNEL_ID_PREFIX], peer_id.as_bytes()].concat();
        let iter = self.prefix_iterator(&prefix);
        iter.map(|(key, _)| {
            let channel_id: [u8; 32] = key[prefix.len()..]
                .try_into()
                .expect("channel id should be 32 bytes");
            channel_id.into()
        })
        .collect()
    }

    fn get_channel_states(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Hash256, ChannelState)> {
        let prefix = match peer_id {
            Some(peer_id) => [&[PEER_ID_CHANNEL_ID_PREFIX], peer_id.as_bytes()].concat(),
            None => vec![PEER_ID_CHANNEL_ID_PREFIX],
        };
        self.prefix_iterator(&prefix)
            .map(|(key, value)| {
                let key_len = key.len();
                let peer_id = PeerId::from_bytes(key[1..key_len - 32].into())
                    .expect("deserialize peer id should be OK");
                let channel_id: [u8; 32] = key[key_len - 32..]
                    .try_into()
                    .expect("channel id should be 32 bytes");
                let state = deserialize_from(value.as_ref(), "ChannelState");
                (peer_id, channel_id.into(), state)
            })
            .collect()
    }

    fn get_channel_state_by_outpoint(&self, outpoint: &OutPoint) -> Option<ChannelActorState> {
        let key = [&[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX], outpoint.as_slice()].concat();
        self.get(key)
            .map(|channel_id| deserialize_from(channel_id.as_ref(), "Hash256"))
            .and_then(|channel_id: Hash256| self.get_channel_actor_state(&channel_id))
    }

    fn insert_payment_custom_records(
        &self,
        payment_hash: &Hash256,
        custom_records: PaymentCustomRecords,
    ) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::PaymentCustomRecord(*payment_hash, custom_records));
        batch.commit();
    }

    fn get_payment_custom_records(&self, payment_hash: &Hash256) -> Option<PaymentCustomRecords> {
        let key = [&[PAYMENT_CUSTOM_RECORD_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "PaymentCustomRecord"))
    }

    fn insert_hold_tlc(&self, payment_hash: Hash256, hold_tlc: HoldTlc) {
        let prefix = [&[HOLD_TLC_PREFIX], payment_hash.as_ref()].concat();
        let mut batch = self.batch();
        let mut hold_tlcs: Vec<HoldTlc> = batch
            .get(&prefix)
            .map(|v| deserialize_from(v.as_ref(), "HoldTlc"))
            .unwrap_or_default();
        hold_tlcs.push(hold_tlc);
        batch.put_kv(KeyValue::HoldTlcs(payment_hash, hold_tlcs));
        batch.commit();
    }

    fn remove_hold_tlc(&self, payment_hash: &Hash256, channel_id: &Hash256, tlc_id: u64) {
        let prefix = [&[HOLD_TLC_PREFIX], payment_hash.as_ref()].concat();
        let mut batch = self.batch();
        let mut hold_tlcs: Vec<HoldTlc> = batch
            .get(&prefix)
            .map(|v| deserialize_from(v.as_ref(), "HoldTlc"))
            .unwrap_or_default();
        hold_tlcs.retain(|hold_tlc| {
            let removed =
                hold_tlc.channel_actor_state_id == *channel_id && hold_tlc.tlc_id == tlc_id;
            !removed
        });
        batch.put_kv(KeyValue::HoldTlcs(*payment_hash, hold_tlcs));
        batch.commit();
    }

    fn get_hold_tlc_set(&self, payment_hash: Hash256) -> Vec<HoldTlc> {
        let prefix = [&[HOLD_TLC_PREFIX], payment_hash.as_ref()].concat();
        self.get(&prefix)
            .map(|v| deserialize_from(v.as_ref(), "HoldTlc"))
            .unwrap_or_default()
    }

    fn remove_hold_tlc_set(&self, payment_hash: &Hash256) {
        let prefix = [&[HOLD_TLC_PREFIX], payment_hash.as_ref()].concat();
        let mut batch = self.batch();
        for (key, _) in self.prefix_iterator(&prefix) {
            batch.delete(key);
        }
        batch.commit();
    }

    fn list_all_hold_tlcs(&self) -> HashMap<Hash256, Vec<HoldTlc>> {
        let prefix = [HOLD_TLC_PREFIX];
        self.prefix_iterator(&prefix)
            .filter_map(|(key, value)| {
                if key.len() != 33 {
                    warn!("invalid hold tlc key: {}", key.len());
                    return None;
                }
                let payment_hash: [u8; 32] = key[1..33]
                    .try_into()
                    .expect("payment_hash should be 32 bytes");
                let hold_tlcs: Vec<HoldTlc> = deserialize_from(value.as_ref(), "HoldTlc");
                Some((payment_hash.into(), hold_tlcs))
            })
            .collect()
    }
}

impl InvoiceStore for Store {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
        let key = [&[CKB_INVOICE_PREFIX], id.as_ref()].concat();
        self.get(key).map(|v| deserialize_from(&v, "CkbInvoice"))
    }

    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let payment_hash = *invoice.payment_hash();
        if self.get_invoice(&payment_hash).is_some() {
            return Err(InvoiceError::DuplicatedInvoice(payment_hash.to_string()));
        }

        let mut batch = self.batch();
        batch.put_kv(KeyValue::CkbInvoice(payment_hash, invoice));
        batch.put_kv(KeyValue::CkbInvoiceStatus(
            payment_hash,
            CkbInvoiceStatus::Open,
        ));
        if let Some(preimage) = preimage {
            batch.put_kv(KeyValue::Preimage(payment_hash, preimage));
        }
        batch.commit();
        return Ok(());
    }

    fn update_invoice_status(
        &self,
        id: &Hash256,
        status: crate::invoice::CkbInvoiceStatus,
    ) -> Result<(), InvoiceError> {
        self.get_invoice(id).ok_or(InvoiceError::InvoiceNotFound)?;
        let mut batch = self.batch();
        batch.put_kv(KeyValue::CkbInvoiceStatus(*id, status));
        batch.commit();
        Ok(())
    }

    fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus> {
        let key = [&[CKB_INVOICE_STATUS_PREFIX], id.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "CkbInvoiceStatus"))
    }
}

impl PreimageStore for Store {
    fn insert_preimage(&self, payment_hash: Hash256, preimage: Hash256) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::Preimage(payment_hash, preimage));
        batch.commit();
    }

    fn remove_preimage(&self, payment_hash: &Hash256) {
        let mut batch = self.batch();
        batch.delete([&[PREIMAGE_PREFIX], payment_hash.as_ref()].concat());
        batch.commit();
    }

    fn get_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        let key = [&[PREIMAGE_PREFIX], payment_hash.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "Preimage"))
    }

    fn search_preimage(&self, payment_hash_prefix: &[u8]) -> Option<Hash256> {
        let prefix = [&[PREIMAGE_PREFIX], payment_hash_prefix].concat();
        let mut iter = self.prefix_iterator(prefix.as_slice());
        iter.next()
            .map(|(_key, value)| deserialize_from(value.as_ref(), "Preimage"))
    }
}

impl NetworkGraphStateStore for Store {
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession> {
        let prefix = [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat();
        self.get(prefix)
            .map(|v| deserialize_from(v.as_ref(), "PaymentSession"))
            .map(|session: PaymentSession| session.init_attempts(self))
    }

    fn get_payment_sessions_with_status(
        &self,
        status: PaymentSessionStatus,
    ) -> Vec<PaymentSession> {
        let prefix = [PAYMENT_SESSION_PREFIX];
        self.prefix_iterator(&prefix)
            .filter_map(|(_key, value)| {
                let session: PaymentSession = deserialize_from(value.as_ref(), "PaymentSession");
                if session.status == status {
                    Some(session.init_attempts(self))
                } else {
                    None
                }
            })
            .collect()
    }

    fn insert_payment_session(&self, session: PaymentSession) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::PaymentSession(session.payment_hash(), session));
        batch.commit();
    }

    fn next_attempt_id(&self) -> u64 {
        let mut batch = self.batch();
        let next_id = batch
            .get([NEXT_ATTEMPT_ID])
            .map(|v| deserialize_from(v.as_ref(), "u64"))
            .unwrap_or(1);
        batch.put_kv(KeyValue::NextAttemptId(next_id + 1));
        batch.commit();
        next_id
    }

    fn get_attempt(&self, payment_hash: Hash256, attempt_id: u64) -> Option<Attempt> {
        let key = [
            &[ATTEMPT_PREFIX],
            payment_hash.as_ref(),
            &attempt_id.to_le_bytes(),
        ]
        .concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "Attempt"))
    }

    fn insert_attempt(&self, attempt: Attempt) {
        assert_ne!(attempt.id, 0, "Attempt ID should not be zero");
        let mut batch = self.batch();
        batch.put_kv(KeyValue::Attempt(
            (attempt.payment_hash, attempt.id),
            attempt,
        ));
        batch.commit();
    }

    fn get_attempts(&self, payment_hash: Hash256) -> Vec<Attempt> {
        let prefix = [&[ATTEMPT_PREFIX], payment_hash.as_ref()].concat();
        self.prefix_iterator(&prefix)
            .map(|(_key, value)| deserialize_from(value.as_ref(), "Attempt"))
            .collect()
    }

    fn get_attempts_with_status(&self, status: PaymentSessionStatus) -> Vec<Attempt> {
        let prefix = [ATTEMPT_PREFIX];
        self.prefix_iterator(&prefix)
            .filter_map(|(_key, value)| {
                let attempt: Attempt = deserialize_from(value.as_ref(), "Attempt");
                if attempt.status == status {
                    Some(attempt)
                } else {
                    None
                }
            })
            .collect()
    }

    fn insert_payment_history_result(
        &mut self,
        channel_outpoint: OutPoint,
        direction: Direction,
        result: TimedResult,
    ) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::PaymentHistoryTimedResult(
            (channel_outpoint, direction),
            result,
        ));
        batch.commit();
    }

    fn get_payment_history_results(&self) -> Vec<(OutPoint, Direction, TimedResult)> {
        let prefix = vec![PAYMENT_HISTORY_TIMED_RESULT_PREFIX];
        let iter = self.prefix_iterator(&prefix);
        iter.map(|(key, value)| {
            let channel_outpoint: OutPoint =
                OutPoint::from_slice(&key[1..=36]).expect("deserialize OutPoint should be OK");
            let direction = deserialize_from(&key[37..], "Direction");
            let result = deserialize_from(value.as_ref(), "TimedResult");
            (channel_outpoint, direction, result)
        })
        .collect()
    }
}

#[cfg(feature = "watchtower")]
impl WatchtowerStore for Store {
    fn get_watch_channels(&self) -> Vec<ChannelData> {
        let prefix = vec![WATCHTOWER_CHANNEL_PREFIX];
        self.prefix_iterator(&prefix)
            .map(|(_key, value)| deserialize_from(value.as_ref(), "ChannelData"))
            .collect()
    }

    fn insert_watch_channel(
        &self,
        channel_id: Hash256,
        funding_tx_lock: Script,
        remote_settlement_data: SettlementData,
    ) {
        let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
        let value = serialize_to_vec(
            &ChannelData {
                channel_id,
                funding_tx_lock,
                remote_settlement_data,
                local_settlement_data: None,
                revocation_data: None,
            },
            "ChannelData",
        );
        let mut batch = self.batch();
        batch.put(key, value);
        batch.commit();
    }

    fn remove_watch_channel(&self, channel_id: Hash256) {
        let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
        self.delete(key);
    }

    fn update_revocation(
        &self,
        channel_id: Hash256,
        revocation_data: RevocationData,
        remote_settlement_data: SettlementData,
    ) {
        let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
        if let Some(mut channel_data) = self
            .get(key)
            .map(|v| deserialize_from::<ChannelData>(v.as_ref(), "ChannelData"))
        {
            channel_data.remote_settlement_data = remote_settlement_data;
            channel_data.revocation_data = Some(revocation_data);
            let mut batch = self.batch();
            batch.put_kv(KeyValue::WatchtowerChannel(channel_id, channel_data));
            batch.commit();
        }
    }

    fn update_local_settlement(&self, channel_id: Hash256, local_settlement_data: SettlementData) {
        let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
        if let Some(mut channel_data) = self
            .get(key)
            .map(|v| deserialize_from::<ChannelData>(v.as_ref(), "ChannelData"))
        {
            channel_data.local_settlement_data = Some(local_settlement_data);
            let mut batch = self.batch();
            batch.put_kv(KeyValue::WatchtowerChannel(channel_id, channel_data));
            batch.commit();
        }
    }
}

impl GossipMessageStore for Store {
    fn get_broadcast_messages_iter(
        &self,
        after_cursor: &Cursor,
    ) -> impl IntoIterator<Item = crate::fiber::types::BroadcastMessageWithTimestamp> {
        let cursor = after_cursor.to_bytes();
        let prefix = [BROADCAST_MESSAGE_PREFIX];
        let start = [&prefix, cursor.as_slice()].concat();
        let start_cloned = start.clone();
        // We should skip the value with the same cursor (after_cursor is exclusive).
        self.prefix_iterator_with_skip_while_and_start(
            &prefix,
            IteratorMode::From(&start, DbDirection::Forward),
            Box::new(move |key: &[u8]| key == start_cloned),
        )
        .map(|(key, value)| {
            debug_assert_eq!(key.len(), 1 + CURSOR_SIZE);
            let mut timestamp_bytes = [0u8; 8];
            timestamp_bytes.copy_from_slice(&key[1..9]);
            let timestamp = u64::from_be_bytes(timestamp_bytes);
            let message: BroadcastMessage = deserialize_from(value.as_ref(), "BroadcastMessage");
            (message, timestamp).into()
        })
        .collect::<Vec<_>>()
    }

    fn get_broadcast_message_with_cursor(
        &self,
        cursor: &Cursor,
    ) -> Option<crate::fiber::types::BroadcastMessageWithTimestamp> {
        let key = [&[BROADCAST_MESSAGE_PREFIX], cursor.to_bytes().as_slice()].concat();
        self.get(key).map(|v| {
            let message: BroadcastMessage = deserialize_from(v.as_ref(), "BroadcastMessage");
            (message, cursor.timestamp).into()
        })
    }

    fn get_latest_broadcast_message_cursor(&self) -> Option<Cursor> {
        let prefix = vec![BROADCAST_MESSAGE_PREFIX];
        self.prefix_iterator_with_skip_while_and_start(
            &prefix,
            IteratorMode::End,
            Box::new(|_| false),
        )
        .last()
        .map(|(key, _)| {
            let last_key = key.to_vec();
            Cursor::from_bytes(&last_key[1..]).expect("deserialize Cursor should be OK")
        })
    }

    fn get_latest_channel_announcement_timestamp(&self, outpoint: &OutPoint) -> Option<u64> {
        let key = get_channel_timestamps_key(outpoint);
        self.get(
            [
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                key.as_slice(),
            ]
            .concat(),
        )
        .and_then(|v| {
            let v: [u8; 24] = v.try_into().expect("Invalid timestamp value length");
            let timestamp = u64::from_be_bytes(
                v[..8]
                    .try_into()
                    .expect("timestamp length valid, shown above"),
            );
            // The default timestamp value is 0.
            (timestamp != 0).then_some(timestamp)
        })
    }

    fn get_latest_channel_update_timestamp(
        &self,
        outpoint: &OutPoint,
        is_node1: bool,
    ) -> Option<u64> {
        let key = get_channel_timestamps_key(outpoint);
        self.get(
            [
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                key.as_slice(),
            ]
            .concat(),
        )
        .map(|v| {
            let v: [u8; 24] = v.try_into().expect("Invalid timestamp value length");
            let start_index = if is_node1 { 8 } else { 16 };
            u64::from_be_bytes(
                v[start_index..start_index + 8]
                    .try_into()
                    .expect("timestamp length valid, shown above"),
            )
        })
    }

    fn get_latest_node_announcement_timestamp(
        &self,
        pk: &crate::fiber::types::Pubkey,
    ) -> Option<u64> {
        self.get(
            [
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                BroadcastMessageID::NodeAnnouncement(*pk)
                    .to_bytes()
                    .as_slice(),
            ]
            .concat(),
        )
        .map(|v| u64::from_be_bytes(v.try_into().expect("Invalid timestamp value length")))
    }

    fn delete_broadcast_message(&self, cursor: &Cursor) {
        let key = [&[BROADCAST_MESSAGE_PREFIX], cursor.to_bytes().as_slice()].concat();
        let mut batch = self.batch();
        batch.delete(key);
        batch.commit();
    }

    fn save_channel_announcement(
        &self,
        timestamp: u64,
        channel_announcement: crate::fiber::types::ChannelAnnouncement,
    ) {
        if let Some(_old_timestamp) =
            self.get_latest_channel_announcement_timestamp(&channel_announcement.channel_outpoint)
        {
            // Channel announcement is immutable. If we have already saved one channel announcement,
            // we can early return now.
            return;
        }

        let mut batch = self.batch();

        update_channel_timestamp(
            &mut batch,
            &channel_announcement.channel_outpoint,
            timestamp,
            ChannelTimestamp::ChannelAnnouncement(),
        );

        batch.put_kv(KeyValue::BroadcastMessage(
            Cursor::new(
                timestamp,
                BroadcastMessageID::ChannelAnnouncement(
                    channel_announcement.channel_outpoint.clone(),
                ),
            ),
            BroadcastMessage::ChannelAnnouncement(channel_announcement),
        ));

        batch.commit();
    }

    fn save_channel_update(&self, channel_update: crate::fiber::types::ChannelUpdate) {
        let mut batch = self.batch();
        let message_id = BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone());

        // Remove old channel update if exists
        if let Some(old_timestamp) = self.get_latest_channel_update_timestamp(
            &channel_update.channel_outpoint,
            channel_update.is_update_of_node_1(),
        ) {
            if channel_update.timestamp <= old_timestamp {
                // This is an outdated channel update, early return
                return;
            }
            // Delete old channel update
            batch.delete(
                [
                    &[BROADCAST_MESSAGE_PREFIX],
                    Cursor::new(old_timestamp, message_id.clone())
                        .to_bytes()
                        .as_slice(),
                ]
                .concat(),
            );
        }

        update_channel_timestamp(
            &mut batch,
            &channel_update.channel_outpoint,
            channel_update.timestamp,
            if channel_update.is_update_of_node_1() {
                ChannelTimestamp::ChannelUpdateOfNode1()
            } else {
                ChannelTimestamp::ChannelUpdateOfNode2()
            },
        );

        // Save the channel update
        batch.put_kv(KeyValue::BroadcastMessage(
            Cursor::new(channel_update.timestamp, message_id),
            BroadcastMessage::ChannelUpdate(channel_update),
        ));
        batch.commit();
    }

    fn save_node_announcement(&self, node_announcement: crate::fiber::types::NodeAnnouncement) {
        debug_assert!(
            node_announcement.verify(),
            "Node announcement must be verified: {:?}",
            node_announcement
        );
        let mut batch = self.batch();
        let message_id = BroadcastMessageID::NodeAnnouncement(node_announcement.node_id);

        if let Some(old_timestamp) =
            self.get_latest_node_announcement_timestamp(&node_announcement.node_id)
        {
            if node_announcement.timestamp <= old_timestamp {
                // This is an outdated node announcement. Early return.
                return;
            }

            // Delete old node announcement
            batch.delete(
                [
                    &[BROADCAST_MESSAGE_PREFIX],
                    Cursor::new(old_timestamp, message_id.clone())
                        .to_bytes()
                        .as_slice(),
                ]
                .concat(),
            );
        }
        batch.put_kv(KeyValue::BroadcastMessageTimestamp(
            BroadcastMessageID::NodeAnnouncement(node_announcement.node_id),
            node_announcement.timestamp,
        ));

        batch.put_kv(KeyValue::BroadcastMessage(
            Cursor::new(node_announcement.timestamp, message_id.clone()),
            BroadcastMessage::NodeAnnouncement(node_announcement.clone()),
        ));
        batch.commit();
    }

    fn get_channel_timestamps_iter(&self) -> impl IntoIterator<Item = (OutPoint, [u64; 3])> {
        // 0 is used to get timestamps for channels instead of node announcements.
        const PREFIX: [u8; 2] = [BROADCAST_MESSAGE_TIMESTAMP_PREFIX, 0];
        self.prefix_iterator(&PREFIX).map(|(key, value)| {
            let outpoint =
                OutPoint::from_slice(&key[2..]).expect("deserialize OutPoint should be OK");
            assert_eq!(value.len(), 24);
            let timestamps = [
                u64::from_be_bytes(value[0..8].try_into().unwrap()),
                u64::from_be_bytes(value[8..16].try_into().unwrap()),
                u64::from_be_bytes(value[16..24].try_into().unwrap()),
            ];
            (outpoint, timestamps)
        })
    }

    fn delete_channel_timestamps(&self, outpoint: &OutPoint) {
        let key = get_channel_timestamps_key(outpoint);
        let mut batch = self.batch();
        batch.delete([&[BROADCAST_MESSAGE_TIMESTAMP_PREFIX], key.as_slice()].concat());
        batch.commit();
    }
}

// All timestamps are saved in a 24-byte array, with BroadcastMessageID::ChannelAnnouncement(outpoint) as the key.
// the first 8 bytes in the 24 bytes is the timestamp for channel announcement, the second 8 bytes
// is the timestamp for channel update of node 1 and the last 8 bytes for channel update of node 2.
// TODO: previous implementation accidentally used BroadcastMessageID::ChannelUpdate as the key
// for the channel updates timestamps. I have fixed it here by using the same key as the channel
// announcement. This is a breaking change, we need migration for this.
pub(crate) fn get_channel_timestamps_key(outpoint: &OutPoint) -> Vec<u8> {
    BroadcastMessageID::ChannelAnnouncement(outpoint.clone())
        .to_bytes()
        .to_vec()
}

fn update_channel_timestamp(
    batch: &mut Batch,
    outpoint: &OutPoint,
    timestamp: u64,
    channel_timestamp: ChannelTimestamp,
) {
    let offset = match channel_timestamp {
        ChannelTimestamp::ChannelAnnouncement() => 0,
        ChannelTimestamp::ChannelUpdateOfNode1() => 8,
        ChannelTimestamp::ChannelUpdateOfNode2() => 16,
    };
    let message_id = get_channel_timestamps_key(outpoint);

    let timestamp_key = [&[BROADCAST_MESSAGE_TIMESTAMP_PREFIX], message_id.as_slice()].concat();
    let mut timestamps = batch
        .get(&timestamp_key)
        .map(|v| v.try_into().expect("Invalid timestamp value length"))
        .unwrap_or([0u8; 24]);
    timestamps[offset..offset + 8].copy_from_slice(&timestamp.to_be_bytes());
    batch.put(timestamp_key, timestamps);
}

/// Check if the database needs to be migrated
pub fn check_migrate<P: AsRef<Path>>(path: P, db: Store) -> Result<Store, String> {
    let migrate = DbMigrate::new(&db);
    migrate.init_or_check(path)?;
    Ok(db)
}
