use super::db_migrate::DbMigrate;
use super::schema::*;
use crate::{
    fiber::{
        channel::{ChannelActorState, ChannelActorStateStore, ChannelState},
        graph::{ChannelInfo, NetworkGraphStateStore, NodeInfo, PaymentSession},
        network::{NetworkActorStateStore, PersistentNetworkActorState},
        types::{Hash256, Pubkey},
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceError, InvoiceStore},
    watchtower::{ChannelData, RevocationData, WatchtowerStore},
};
use ckb_jsonrpc_types::JsonBytes;
use ckb_types::packed::{OutPoint, Script};
use ckb_types::prelude::Entity;
use rocksdb::{prelude::*, DBIterator, Direction, IteratorMode, WriteBatch, DB};
use std::io::Write;
use std::{
    cmp::Ordering,
    io::{stdin, stdout},
    path::Path,
    sync::Arc,
};
use tentacle::secio::PeerId;
use tracing::{error, info};

#[derive(Clone)]
pub struct Store {
    pub(crate) db: Arc<DB>,
}

impl Store {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let db = Self::open_or_create_db(path).expect("Failed to open rocksdb");
        Self { db }
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
    fn open_or_create_db<P: AsRef<Path>>(path: P) -> Result<Arc<DB>, String> {
        let migrate = DbMigrate::new(path.as_ref());
        if !migrate.need_init() {
            match migrate.check() {
                Ordering::Greater => {
                    eprintln!(
                        "The database was created by a higher version fiber executable binary \n\
                     and cannot be opened by the current binary.\n\
                     Please download the latest fiber executable binary."
                    );
                    return Err("incompatible database, need to upgrade fiber binary".to_string());
                }
                Ordering::Equal => {
                    eprintln!("no need to migrate, everything is OK ...");
                    return Ok(migrate.db());
                }
                Ordering::Less => {
                    let path_buf = path.as_ref().to_path_buf();
                    let input = prompt(format!("\
                    \n\
                    Fiber need to run some database migrations.\n\
                    \n\
                    Once the migration started, the data will be no longer compatible with all older version,\n\
                    so we strongly recommended you to backup the old data {} before migrating.\n\
                    \n\
                    If the migration failed, try to delete all data and sync from scratch.\n\
                    \nIf you want to migrate the data, please input YES, otherwise, the current process will exit.\n\
                    > ", path_buf.display()).as_str());

                    if input.trim().to_lowercase() != "yes" {
                        error!("Migration was declined since the user didn't confirm.");
                        return Err("need to migrate".to_string());
                    }
                    info!("now begin to migrate db ...");
                    let db = migrate.migrate().expect("failed to migrate db");
                    info!("db migrated successfully ...");
                    Ok(db)
                }
            }
        } else {
            eprintln!("now begin to init db version ...");
            migrate
                .init_db_version()
                .expect("failed to init db version");
            Ok(migrate.db())
        }
    }
}

pub fn prompt(msg: &str) -> String {
    let stdout = stdout();
    let mut stdout = stdout.lock();
    let stdin = stdin();

    write!(stdout, "{msg}").unwrap();
    stdout.flush().unwrap();

    let mut input = String::new();
    let _ = stdin.read_line(&mut input);

    input
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
    NodeInfo(Pubkey, NodeInfo),
    ChannelInfo(OutPoint, ChannelInfo),
    WatchtowerChannel(Hash256, ChannelData),
    PaymentSession(Hash256, PaymentSession),
    NetworkActorState(PeerId, PersistentNetworkActorState),
}

