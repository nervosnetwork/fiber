use super::{KVStore, KeyValue, StoreChange, StoreKeyValue};

pub use fiber_store::DbDirection;
pub use fiber_store::IteratorMode;
use ractor::OutputPort;
use std::{fmt::Debug, path::Path, sync::Arc};

pub trait StoreChangeWatcher: Send + Sync + Debug {
    fn on_store_change(&self, change: StoreChange);
}

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) inner: fiber_store::Store,
    watcher: Option<Arc<dyn StoreChangeWatcher>>,
}

impl Store {
    /// Open a store, with migration check
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let store = Self::open_db(path.as_ref())?;
        let store = super::check_migrate(path, store)?;
        Ok(store)
    }
    /// Open a store, without migration check
    pub fn open_db(path: &Path) -> Result<Self, String> {
        let inner = fiber_store::Store::open_db(path)?;
        Ok(Self {
            inner,
            watcher: None,
        })
    }

    pub fn set_watcher(&mut self, watcher: Arc<dyn StoreChangeWatcher>) {
        self.watcher = Some(watcher);
    }

    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.inner.get(key)
    }

    pub(crate) fn delete<K: AsRef<[u8]>>(&self, key: K) {
        self.inner.delete(key)
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        self.inner.put(key, value)
    }

    pub fn batch(&self) -> Batch {
        Batch {
            inner: self.inner.batch(),
            watcher: self.watcher.clone().map(BatchWatcher::new),
        }
    }

    /// Returns a prefix iterator, using iterator mode `mode`, skipping items until `skip_while`
    /// returns false, iterating over items prefixed with `prefix`
    #[allow(clippy::type_complexity)]
    pub fn prefix_iterator_with_skip_while_and_start<'a>(
        &'a self,
        prefix: &'a [u8],
        mode: IteratorMode<'a>,
        skip_while: Box<dyn Fn(&[u8]) -> bool + 'static>,
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        self.inner
            .prefix_iterator_with_skip_while_and_start(prefix, mode, skip_while)
    }

    pub fn prefix_iterator<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        self.inner.prefix_iterator(prefix)
    }
}

impl KVStore for Store {
    /// Returns the Store type.
    fn inner_db(&self) -> &fiber_store::Store {
        &self.inner
    }
}

pub struct BatchWatcher {
    inner: Arc<dyn StoreChangeWatcher>,
    pending_changes: Vec<StoreChange>,
}

pub struct Batch {
    inner: fiber_store::Batch,
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
            KeyValue::Preimage(payment_hash, payment_preimage) => StoreChange::PutPreimage {
                payment_hash,
                payment_preimage,
            },
            KeyValue::CkbInvoiceStatus(payment_hash, payment_status) => {
                StoreChange::PutCkbInvoiceStatus {
                    payment_hash,
                    invoice_status: payment_status,
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

    pub fn commit(self) {
        for change in self.pending_changes {
            self.inner.on_store_change(change);
        }
    }
}

impl Batch {
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.inner.get(key)
    }

    pub fn put_kv(&mut self, key_value: KeyValue) {
        self.inner.put(key_value.key(), key_value.value());
        if let Some(watcher) = &mut self.watcher {
            watcher.record_put(key_value);
        }
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.inner.put(key, value)
    }

    pub fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.inner.delete(key)
    }

    pub fn commit(self) {
        self.inner.commit();
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
