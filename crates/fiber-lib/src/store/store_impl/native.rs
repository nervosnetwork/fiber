use crate::fiber::types::Hash256;
use crate::store::schema::PREIMAGE_PREFIX;

use super::check_migrate;
use super::{KeyValue, StoreChange, StoreKeyValue};

use ractor::OutputPort;
pub use rocksdb::Direction as DbDirection;
pub use rocksdb::IteratorMode;
use rocksdb::{prelude::*, DBCompressionType, DBIterator, WriteBatch, DB};
use std::{fmt::Debug, path::Path, sync::Arc};

pub trait StoreChangeWatcher: Send + Sync + Debug {
    fn on_store_change(&self, change: StoreChange);
}

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) db: Arc<DB>,
    watcher: Option<Arc<dyn StoreChangeWatcher>>,
}

impl Store {
    /// Open a store, with migration check
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let store = Self::open_db(path.as_ref())?;
        let store = check_migrate(path, store)?;
        Ok(store)
    }
    /// Open a store, without migration check
    pub fn open_db(path: &Path) -> Result<Self, String> {
        // add more migrations here
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_compression_type(DBCompressionType::Lz4);
        let db = Arc::new(DB::open(&options, path).map_err(|e| e.to_string())?);
        Ok(Self { db, watcher: None })
    }

    pub fn set_watcher(&mut self, watcher: Arc<dyn StoreChangeWatcher>) {
        self.watcher = Some(watcher);
    }

    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    pub(crate) fn delete<K: AsRef<[u8]>>(&self, key: K) {
        self.db.delete(key).expect("Unexpected error from get");
    }

    pub(crate) fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        self.db.put(key, value).expect("put should be ok");
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

    pub fn batch(&self) -> Batch {
        Batch {
            db: Arc::clone(&self.db),
            wb: WriteBatch::default(),
            watcher: self.watcher.clone().map(BatchWatcher::new),
        }
    }
    /// Returns a prefix iterator, using iterator mode `mode`, skipping items until `skip_while` returns false, iterating over items prefixed with `prefix`
    #[allow(clippy::type_complexity)]
    pub fn prefix_iterator_with_skip_while_and_start<'a>(
        &'a self,
        prefix: &'a [u8],
        mode: IteratorMode<'a>,
        skip_while: Box<dyn Fn(&[u8]) -> bool + 'static>,
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        self.db
            .get_iter(
                &{
                    let mut opts = ReadOptions::default();
                    opts.set_prefix_same_as_start(true);
                    opts
                },
                mode,
            )
            .skip_while(move |(key, _)| skip_while(key))
            .take_while(move |(col_key, _)| col_key.starts_with(prefix))
    }

    pub fn prefix_iterator<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        self.prefix_iterator_with_skip_while_and_start(
            prefix,
            IteratorMode::From(prefix, DbDirection::Forward),
            Box::new(|_| false),
        )
    }
}

pub struct BatchWatcher {
    inner: Arc<dyn StoreChangeWatcher>,
    pending_changes: Vec<StoreChange>,
}

pub struct Batch {
    db: Arc<DB>,
    wb: WriteBatch,
    watcher: Option<BatchWatcher>,
}

impl BatchWatcher {
    pub fn new(inner: Arc<dyn StoreChangeWatcher>) -> Self {
        Self {
            inner,
            pending_changes: Vec::new(),
        }
    }

    pub fn record_put(&mut self, key_value: KeyValue) {
        let change = match key_value {
            KeyValue::CkbInvoice(payment_hash, invoice) => StoreChange::PutCkbInvoice {
                payment_hash,
                invoice,
            },
            KeyValue::Preimage(payment_hash, payment_preimage) => StoreChange::PutPreimage {
                payment_hash,
                payment_preimage,
            },
            KeyValue::CkbInvoiceStatus(payment_hash, payment_status) => {
                StoreChange::PutCkbInvoiceStatus {
                    payment_hash,
                    payment_status,
                }
            }
            KeyValue::PaymentSession(payment_hash, payment_session) => {
                StoreChange::PutPaymentSession {
                    payment_hash,
                    payment_session,
                }
            }
            _ => return,
        };

        self.pending_changes.push(change);
    }

    pub fn record_delete(&mut self, key: &[u8]) {
        let change = match key.first().copied().unwrap_or_default() {
            PREIMAGE_PREFIX => {
                let mut buf = [0u8; 32];
                buf.copy_from_slice(&key[1..]);
                StoreChange::DeletePreimage {
                    payment_hash: Hash256::from(buf),
                }
            }
            _ => return,
        };

        self.pending_changes.push(change);
    }

    pub fn commit(self) {
        for change in self.pending_changes {
            self.inner.on_store_change(change);
        }
    }
}

impl Batch {
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    pub fn put_kv(&mut self, key_value: KeyValue) {
        self.put(key_value.key(), key_value.value());
        if let Some(watcher) = &mut self.watcher {
            watcher.record_put(key_value);
        }
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.wb.put(key, value).expect("put should be OK")
    }

    pub fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.wb.delete(key.as_ref()).expect("delete should be OK");
        if let Some(watcher) = &mut self.watcher {
            watcher.record_delete(key.as_ref());
        }
    }

    pub fn commit(self) {
        self.db.write(&self.wb).expect("commit should be OK");
        if let Some(watcher) = self.watcher {
            watcher.commit();
        }
    }
}

impl StoreChangeWatcher for OutputPort<StoreChange> {
    fn on_store_change(&self, change: StoreChange) {
        self.send(change);
    }
}
