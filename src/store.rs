use std::{path::Path, sync::Arc};

use rocksdb::{prelude::*, WriteBatch, DB};
use serde_json;
use tentacle::secio::PeerId;

use crate::{
    ckb::{
        channel::{ChannelActorState, ChannelActorStateStore, ChannelState},
        types::Hash256,
    },
    invoice::{CkbInvoice, InvoiceStore},
};

#[derive(Clone)]
pub struct Store {
    pub(crate) db: Arc<DB>,
}

impl Store {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let db = Arc::new(DB::open_default(path).expect("Failed to open rocksdb"));
        Self { db }
    }

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    fn batch(&self) -> Batch {
        Batch {
            db: Arc::clone(&self.db),
            wb: WriteBatch::default(),
        }
    }
}

pub struct Batch {
    db: Arc<DB>,
    wb: WriteBatch,
}

impl Batch {
    fn put_kv(&mut self, key_value: KeyValue) {
        let (key, value) = match key_value {
            KeyValue::ChannelActorState(id, state) => {
                let key = [&[0], id.as_ref()].concat();
                (
                    key,
                    serde_json::to_vec(&state).expect("serialize ChannelActorState should be OK"),
                )
            }
            KeyValue::CkbInvoice(id, invoice) => {
                let key = [&[32], id.as_ref()].concat();
                (
                    key,
                    serde_json::to_vec(&invoice).expect("serialize CkbInvoice should be OK"),
                )
            }
            KeyValue::PeerIdChannelId((peer_id, channel_id), state) => {
                let key = [&[64], peer_id.as_bytes(), channel_id.as_ref()].concat();
                (
                    key,
                    serde_json::to_vec(&state).expect("serialize ChannelState should be OK"),
                )
            }
        };
        self.put(key, value)
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

///
/// +--------------+--------------------+--------------------------+
/// | KeyPrefix::  | Key::              | Value::                  |
/// +--------------+--------------------+--------------------------+
/// | 0            | Hash256            | ChannelActorState        |
/// | 32           | Hash256            | CkbInvoice               |
/// | 64           | PeerId | Hash256   | ChannelState             |
/// +--------------+--------------------+--------------------------+
///

enum KeyValue {
    ChannelActorState(Hash256, ChannelActorState),
    CkbInvoice(Hash256, CkbInvoice),
    PeerIdChannelId((PeerId, Hash256), ChannelState),
}

impl ChannelActorStateStore for Store {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState> {
        let mut key = Vec::with_capacity(33);
        key.extend_from_slice(&[0]);
        key.extend_from_slice(id.as_ref());

        self.get(key).map(|v| {
            serde_json::from_slice(v.as_ref()).expect("deserialize ChannelActorState should be OK")
        })
    }

    fn insert_channel_actor_state(&self, state: ChannelActorState) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::ChannelActorState(state.id.clone(), state.clone()));
        batch.put_kv(KeyValue::PeerIdChannelId(
            (state.peer_id, state.id),
            state.state,
        ));
        batch.commit();
    }

    fn delete_channel_actor_state(&self, id: &Hash256) {
        if let Some(state) = self.get_channel_actor_state(id) {
            let mut batch = self.batch();
            batch.delete([&[0], id.as_ref()].concat());
            batch.delete([&[64], state.peer_id.as_bytes(), id.as_ref()].concat());
            batch.commit();
        }
    }

    fn get_channels(&self, peer_id: &tentacle::secio::PeerId) -> Vec<Hash256> {
        let prefix = [&[64], peer_id.as_bytes()].concat();
        let iter = self.db.prefix_iterator(prefix.as_ref());
        iter.map(|(key, _)| {
            let channel_id: [u8; 32] = key[prefix.len()..]
                .try_into()
                .expect("channel id should be 32 bytes");
            channel_id.into()
        })
        .collect()
    }
}

impl InvoiceStore for Store {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
        let mut key = Vec::with_capacity(33);
        key.extend_from_slice(&[32]);
        key.extend_from_slice(id.as_ref());

        self.get(key).map(|v| {
            serde_json::from_slice(v.as_ref()).expect("deserialize CkbInvoice should be OK")
        })
    }

    fn insert_invoice(&self, invoice: CkbInvoice) {
        let mut batch = self.batch();
        batch.put_kv(KeyValue::CkbInvoice(
            invoice.payment_hash().clone(),
            invoice,
        ));
        batch.commit();
    }
}
