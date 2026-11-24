use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::backend::{BatchWriter, StorageBackend, TakeWhileFn};
use crate::error::StoreError;
use crate::iterator::{IteratorDirection, KVPair};

/// SQLite-backed key-value store.
#[derive(Clone, Debug)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    #[cfg(feature = "simulation")]
    batch_faults: Option<crate::simulation::BatchFaults>,
}

impl Store {
    /// Open a SQLite store (without migration check).
    ///
    /// `path` is treated as a directory (matching the RocksDB backend convention).
    /// The actual database file is created as `<path>/data.sqlite`.
    pub fn open_db(path: &Path) -> Result<Self, String> {
        // Ensure the directory exists
        std::fs::create_dir_all(path)
            .map_err(|e| format!("failed to create database directory: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        }
        let db_file = path.join("data.sqlite");
        #[cfg(feature = "simulation")]
        let (connection, batch_faults) = match crate::simulation::vfs_for(path)? {
            Some((vfs, batch_faults)) => (
                Connection::open_with_flags_and_vfs(&db_file, rusqlite::OpenFlags::default(), vfs),
                Some(batch_faults),
            ),
            None => (Connection::open(&db_file), None),
        };
        #[cfg(not(feature = "simulation"))]
        let connection = Connection::open(&db_file);
        let conn = connection.map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&db_file, std::fs::Permissions::from_mode(0o600));
        }

        // Configure SQLite for performance
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA mmap_size = 268435456;
             PRAGMA cache_size = -65536;",
        )
        .map_err(|e| e.to_string())?;

        // Create the key-value table.
        // WITHOUT ROWID: stores rows directly in the PRIMARY KEY B-tree,
        // eliminating the hidden rowid column and its separate B-tree.
        // This is ideal for a key-value table where the key is always a BLOB.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv_store (
                 key BLOB PRIMARY KEY NOT NULL,
                 value BLOB NOT NULL
             ) WITHOUT ROWID;",
        )
        .map_err(|e| e.to_string())?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            #[cfg(feature = "simulation")]
            batch_faults,
        })
    }

    /// Whether each commit waits for the disk (`synchronous = FULL`). Off when opened: in WAL
    /// mode `NORMAL` may lose the last commits on a power loss or a hard reboot.
    pub fn set_sync_writes(&mut self, sync_writes: bool) {
        let level = if sync_writes { "FULL" } else { "NORMAL" };
        self.conn
            .lock()
            .expect("lock poisoned")
            .execute_batch(&format!("PRAGMA synchronous = {level};"))
            .expect("set synchronous should be OK");
    }
}

impl StorageBackend for Store {
    type Batch = Batch;

    fn sync_writes(&self) -> bool {
        let level: i64 = self
            .conn
            .lock()
            .expect("lock poisoned")
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("read synchronous should be OK");
        // 2 is FULL and 3 is EXTRA.
        level >= 2
    }

    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>> {
        let conn = self.conn.lock().expect("lock poisoned");
        match conn.query_row(
            "SELECT value FROM kv_store WHERE key = ?1",
            [key.as_ref()],
            |row| row.get(0),
        ) {
            Ok(value) => Some(value),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => panic!("get failed: {e}"),
        }
    }