impl Batch {
    fn put_kv(&mut self, key_value: KeyValue) {
        match key_value {
            KeyValue::ChannelActorState(id, state) => {
                let key = [&[CHANNEL_ACTOR_STATE_PREFIX], id.as_ref()].concat();
                let value =
                    bincode::serialize(&state).expect("serialize ChannelActorState should be OK");
                self.put(key, value);
            }
            KeyValue::CkbInvoice(id, invoice) => {
                let key = [&[CKB_INVOICE_PREFIX], id.as_ref()].concat();
                let value =
                    bincode::serialize(&invoice).expect("serialize CkbInvoice should be OK");
                self.put(key, value);
            }
            KeyValue::CkbInvoicePreimage(id, preimage) => {
                let key = [&[CKB_INVOICE_PREIMAGE_PREFIX], id.as_ref()].concat();
                self.put(
                    key,
                    bincode::serialize(&preimage).expect("serialize Hash256 should be OK"),
                );
            }
            KeyValue::CkbInvoiceStatus(id, status) => {
                let key = [&[CKB_INVOICE_STATUS_PREFIX], id.as_ref()].concat();
                self.put(
                    key,
                    bincode::serialize(&status).expect("serialize CkbInvoiceStatus should be OK"),
                );
            }
            KeyValue::PeerIdChannelId((peer_id, channel_id), state) => {
                let key = [
                    &[PEER_ID_CHANNEL_ID_PREFIX],
                    peer_id.as_bytes(),
                    channel_id.as_ref(),
                ]
                .concat();
                self.put(
                    key,
                    bincode::serialize(&state).expect("serialize ChannelState should be OK"),
                );
            }
            KeyValue::ChannelInfo(channel_id, channel) => {
                // Save channel update timestamp to index, so that we can query channels by timestamp
                self.put(
                    [
                        CHANNEL_UPDATE_INDEX_PREFIX.to_be_bytes().as_slice(),
                        channel.timestamp.to_be_bytes().as_slice(),
                    ]
                    .concat(),
                    channel_id.as_slice(),
                );

                // Save channel announcement block numbers to index, so that we can query channels by block number
                self.put(
                    [
                        CHANNEL_ANNOUNCEMENT_INDEX_PREFIX.to_be_bytes().as_slice(),
                        channel.funding_tx_block_number.to_be_bytes().as_slice(),
                        channel.funding_tx_index.to_be_bytes().as_slice(),
                    ]
                    .concat(),
                    channel_id.as_slice(),
                );

                let key = [&[CHANNEL_INFO_PREFIX], channel_id.as_slice()].concat();
                self.put(
                    key,
                    bincode::serialize(&channel).expect("serialize ChannelInfo should be OK"),
                );
            }
            KeyValue::PaymentSession(payment_hash, payment_session) => {
                let key = [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat();
                self.put(
                    key,
                    bincode::serialize(&payment_session)
                        .expect("serialize PaymentSession should be OK"),
                );
            }
            KeyValue::NodeInfo(id, node) => {
                // Save node announcement timestamp to index, so that we can query nodes by timestamp
                self.put(
                    [
                        &[NODE_ANNOUNCEMENT_INDEX_PREFIX],
                        node.timestamp.to_be_bytes().as_slice(),
                    ]
                    .concat(),
                    id.serialize(),
                );

                self.put(
                    [&[NODE_INFO_PREFIX], id.serialize().as_slice()].concat(),
                    bincode::serialize(&node).expect("serialize NodeInfo should be OK"),
                );
            }
            KeyValue::WatchtowerChannel(channel_id, channel_data) => {
                let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
                self.put(
                    key,
                    bincode::serialize(&channel_data).expect("serialize ChannelData should be OK"),
                );
            }
            KeyValue::NetworkActorState(peer_id, persistent_network_actor_state) => {
                let key = [&[PEER_ID_NETWORK_ACTOR_STATE_PREFIX], peer_id.as_bytes()].concat();
                self.put(
                    key,
                    bincode::serialize(&persistent_network_actor_state)
                        .expect("serialize PersistentNetworkActorState should be OK"),
                );
            }
        }
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
        self.get(key).map(|value| {
            bincode::deserialize(value.as_ref())
                .expect("deserialize PersistentNetworkActorState should be OK")
        })
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
        self.get(key).map(|v| {
            bincode::deserialize(v.as_ref()).expect("deserialize ChannelActorState should be OK")
        })
    }

    fn insert_channel_actor_state(&self, state: ChannelActorState) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::ChannelActorState(state.id, state.clone()));
        batch.put_kv(KeyValue::PeerIdChannelId(
            (state.get_remote_peer_id(), state.id),
            state.state,
        ));
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
                let state = bincode::deserialize(value.as_ref())
                    .expect("deserialize ChannelState should be OK");
                (peer_id, channel_id.into(), state)
            })
            .collect()
    }
}

impl InvoiceStore for Store {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
        let key = [&[CKB_INVOICE_PREFIX], id.as_ref()].concat();
        self.get(key)
            .map(|v| bincode::deserialize(v.as_ref()).expect("deserialize CkbInvoice should be OK"))
    }

    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let hash = invoice.payment_hash();
        if self.get_invoice(hash).is_some() {
            return Err(InvoiceError::DuplicatedInvoice(hash.to_string()));
        }

        let mut batch = self.batch();
        if let Some(preimage) = preimage {
            batch.put_kv(KeyValue::CkbInvoicePreimage(*hash, preimage));
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
            .map(|v| bincode::deserialize(v.as_ref()).expect("deserialize Hash256 should be OK"))
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
        self.get(key).map(|v| {
            bincode::deserialize(v.as_ref()).expect("deserialize CkbInvoiceStatus should be OK")
        })
    }
}

impl NetworkGraphStateStore for Store {
    fn get_channels(&self, channel_id: Option<OutPoint>) -> Vec<ChannelInfo> {
        let (channels, _) = self.get_channels_with_params(usize::MAX, None, channel_id);
        channels
    }

