use super::db_migrate::DbMigrate;
use super::schema::*;
use crate::{
    fiber::{
        channel::{
            ChannelActorState, ChannelActorStateStore, ChannelState, RevocationData, SettlementData,
        },
        gossip::GossipMessageStore,
        graph::{NetworkGraphStateStore, PaymentSession},
        history::{Direction, TimedResult},
        network::{NetworkActorStateStore, PersistentNetworkActorState},
        types::{BroadcastMessage, BroadcastMessageID, Cursor, Hash256, CURSOR_SIZE},
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceError, InvoiceStore},
    watchtower::{ChannelData, WatchtowerStore},
};
use ckb_types::packed::{OutPoint, Script};
use ckb_types::prelude::Entity;
use rocksdb::{
    prelude::*, DBCompressionType, DBIterator, Direction as DbDirection, IteratorMode, WriteBatch,
    DB,
};
use serde::Serialize;
use std::{path::Path, sync::Arc};
use tentacle::secio::PeerId;

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) db: Arc<DB>,
}

#[derive(Copy, Clone)]
enum ChannelTimestamp {
    ChannelAnnouncement(),
    ChannelUpdateOfNode1(),
    ChannelUpdateOfNode2(),
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
    let message_id = match channel_timestamp {
        ChannelTimestamp::ChannelAnnouncement() => {
            BroadcastMessageID::ChannelAnnouncement(outpoint.clone())
        }
        ChannelTimestamp::ChannelUpdateOfNode1() => {
            BroadcastMessageID::ChannelUpdate(outpoint.clone())
        }
        ChannelTimestamp::ChannelUpdateOfNode2() => {
            BroadcastMessageID::ChannelUpdate(outpoint.clone())
        }
    };

    let timestamp_key = [
        &[BROADCAST_MESSAGE_TIMESTAMP_PREFIX],
        message_id.to_bytes().as_slice(),
    ]
    .concat();
    let mut timestamps = batch
        .get(&timestamp_key)
        .map(|v| v.try_into().expect("Invalid timestamp value length"))
        .unwrap_or([0u8; 24]);
    timestamps[offset..offset + 8].copy_from_slice(&timestamp.to_be_bytes());
    batch.put(timestamp_key, timestamps);
}

impl Store {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let db = Self::open_db(path.as_ref())?;
        let db = Self::check_migrate(path, db)?;
        Ok(Self { db })
    }

    fn open_db(path: &Path) -> Result<Arc<DB>, String> {
        // add more migrations here
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_compression_type(DBCompressionType::Lz4);
        let db = Arc::new(DB::open(&options, path).map_err(|e| e.to_string())?);
        Ok(db)
    }

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    #[allow(dead_code)]
    fn get_range<K: AsRef<[u8]>>(
        &self,
        lower_bound: Option<K>,
        upper_bound: Option<K>,
    ) -> DBIterator {
        assert!(lower_bound.is_some() || upper_bound.is_some());
        let mut read_options = ReadOptions::default();
        if let Some(lower_bound) = lower_bound {
            read_options.set_iterate_lower_bound(lower_bound.as_ref());
        }
        if let Some(upper_bound) = upper_bound {
            read_options.set_iterate_upper_bound(upper_bound.as_ref());
        }
        let mode = IteratorMode::Start;
        self.db.get_iter(&read_options, mode)
    }

    fn batch(&self) -> Batch {
        Batch {
            db: Arc::clone(&self.db),
            wb: WriteBatch::default(),
        }
    }

    fn prefix_iterator<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        self.db
            .prefix_iterator(prefix)
            .take_while(move |(col_key, _)| col_key.starts_with(prefix))
    }

    /// Open or create a rocksdb
    fn check_migrate<P: AsRef<Path>>(path: P, db: Arc<DB>) -> Result<Arc<DB>, String> {
        let migrate = DbMigrate::new(db);
        migrate.init_or_check(path)
    }
}

pub struct Batch {
    db: Arc<DB>,
    wb: WriteBatch,
}