    fn multi_get<K: AsRef<[u8]>>(&self, keys: &[K]) -> Vec<Option<Vec<u8>>> {
        if keys.is_empty() {
            return Vec::new();
        }
        // One statement over all keys, so the values come from one database state even if a commit
        // lands right after. The connection is shared through this lock because `Connection` is not
        // `Sync`, and commits take the same lock, so a batch is never observed half applied.
        let conn = self.conn.lock().expect("lock poisoned");
        let placeholders = vec!["?"; keys.len()].join(",");
        let mut statement = conn
            .prepare(&format!(
                "SELECT key, value FROM kv_store WHERE key IN ({placeholders})"
            ))
            .expect("prepare should be OK");
        // SQLite returns the rows in its own order, so map them back onto the requested order.
        let found: std::collections::HashMap<Vec<u8>, Vec<u8>> = statement
            .query_map(
                rusqlite::params_from_iter(keys.iter().map(|key| key.as_ref())),
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .expect("query should be OK")
            .collect::<Result<_, _>>()
            .expect("row should be OK");
        keys.iter()
            .map(|key| found.get(key.as_ref()).cloned())
            .collect()
    }

    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) {
        let conn = self.conn.lock().expect("lock poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO kv_store (key, value) VALUES (?1, ?2)",
            rusqlite::params![key.as_ref(), value.as_ref()],
        )
        .expect("put should be ok");
    }

    fn delete<K: AsRef<[u8]>>(&self, key: K) {
        let conn = self.conn.lock().expect("lock poisoned");
        conn.execute("DELETE FROM kv_store WHERE key = ?1", [key.as_ref()])
            .expect("delete should be ok");
    }

    fn batch(&self) -> Self::Batch {
        Batch {
            conn: Arc::clone(&self.conn),
            operations: Vec::new(),
            #[cfg(feature = "simulation")]
            batch_faults: self.batch_faults.clone(),
        }
    }

    fn collect_iterator(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair> {
        let conn = self.conn.lock().expect("lock poisoned");

        // Use LIMIT ?2 as a bind parameter so SQLite can cache the prepared
        // statement across calls with different limit values.  A limit of -1
        // means "no limit" in SQLite, so we map 0 (our "unlimited" sentinel)
        // to -1.
        let sql = match direction {
            IteratorDirection::Forward => {
                "SELECT key, value FROM kv_store WHERE key >= ?1 ORDER BY key ASC LIMIT ?2"
            }
            IteratorDirection::Reverse => {
                "SELECT key, value FROM kv_store WHERE key <= ?1 ORDER BY key DESC LIMIT ?2"
            }
        };
        let sql_limit: i64 = if limit == 0 { -1 } else { limit as i64 };

        let mut stmt = conn.prepare(sql).expect("prepare should be ok");
        let mut rows = stmt
            .query(rusqlite::params![start.as_slice(), sql_limit])
            .expect("query should be ok");

        // Consume rows lazily — take_while is applied as we iterate and
        // LIMIT is enforced by SQLite, so we never materialize more rows
        // than needed.
        let mut results = Vec::new();
        while let Some(row) = rows.next().expect("row read failed") {
            let key: Vec<u8> = row.get(0).expect("get key failed");
            if !take_while_fn(&key) {
                break;
            }
            let value: Vec<u8> = row.get(1).expect("get value failed");
            results.push(KVPair { key, value });
        }
        results
    }

    fn backup(&self, path: &Path) -> Result<(), StoreError> {
        let target_dir = PathBuf::from(&path);

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

        let db_file_path = target_dir.join("data.sqlite");
        let db_file_str = db_file_path
            .to_str()
            .ok_or_else(|| StoreError::BackupError("Invalid backup path encoding".to_string()))?;

        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::BackupError("lock poisoned".to_string()))?;

        conn.execute("VACUUM INTO ?1", [db_file_str])
            .map_err(|e| StoreError::BackupError(format!("SQLite VACUUM INTO failed: {}", e)))?;

        Ok(())
    }

    fn restore(&self, restore_path: &Path, db_path: &Path) -> Result<(), StoreError> {
        Self::restore_db(restore_path, db_path)
    }
}

