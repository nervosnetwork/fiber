use rocksdb::Direction as DbDirection;
use rocksdb::IteratorMode;
use rocksdb::{prelude::*, DBCompressionType, WriteBatch, DB};
use std::path::Path;
use std::sync::Arc;

use crate::backend::{BatchWriter, StorageBackend, TakeWhileFn};
use crate::iterator::{IteratorDirection, KVPair};

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) db: Arc<DB>,
}

impl Store {
    /// Open a store (without migration check — use `check_migrate` or `DbMigrate` for that)
    pub fn open_db(path: &Path) -> Result<Self, String> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_compression_type(DBCompressionType::Lz4);
        let db = Arc::new(DB::open(&options, path).map_err(|e| e.to_string())?);
        Ok(Self { db })
    }

    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    pub fn delete<K: AsRef<[u8]>>(&self, key: K) {
        self.db.delete(key).expect("Unexpected error from delete");
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        self.db.put(key, value).expect("put should be ok");
    }

    pub fn batch(&self) -> Batch {
        Batch {
            db: Arc::clone(&self.db),
            wb: WriteBatch::default(),
        }
    }
}

impl StorageBackend for Store {
    type Batch = Batch;

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.get(key)
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        self.put(key, value)
    }

    fn delete<K: AsRef<[u8]>>(&self, key: K) {
        self.delete(key)
    }

    fn batch(&self) -> Self::Batch {
        self.batch()
    }

    fn collect_iterator(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair> {
        let db_direction = match direction {
            IteratorDirection::Forward => DbDirection::Forward,
            IteratorDirection::Reverse => DbDirection::Reverse,
        };

        let mode = IteratorMode::From(&start, db_direction);

        let mut opts = ReadOptions::default();
        opts.set_prefix_same_as_start(true);

        let iter = self.db.get_iter(&opts, mode);

        let mut results = Vec::new();
        for (key, value) in iter {
            if !take_while_fn(&key) {
                break;
            }

            results.push(KVPair {
                key: key.to_vec(),
                value: value.to_vec(),
            });
            if limit > 0 && results.len() >= limit {
                break;
            }
        }

        results
    }
}

pub struct Batch {
    db: Arc<DB>,
    wb: WriteBatch,
}

impl Batch {
    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.wb.put(key, value).expect("put should be OK")
    }

    pub fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.wb.delete(key.as_ref()).expect("delete should be OK");
    }

    pub fn commit(self) {
        self.db.write(&self.wb).expect("commit should be OK");
    }
}

impl BatchWriter for Batch {
    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.wb.put(key, value).expect("put should be OK")
    }

    fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.wb.delete(key.as_ref()).expect("delete should be OK");
    }

    fn commit(self) {
        self.db.write(&self.wb).expect("commit should be OK");
    }
}