enum KeyValue {
    ChannelActorState(Hash256, ChannelActorState),
    CkbInvoice(Hash256, CkbInvoice),
    CkbInvoicePreimage(Hash256, Hash256),
    CkbInvoiceStatus(Hash256, CkbInvoiceStatus),
    PeerIdChannelId((PeerId, Hash256), ChannelState),
    OutPointChannelId(OutPoint, Hash256),
    BroadcastMessageTimestamp(BroadcastMessageID, u64),
    BroadcastMessage(Cursor, BroadcastMessage),
    WatchtowerChannel(Hash256, ChannelData),
    PaymentSession(Hash256, PaymentSession),
    PaymentHistoryTimedResult((OutPoint, Direction), TimedResult),
    NetworkActorState(PeerId, PersistentNetworkActorState),
}

pub trait StoreKeyValue {
    fn key(&self) -> Vec<u8>;
    fn value(&self) -> Vec<u8>;
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

impl StoreKeyValue for KeyValue {
    fn key(&self) -> Vec<u8> {
        match self {
            KeyValue::ChannelActorState(id, _) => {
                [&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat()
            }
            KeyValue::CkbInvoice(id, _) => [&[CKB_INVOICE_PREFIX], id.as_ref()].concat(),
            KeyValue::CkbInvoicePreimage(id, _) => {
                [&[CKB_INVOICE_PREIMAGE_PREFIX], id.as_ref()].concat()
            }
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
        }
    }

    fn value(&self) -> Vec<u8> {
        match self {
            KeyValue::ChannelActorState(_, state) => serialize_to_vec(state, "ChannelActorState"),
            KeyValue::CkbInvoice(_, invoice) => serialize_to_vec(invoice, "CkbInvoice"),
            KeyValue::CkbInvoicePreimage(_, preimage) => serialize_to_vec(preimage, "Hash256"),
            KeyValue::CkbInvoiceStatus(_, status) => serialize_to_vec(status, "CkbInvoiceStatus"),
            KeyValue::PeerIdChannelId(_, state) => serialize_to_vec(state, "ChannelState"),
            KeyValue::OutPointChannelId(_, channel_id) => serialize_to_vec(channel_id, "ChannelId"),
            KeyValue::PaymentSession(_, payment_session) => {
                serialize_to_vec(payment_session, "PaymentSession")
            }
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
        }
    }
}

impl Batch {
    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    fn put_kv(&mut self, key_value: KeyValue) {
        self.put(key_value.key(), key_value.value());
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.wb.put(key, value).expect("put should be OK")
    }

    fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.wb.delete(key.as_ref()).expect("delete should be OK")
    }

    fn commit(self) {
        self.db.write(&self.wb).expect("commit should be OK")
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
        batch.put_kv(KeyValue::ChannelActorState(state.id, state.clone()));
        batch.put_kv(KeyValue::PeerIdChannelId(
            (state.get_remote_peer_id(), state.id),
            state.state,
        ));
        if let Some(outpoint) = state.get_funding_transaction_outpoint() {
            batch.put_kv(KeyValue::OutPointChannelId(outpoint, state.id));
        }
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
            batch.delete(
                [
                    &[CHANNEL_OUTPOINT_CHANNEL_ID_PREFIX],
                    state.must_get_funding_transaction_outpoint().as_slice(),
                ]
                .concat(),
            );
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
        let payment_hash = invoice.payment_hash();
        if self.get_invoice(payment_hash).is_some() {
            return Err(InvoiceError::DuplicatedInvoice(payment_hash.to_string()));
        }

        let mut batch = self.batch();
        if let Some(preimage) = preimage {
            batch.put_kv(KeyValue::CkbInvoicePreimage(*payment_hash, preimage));
        }
        let payment_hash = *invoice.payment_hash();
        batch.put_kv(KeyValue::CkbInvoice(payment_hash, invoice));
        batch.put_kv(KeyValue::CkbInvoiceStatus(
            payment_hash,
            CkbInvoiceStatus::Open,
        ));
        batch.commit();
        return Ok(());
    }

