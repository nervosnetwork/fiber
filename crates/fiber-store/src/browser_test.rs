use std::fmt::Debug;
use std::{cell::RefCell, collections::BTreeMap, path::Path, rc::Rc};

use crate::backend::{BatchWriter, StorageBackend, TakeWhileFn};
use crate::iterator::{IteratorDirection, KVPair};

#[derive(Clone)]
pub struct Store {
    data: Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "BrowserTestStore")?;
        Ok(())
    }
}

unsafe impl Send for Store {}
unsafe impl Sync for Store {}

impl Store {
    /// Open a store (without migration check)
    pub fn open_db(_path: &Path) -> Result<Self, String> {
        Ok(Self {
            data: Default::default(),
        })
    }

    pub fn shutdown(self) {
        // no-op for in-memory test store
    }

    /// Helper for collect_iterator: collect matching items from a BTreeMap range
    fn collect_from_btree(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: &TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair> {
        let guard = self.data.borrow();
        let mut results = Vec::new();

        match direction {
            IteratorDirection::Forward => {
                for (key, value) in guard.range::<Vec<u8>, _>(start..) {
                    if !take_while_fn(key) {
                        break;
                    }
                    results.push(KVPair {
                        key: key.clone(),
                        value: value.clone(),
                    });
                    if limit > 0 && results.len() >= limit {
                        break;
                    }
                }
            }
            IteratorDirection::Reverse => {
                for (key, value) in guard.range::<Vec<u8>, _>(..=start).rev() {
                    if !take_while_fn(key) {
                        break;
                    }
                    results.push(KVPair {
                        key: key.clone(),
                        value: value.clone(),
                    });
                    if limit > 0 && results.len() >= limit {
                        break;
                    }
                }
            }
        }

        results
    }
}

impl StorageBackend for Store {
    type Batch = Batch;

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        let guard = self.data.borrow();
        guard.get(key.as_ref()).cloned()
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        let mut guard = self.data.borrow_mut();
        guard.insert(key.as_ref().to_vec(), value.as_ref().to_vec());
    }

    fn delete<K: AsRef<[u8]>>(&self, key: K) {
        let mut guard = self.data.borrow_mut();
        guard.remove(key.as_ref());
    }

    fn batch(&self) -> Self::Batch {
        Batch {
            data: self.data.clone(),
            delete: vec![],
            puts: vec![],
        }
    }

    fn collect_iterator(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair> {
        self.collect_from_btree(start, direction, &take_while_fn, limit)
    }
}

struct KV {
    key: Vec<u8>,
    value: Vec<u8>,
}

pub struct Batch {
    data: Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>,
    puts: Vec<KV>,
    delete: Vec<Vec<u8>>,
}

impl BatchWriter for Batch {
    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.puts.push(KV {
            key: key.as_ref().to_vec(),
            value: value.as_ref().to_vec(),
        });
    }

    fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.delete.push(key.as_ref().to_vec());
    }

    fn commit(self) {
        let mut guard = self.data.borrow_mut();
        for key in self.delete {
            guard.remove(&key);
        }
        for KV { key, value } in self.puts {
            guard.insert(key, value);
        }
    }
}