    fn get_channels_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
        outpoint: Option<OutPoint>,
    ) -> (Vec<ChannelInfo>, JsonBytes) {
        let channel_prefix = vec![CHANNEL_INFO_PREFIX];
        let (prefix, skip) = after
            .as_ref()
            .map_or((vec![CHANNEL_INFO_PREFIX], 0), |after| {
                let key = [after.as_bytes().as_ref()].concat();
                (key, 1)
            });
        let outpoint_key =
            outpoint.map(|outpoint| [&[CHANNEL_INFO_PREFIX], outpoint.as_slice()].concat());

        let mode = IteratorMode::From(prefix.as_ref(), Direction::Forward);
        let mut last_key = Vec::new();
        let channels: Vec<_> = self
            .db
            .iterator(mode)
            .take_while(|(key, _)| key.starts_with(&channel_prefix))
            .filter_map(|(col_key, value)| {
                if let Some(key) = &outpoint_key {
                    if !col_key.starts_with(key) {
                        return None;
                    }
                }
                let channel: ChannelInfo = bincode::deserialize(value.as_ref())
                    .expect("deserialize ChannelInfo should be OK");
                if !channel.is_explicitly_disabled() {
                    last_key = col_key.to_vec();
                    Some(channel)
                } else {
                    None
                }
            })
            .skip(skip)
            .take(limit)
            .collect();
        (channels, JsonBytes::from_bytes(last_key.into()))
    }

    fn get_nodes(&self, node_id: Option<Pubkey>) -> Vec<NodeInfo> {
        let (nodes, _) = self.get_nodes_with_params(usize::MAX, None, node_id);
        nodes
    }

    fn get_nodes_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
        node_id: Option<Pubkey>,
    ) -> (Vec<NodeInfo>, JsonBytes) {
        let node_prefix = vec![NODE_INFO_PREFIX];
        let (prefix, skip) = after.as_ref().map_or((vec![NODE_INFO_PREFIX], 0), |after| {
            let key = [after.as_bytes().as_ref()].concat();
            (key, 1)
        });
        let node_key = node_id.map(|node_id| {
            [
                NODE_INFO_PREFIX.to_le_bytes().as_slice(),
                node_id.serialize().as_ref(),
            ]
            .concat()
        });
        let mode = IteratorMode::From(prefix.as_ref(), Direction::Forward);
        let mut last_key = Vec::new();
        let nodes: Vec<_> = self
            .db
            .iterator(mode)
            .take_while(|(key, _)| key.starts_with(&node_prefix))
            .filter_map(|(col_key, value)| {
                if let Some(key) = &node_key {
                    if !col_key.starts_with(key) {
                        return None;
                    }
                }
                last_key = col_key.to_vec();
                Some(
                    bincode::deserialize(value.as_ref())
                        .expect("deserialize NodeInfo should be OK"),
                )
            })
            .skip(skip)
            .take(limit)
            .collect();
        (nodes, JsonBytes::from_bytes(last_key.into()))
    }

    fn insert_channel(&self, channel: ChannelInfo) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::ChannelInfo(channel.out_point(), channel.clone()));
        batch.commit();
    }

    fn insert_node(&self, node: NodeInfo) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::NodeInfo(node.node_id, node.clone()));
        batch.commit();
    }

    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession> {
        let prefix = [&[PAYMENT_SESSION_PREFIX], payment_hash.as_ref()].concat();
        self.get(prefix).map(|v| {
            bincode::deserialize(v.as_ref()).expect("deserialize PaymentSession should be OK")
        })
    }

    fn insert_payment_session(&self, session: PaymentSession) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::PaymentSession(session.payment_hash(), session));
        batch.commit();
    }
}

impl WatchtowerStore for Store {
    fn get_watch_channels(&self) -> Vec<ChannelData> {
        let prefix = vec![WATCHTOWER_CHANNEL_PREFIX];
        self.prefix_iterator(&prefix)
            .map(|(_key, value)| {
                bincode::deserialize(value.as_ref()).expect("deserialize ChannelData should be OK")
            })
            .collect()
    }

    fn insert_watch_channel(&self, channel_id: Hash256, funding_tx_lock: Script) {
        let mut batch = self.batch();
        let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
        let value = bincode::serialize(&ChannelData {
            channel_id,
            funding_tx_lock,
            revocation_data: None,
        })
        .expect("serialize ChannelData should be OK");
        batch.put(key, value);
        batch.commit();
    }

    fn remove_watch_channel(&self, channel_id: Hash256) {
        let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
        self.db.delete(key).expect("delete should be OK");
    }

    fn update_revocation(&self, channel_id: Hash256, revocation_data: RevocationData) {
        let key = [&[WATCHTOWER_CHANNEL_PREFIX], channel_id.as_ref()].concat();
        if let Some(mut channel_data) = self.get(key).map(|v| {
            bincode::deserialize::<ChannelData>(v.as_ref())
                .expect("deserialize ChannelData should be OK")
        }) {
            channel_data.revocation_data = Some(revocation_data);
            let mut batch = self.batch();
            batch.put_kv(KeyValue::WatchtowerChannel(channel_id, channel_data));
            batch.commit();
        }
    }
}
