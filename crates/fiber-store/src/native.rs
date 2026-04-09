pub use rocksdb::Direction as DbDirection;
pub use rocksdb::IteratorMode;
use rocksdb::{checkpoint::Checkpoint, prelude::*, DBCompressionType, WriteBatch, DB};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::{BatchWriter, StorageBackend, TakeWhileFn};
use crate::error::StoreError;
use crate::iterator::{IteratorDirection, KVPair};
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) db: Arc<DB>,
}

impl Store {
    /// Open a store (without migration check — use `check_migrate` or `DbMigrate` for that)
    pub fn open_db(path: &Path) -> Result<Self, String> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_compression_type(DBCompressionType::Lz4);
        let db = Arc::new(DB::open(&options, path).map_err(|e| e.to_string())?);
        Ok(Self { db })
    }
}

impl StorageBackend for Store {
    type Batch = Batch;

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        self.db
            .get(key.as_ref())
            .map(|v| v.map(|vi| vi.to_vec()))
            .expect("get should be OK")
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        self.db.put(key, value).expect("put should be ok");
    }

    fn delete<K: AsRef<[u8]>>(&self, key: K) {
        self.db.delete(key).expect("Unexpected error from delete");
    }

    fn batch(&self) -> Self::Batch {
        Batch {
            db: Arc::clone(&self.db),
            wb: WriteBatch::default(),
        }
    }

    fn collect_iterator(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair> {
        let db_direction = match direction {
            IteratorDirection::Forward => DbDirection::Forward,
            IteratorDirection::Reverse => DbDirection::Reverse,
        };

        let mode = IteratorMode::From(&start, db_direction);

        let mut opts = ReadOptions::default();
        opts.set_prefix_same_as_start(true);

        let iter = self.db.get_iter(&opts, mode);

        let mut results = Vec::new();
        for (key, value) in iter {
            if !take_while_fn(&key) {
                break;
            }

            results.push(KVPair {
                key: key.to_vec(),
                value: value.to_vec(),
            });
            if limit > 0 && results.len() >= limit {
                break;
            }
        }

        results
    }

    fn backup_now(&self, path: &Path) -> Result<(), StoreError> {
        let target_dir = PathBuf::from(&path);

        // Prevent overwriting existing data
        if target_dir.exists() {
            return Err(StoreError::BackupError(format!(
                "Backup directory: {:?} already exists",
                path
            )));
        }

        if let Err(e) = std::fs::create_dir_all(&target_dir) {
            return Err(StoreError::BackupError(format!(
                "Failed to create backup directory: {}",
                e
            )));
        }
        info!("Starting node backup to: {:?}", target_dir);

        let db_backup_path = target_dir.join("db");
        let checkpoint =
            Checkpoint::new(&self.db).map_err(|e| StoreError::BackupError(e.into_string()))?;
        if let Err(e) = checkpoint.create_checkpoint(&db_backup_path) {
            return Err(StoreError::BackupError(format!(
                "Failed to create DB checkpoint: {}",
                e
            )));
        }

        info!("Backup completed successfully");

        Ok(())
    }

    /// Restore the RocksDB database from a checkpoint.
    fn restore(&self, restore_path: &Path, db_path: &Path) -> Result<(), StoreError> {
        // Usually, the actual RocksDB data is stored in a 'db' subdirectory of the backup
        let source_dir = restore_path.join("db");

        if !source_dir.exists() {
            return Err(StoreError::RestoreError(format!(
                "Restore source path does not exist: {:?}",
                source_dir
            )));
        }

        // Create a safety backup path for the current database
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Duration since unix epoch")
            .as_millis() as u64;
        let backup_path = db_path.with_extension(format!("bak.{}", now));

        info!("Starting physical database restoration.");
        info!("Source (Checkpoint): {:?}", source_dir);
        info!("Target (Current DB): {:?}", db_path);

        let mut db_was_moved = false;
        if db_path.exists() {
            info!(
                "Moving current database to safety backup: {:?}",
                backup_path
            );
            std::fs::rename(db_path, &backup_path).map_err(|e| {
                StoreError::RestoreError(format!("Failed to move current database: {}", e))
            })?;
            db_was_moved = true;
        }

        info!("Copying files from checkpoint to target...");
        if let Err(e) = copy_dir_all(&source_dir, db_path) {
            // Logging the error before starting rollback
            warn!(
                "Failed to copy checkpoint files: {}. Starting rollback...",
                e
            );

            if db_was_moved {
                // Attempt to restore the original database from the safety backup
                if let Err(rollback_err) = std::fs::rename(&backup_path, db_path) {
                    return Err(StoreError::RestoreError(format!(
                        "Critical failure during copy and subsequent rollback. Error: {}",
                        rollback_err
                    )));
                } else {
                    warn!("Rollback successful. Original database restored.");
                }
            } else {
                // If we didn't have an old DB to move, just clean up the partial copy
                let _ = std::fs::remove_dir_all(db_path);
            }

            return Err(StoreError::RestoreError(format!(
                "Failed to restore database from checkpoint: {}",
                e
            )));
        }

        info!("Physical database swap completed successfully.");
        Ok(())
    }
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    std::fs::create_dir_all(&dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

pub struct Batch {
    db: Arc<DB>,
    wb: WriteBatch,
}

impl BatchWriter for Batch {
    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.wb.put(key, value).expect("put should be OK")
    }

    fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.wb.delete(key.as_ref()).expect("delete should be OK");
    }

    fn commit(self) {
        self.db.write(&self.wb).expect("commit should be OK");
    }
}
