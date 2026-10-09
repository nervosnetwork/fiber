use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use fiber_lsp_sdk::{MemoryStore, MemoryStoreError, SignerStore};

const SNAPSHOT_FILE: &str = "snapshot.bin";

/// `MemoryStore` persisted atomically after every mutation.
pub struct FileSignerStore {
    inner: MemoryStore,
    snapshot_path: PathBuf,
    persist_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Clone for FileSignerStore {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            snapshot_path: self.snapshot_path.clone(),
            persist_lock: Arc::clone(&self.persist_lock),
        }
    }
}

impl FileSignerStore {
    /// Load an existing snapshot or create an empty store.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("create signer store directory {}", dir.display()))?;
        let snapshot_path = dir.join(SNAPSHOT_FILE);
        let inner = if snapshot_path.exists() {
            let bytes = fs::read(&snapshot_path)
                .with_context(|| format!("read {}", snapshot_path.display()))?;
            MemoryStore::from_snapshot(&bytes).context("decode signer snapshot")?
        } else {
            MemoryStore::default()
        };
        Ok(Self {
            inner,
            snapshot_path,
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    fn persist(&self) -> Result<(), MemoryStoreError> {
        let snapshot = self.inner.snapshot()?;
        let tmp = self.snapshot_path.with_extension("bin.tmp");
        fs::write(&tmp, snapshot).map_err(|error| {
            MemoryStoreError::InvalidSnapshot(format!("write {}: {error}", tmp.display()))
        })?;
        fs::rename(&tmp, &self.snapshot_path).map_err(|error| {
            MemoryStoreError::InvalidSnapshot(format!(
                "rename {} -> {}: {error}",
                tmp.display(),
                self.snapshot_path.display()
            ))
        })?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SignerStore for FileSignerStore {
    type Error = MemoryStoreError;

    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        self.inner.get(key).await
    }

    async fn insert_if_absent(&self, key: &[u8], value: &[u8]) -> Result<bool, Self::Error> {
        let _guard = self.persist_lock.lock().await;
        let inserted = self.inner.insert_if_absent(key, value).await?;
        if inserted {
            self.persist()?;
        }
        Ok(inserted)
    }

    async fn compare_and_swap(
        &self,
        key: &[u8],
        expected: Option<&[u8]>,
        replacement: &[u8],
    ) -> Result<bool, Self::Error> {
        let _guard = self.persist_lock.lock().await;
        let swapped = self
            .inner
            .compare_and_swap(key, expected, replacement)
            .await?;
        if swapped {
            self.persist()?;
        }
        Ok(swapped)
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), Self::Error> {
        let _guard = self.persist_lock.lock().await;
        self.inner.put(key, value).await?;
        self.persist()
    }

    async fn delete(&self, key: &[u8]) -> Result<(), Self::Error> {
        let _guard = self.persist_lock.lock().await;
        self.inner.delete(key).await?;
        self.persist()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSignerStore::open(dir.path()).expect("open store");
        store.put(b"key", b"value").await.expect("put");
        drop(store);

        let reopened = FileSignerStore::open(dir.path()).expect("reopen store");
        assert_eq!(
            reopened.get(b"key").await.expect("get"),
            Some(b"value".to_vec())
        );
    }

    #[test]
    fn corrupt_snapshot_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(SNAPSHOT_FILE), b"not a snapshot").expect("write corrupt file");
        let error = FileSignerStore::open(dir.path())
            .err()
            .expect("corrupt snapshot");
        assert!(error.to_string().contains("decode signer snapshot"));
    }
}
