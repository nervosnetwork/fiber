use super::KeyValue;
use super::StoreKeyValue;
pub use fiber_store::DbDirection;
pub use fiber_store::IteratorMode;
use std::fmt::Debug;
use std::path::Path;

#[derive(Clone)]
pub struct Store {
    pub(crate) inner: fiber_store::Store,
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
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        Ok(Self {
            inner: fiber_store::Store::open_db(path.as_ref())?,
        })
    }
    /// Open a store, without migration check
    pub fn open_db(path: &Path) -> Result<Self, String> {
        Self::new(path)
    }

    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.inner.get(key)
    }

    pub fn delete<K: AsRef<[u8]>>(&self, key: K) {
        self.inner.delete(key)
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        self.inner.put(key, value)
    }

    pub fn batch(&self) -> Batch {
        Batch {
            inner: self.inner.batch(),
        }
    }

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

pub struct Batch {
    inner: fiber_store::Batch,
}

impl Batch {
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.inner.get(key)
    }

    pub fn put_kv(&mut self, key_value: KeyValue) {
        self.inner.put(key_value.key(), key_value.value());
    }

    pub fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.inner.put(key, value)
    }

    pub fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.inner.delete(key)
    }

    pub fn commit(self) {
        self.inner.commit()
    }
}
