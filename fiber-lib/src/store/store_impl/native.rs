use super::{super::db_migrate::DbMigrate, KeyValue, StoreKeyValue};

use rocksdb::{prelude::*, DBCompressionType, DBIterator, IteratorMode, WriteBatch, DB};
use std::{path::Path, sync::Arc};

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) db: Arc<DB>,
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

    pub(crate) fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    #[allow(dead_code)]
    pub(crate) fn get_range<K: AsRef<[u8]>>(
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

    pub(crate) fn batch(&self) -> Batch {
        Batch {
            db: Arc::clone(&self.db),
            wb: WriteBatch::default(),
        }
    }

    pub(crate) fn prefix_iterator<'a>(
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

impl Batch {
    pub(crate) fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    pub(super) fn put_kv(&mut self, key_value: KeyValue) {
        self.put(key_value.key(), key_value.value());
    }

    pub(crate) fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.wb.put(key, value).expect("put should be OK")
    }

    pub(crate) fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.wb.delete(key.as_ref()).expect("delete should be OK")
    }

    pub(crate) fn commit(self) {
        self.db.write(&self.wb).expect("commit should be OK")
    }
}
