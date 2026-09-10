use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use thiserror::Error;

/// Async byte store used by the signer SDK.
///
/// Implementations may use memory, IndexedDB, SQLite, or another durable
/// backend. Conditional writes must be atomic for a given key.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait SignerStore: Clone + 'static {
    /// Backend-specific error type.
    type Error: std::fmt::Display;

    /// Fetch an opaque value.
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Atomically insert a value only if the key is absent.
    async fn insert_if_absent(&self, key: &[u8], value: &[u8]) -> Result<bool, Self::Error>;

    /// Atomically replace a value when its current bytes equal `expected`.
    ///
    /// `expected == None` means the key must be absent. This operation is used
    /// to ensure that user approval and signer-state updates cannot race with
    /// another signing operation.
    async fn compare_and_swap(
        &self,
        key: &[u8],
        expected: Option<&[u8]>,
        replacement: &[u8],
    ) -> Result<bool, Self::Error>;

    /// Replace an opaque value.
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), Self::Error>;

    /// Delete an opaque value.
    async fn delete(&self, key: &[u8]) -> Result<(), Self::Error>;
}

/// Error returned by the in-memory store and its snapshot codec.
#[derive(Debug, Error)]
pub enum MemoryStoreError {
    /// The in-memory lock was poisoned by a panic.
    #[error("memory signer store lock poisoned")]
    LockPoisoned,
    /// A serialized snapshot could not be decoded.
    #[error("invalid memory signer store snapshot: {0}")]
    InvalidSnapshot(String),
}

/// Byte-accurate in-memory store for tests and ephemeral signer instances.
#[derive(Clone, Default)]
pub struct MemoryStore {
    entries: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl MemoryStore {
    /// Serialize all store entries so restart recovery can be tested.
    pub fn snapshot(&self) -> Result<Vec<u8>, MemoryStoreError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| MemoryStoreError::LockPoisoned)?;
        bincode::serialize(&*entries)
            .map_err(|error| MemoryStoreError::InvalidSnapshot(error.to_string()))
    }

    /// Restore an in-memory store from a previous snapshot.
    pub fn from_snapshot(snapshot: &[u8]) -> Result<Self, MemoryStoreError> {
        let entries = bincode::deserialize(snapshot)
            .map_err(|error| MemoryStoreError::InvalidSnapshot(error.to_string()))?;
        Ok(Self {
            entries: Arc::new(Mutex::new(entries)),
        })
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl SignerStore for MemoryStore {
    type Error = MemoryStoreError;

    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| MemoryStoreError::LockPoisoned)?;
        Ok(entries.get(key).cloned())
    }

    async fn insert_if_absent(&self, key: &[u8], value: &[u8]) -> Result<bool, Self::Error> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| MemoryStoreError::LockPoisoned)?;
        if entries.contains_key(key) {
            return Ok(false);
        }
        entries.insert(key.to_vec(), value.to_vec());
        Ok(true)
    }

    async fn compare_and_swap(
        &self,
        key: &[u8],
        expected: Option<&[u8]>,
        replacement: &[u8],
    ) -> Result<bool, Self::Error> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| MemoryStoreError::LockPoisoned)?;
        if entries.get(key).map(Vec::as_slice) != expected {
            return Ok(false);
        }
        entries.insert(key.to_vec(), replacement.to_vec());
        Ok(true)
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), Self::Error> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| MemoryStoreError::LockPoisoned)?;
        entries.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    async fn delete(&self, key: &[u8]) -> Result<(), Self::Error> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| MemoryStoreError::LockPoisoned)?;
        entries.remove(key);
        Ok(())
    }
}
