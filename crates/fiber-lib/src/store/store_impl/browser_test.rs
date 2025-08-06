use super::{KeyValue, StoreKeyValue};
use std::fmt::Debug;
use std::{cell::RefCell, collections::BTreeMap, path::Path, rc::Rc};

pub enum DbDirection {
    Forward,
    Reverse,
}
pub enum IteratorMode<'a> {
    Start,
    End,
    From(&'a [u8], DbDirection),
}
pub struct KV {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}
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
    /// Open a store, with migration check
    pub fn new<P: AsRef<Path>>(_path: P) -> Result<Self, String> {
        Ok(Self {
            data: Default::default(),
        })
    }
    /// Open a store, without migration check
    pub fn open_db(path: &Path) -> Result<Self, String> {
        Self::new(path)
    }
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        let guard = self.data.borrow();
        guard.get(key.as_ref()).cloned()
    }
    pub fn delete<K: AsRef<[u8]>>(&self, key: K) {
        let mut guard = self.data.borrow_mut();
        guard.remove(key.as_ref());
    }
    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        let mut guard = self.data.borrow_mut();
        guard.insert(key.as_ref().to_vec(), value.as_ref().to_vec());
    }
    pub fn batch(&self) -> Batch {
        Batch {
            data: self.data.clone(),
            delete: vec![],
            puts: vec![],
        }
    }
    #[allow(clippy::type_complexity)]
    pub fn prefix_iterator_with_skip_while_and_start<'a>(
        &'a self,
        prefix: &'a [u8],
        mode: IteratorMode<'a>,
        skip_while: Box<dyn Fn(&[u8]) -> bool + 'static>,
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        let guard = self.data.borrow();
        let iter: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match mode {
            IteratorMode::Start => Box::new(guard.iter()),
            IteratorMode::End => Box::new(guard.iter().rev()),
            IteratorMode::From(items, db_direction) => match db_direction {
                DbDirection::Forward => Box::new(
                    guard
                        .iter()
                        .skip_while(move |(key, _)| **key < items.to_vec()),
                ),
                DbDirection::Reverse => Box::new(
                    guard
                        .iter()
                        .rev()
                        .skip_while(move |(key, _)| **key > items.to_vec()),
                ),
            },
        };
        let mut result = vec![];
        for (key, value) in iter.skip_while(move |(key, _)| skip_while(&key)) {
            if !key.starts_with(prefix) {
                break;
            }
            result.push((
                key.clone().into_boxed_slice(),
                value.clone().into_boxed_slice(),
            ));
        }
        result.into_iter()
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
pub struct Batch {
    data: Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>,
    puts: Vec<KV>,
    delete: Vec<Vec<u8>>,
}
impl Batch {
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        let guard = self.data.borrow();
        guard.get(key.as_ref()).cloned()
    }

    pub fn put_kv(&mut self, key_value: KeyValue) {
        self.puts.push(KV {
            key: key_value.key(),
            value: key_value.value(),
        });
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.puts.push(KV {
            key: key.as_ref().to_vec(),
            value: value.as_ref().to_vec(),
        });
    }

    pub fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.delete.push(key.as_ref().to_vec());
    }

    pub fn commit(self) {
        let mut guard = self.data.borrow_mut();
        for key in self.delete {
            guard.remove(&key);
        }
        for KV { key, value } in self.puts {
            guard.insert(key, value);
        }
    }
}