impl Store {
    /// Restore the SQLite database from a backup without opening a live handle on
    /// `db_path` first.
    pub fn restore_db(restore_path: &Path, db_path: &Path) -> Result<(), StoreError> {
        // Restore source: restore_path/data.sqlite
        let source_file = restore_path.join("data.sqlite");
        // Target db file: db_path/data.sqlite
        let target_file = db_path.join("data.sqlite");

        if !source_file.exists() || !source_file.is_file() {
            return Err(StoreError::RestoreError(format!(
                "Backup source file not found at: {:?}",
                source_file
            )));
        }

        if !db_path.exists() {
            std::fs::create_dir_all(db_path).map_err(|e| {
                StoreError::RestoreError(format!("Failed to create DB directory: {}", e))
            })?;
        }

        if target_file.exists() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Duration since unix epoch")
                .as_millis() as u64;
            // Prevent accidental deletion of the current database
            let backup_name = format!("data.sqlite.bak.{}", now);
            let safety_bak_path = db_path.join(backup_name);

            std::fs::rename(&target_file, &safety_bak_path).map_err(|e| {
                StoreError::RestoreError(format!("Failed to backup current DB: {}", e))
            })?;

            // Old logs and indexes must be deleted to prevent offset conflicts when the new database is started.
            let wal_file = db_path.join("data.sqlite-wal");
            let shm_file = db_path.join("data.sqlite-shm");

            if wal_file.exists() {
                let _ = std::fs::remove_file(wal_file);
            }
            if shm_file.exists() {
                let _ = std::fs::remove_file(shm_file);
            }
        }

        std::fs::copy(&source_file, &target_file).map_err(|e| {
            StoreError::RestoreError(format!("Failed to copy database file: {}", e))
        })?;

        Ok(())
    }
}

enum BatchOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

/// Batch writer for SQLite. Operations are collected and committed in a single transaction.
pub struct Batch {
    conn: Arc<Mutex<Connection>>,
    operations: Vec<BatchOp>,
    #[cfg(feature = "simulation")]
    batch_faults: Option<crate::simulation::BatchFaults>,
}

impl BatchWriter for Batch {
    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V) {
        self.operations.push(BatchOp::Put {
            key: key.as_ref().to_vec(),
            value: value.as_ref().to_vec(),
        });
    }

    fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
        self.operations.push(BatchOp::Delete {
            key: key.as_ref().to_vec(),
        });
    }

    fn commit(self) {
        let mut conn = self.conn.lock().expect("lock poisoned");
        #[cfg(feature = "simulation")]
        let _batch_scope = self.batch_faults.as_ref().and_then(|faults| {
            faults.enter(|target| {
                target.put_prefix.as_ref().is_none_or(|prefix| {
                    self.operations
                        .iter()
                        .any(|op| matches!(op, BatchOp::Put { key, .. } if key.starts_with(prefix)))
                }) && target.delete_prefix.as_ref().is_none_or(|prefix| {
                    self.operations
                        .iter()
                        .any(|op| matches!(op, BatchOp::Delete { key } if key.starts_with(prefix)))
                })
            })
        });
        let tx = conn.transaction().expect("begin transaction");
        for op in &self.operations {
            match op {
                BatchOp::Put { key, value } => {
                    tx.execute(
                        "INSERT OR REPLACE INTO kv_store (key, value) VALUES (?1, ?2)",
                        rusqlite::params![key, value],
                    )
                    .expect("put should be ok");
                }
                BatchOp::Delete { key } => {
                    tx.execute("DELETE FROM kv_store WHERE key = ?1", [key.as_slice()])
                        .expect("delete should be ok");
                }
            }
        }
        tx.commit().expect("commit transaction");
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    #[test]
    fn multi_get_returns_values_in_the_requested_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_db(dir.path()).unwrap();
        let mut batch = store.batch();
        batch.put(b"b", b"second");
        batch.put(b"a", b"first");
        batch.commit();

        // Missing keys stay in place, and the result follows the request order rather than the
        // order SQLite happens to return.
        assert_eq!(
            store.multi_get(&[b"b".as_slice(), b"missing", b"a"]),
            vec![Some(b"second".to_vec()), None, Some(b"first".to_vec()),]
        );
        assert!(store.multi_get::<&[u8]>(&[]).is_empty());
    }
}
