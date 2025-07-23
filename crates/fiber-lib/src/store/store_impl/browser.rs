#![allow(unused)]
use std::path::Path;

use super::KeyValue;

pub struct Store {}
impl Store {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        todo!()
    }
    pub fn open_db(path: &Path) -> Result<Self, String> {
        todo!()
    }
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        todo!()
    }
    pub fn delete<K: AsRef<[u8]>>(&self, key: K) {
        todo!()
    }
    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        todo!()
    }
    pub fn batch(&self) -> Batch {
        Batch {}
    }
    #[allow(clippy::type_complexity)]
    pub fn prefix_iterator_with_skip_while_and_start<'a>(
        &'a self,
        prefix: &'a [u8],
        mode: IteratorMode<'a>,
        skip_while: Box<dyn Fn(&[u8]) -> bool + 'static>,
    ) -> impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a {
        vec![].into_iter()
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
pub struct Batch {}
impl Batch {
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        todo!()
    }

    pub fn put_kv(&mut self, key_value: KeyValue) {
        todo!()
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        todo!()
    }

    pub fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        todo!()
    }

    pub fn commit(self) {
        todo!()
    }
}
pub enum IteratorMode<'a> {
    Start,
    End,
    From(&'a [u8], DbDirection),
}

pub enum DbDirection {
    Forward,
    Reverse,
}
