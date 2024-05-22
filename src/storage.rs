use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use rocksdb::{prelude::*, Direction, IteratorMode, WriteBatch, DB};

#[derive(Clone)]
pub struct Storage {
    pub(crate) db: Arc<DB>,
}

impl Storage {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let db = Arc::new(DB::open_default(path).expect("Failed to open rocksdb"));
        Self { db }
    }

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Result<Option<Vec<u8>>, Error> {
        self.db.get(key.as_ref()).map(|v| v.map(|vi| vi.to_vec()))
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
    fn put_kv<K: Into<Vec<u8>>, V: Into<Vec<u8>>>(
        &mut self,
        key: K,
        value: V,
    ) -> Result<(), Error> {
        self.put(&Into::<Vec<u8>>::into(key), &Into::<Vec<u8>>::into(value))
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) -> Result<(), Error> {
        self.wb.put(key, value)
    }

    fn delete<K: AsRef<[u8]>>(&mut self, key: K) -> Result<(), Error> {
        self.wb.delete(key.as_ref())
    }

    fn commit(self) -> Result<(), Error> {
        self.db.write(&self.wb)
    }
}