    fn get_invoice_preimage(&self, id: &Hash256) -> Option<Hash256> {
        let key = [&[CKB_INVOICE_PREIMAGE_PREFIX], id.as_ref()].concat();
        self.get(key)
            .map(|v| deserialize_from(v.as_ref(), "Hash256"))
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

    fn insert_payment_preimage(
        &self,
        payment_hash: Hash256,
        preimage: Hash256,
    ) -> Result<(), InvoiceError> {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::CkbInvoicePreimage(payment_hash, preimage));
        batch.commit();
        Ok(())
    }
}

impl NetworkGraphStateStore for Store {
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession> {
        let prefix = [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat();
        self.get(prefix)
            .map(|v| deserialize_from(v.as_ref(), "PaymentSession"))
    }

    fn insert_payment_session(&self, session: PaymentSession) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::PaymentSession(session.payment_hash(), session));
        batch.commit();
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
            let channel_outpoint: OutPoint = OutPoint::from_slice(&key[1..=36])
                .expect("deserialize OutPoint should be OK")
                .into();
            let direction = deserialize_from(&key[37..], "Direction");
            let result = deserialize_from(value.as_ref(), "TimedResult");
            (channel_outpoint, direction, result)
        })
        .collect()
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
        let mode = IteratorMode::From(&start, DbDirection::Forward);
        self.db
            .iterator(mode)
            // We should skip the value with the same cursor (after_cursor is exclusive).
            .skip_while(move |(key, _)| key.as_ref() == &start)
            .take_while(move |(key, _)| key.starts_with(&prefix))
            .map(|(key, value)| {
                debug_assert_eq!(key.len(), 1 + CURSOR_SIZE);
                let mut timestamp_bytes = [0u8; 8];
                timestamp_bytes.copy_from_slice(&key[1..9]);
                let timestamp = u64::from_be_bytes(timestamp_bytes);
                let message: BroadcastMessage =
                    deserialize_from(value.as_ref(), "BroadcastMessage");
                (message, timestamp).into()
            })
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
        let mode = IteratorMode::End;
        self.db
            .iterator(mode)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .last()
            .map(|(key, _)| {
                let last_key = key.to_vec();
                Cursor::from_bytes(&last_key[1..]).expect("deserialize Cursor should be OK")
            })
    }

    fn get_latest_channel_announcement_timestamp(&self, outpoint: &OutPoint) -> Option<u64> {
        self.get(
            &[
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                BroadcastMessageID::ChannelAnnouncement(outpoint.clone())
                    .to_bytes()
                    .as_slice(),
            ]
            .concat(),
        )
        .map(|v| {
            let v: [u8; 24] = v.try_into().expect("Invalid timestamp value length");
            u64::from_be_bytes(
                v[..8]
                    .try_into()
                    .expect("timestamp length valid, shown above"),
            )
        })
    }

    fn get_latest_channel_update_timestamp(
        &self,
        outpoint: &OutPoint,
        is_node1: bool,
    ) -> Option<u64> {
        self.get(
            &[
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                BroadcastMessageID::ChannelUpdate(outpoint.clone())
                    .to_bytes()
                    .as_slice(),
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
            &[
                [BROADCAST_MESSAGE_TIMESTAMP_PREFIX].as_slice(),
                BroadcastMessageID::NodeAnnouncement(pk.clone())
                    .to_bytes()
                    .as_slice(),
            ]
            .concat(),
        )
        .map(|v| u64::from_be_bytes(v.try_into().expect("Invalid timestamp value length")))
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
        let message_id = BroadcastMessageID::NodeAnnouncement(node_announcement.node_id.clone());

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
            BroadcastMessageID::NodeAnnouncement(node_announcement.node_id.clone()),
            node_announcement.timestamp,
        ));

        batch.put_kv(KeyValue::BroadcastMessage(
            Cursor::new(node_announcement.timestamp, message_id.clone()),
            BroadcastMessage::NodeAnnouncement(node_announcement.clone()),
        ));
        batch.commit();
    }
}

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
        self.db.delete(key).expect("delete should be OK");
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
