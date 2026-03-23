use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::backend::{BatchWriter, StorageBackend, TakeWhileFn};
use crate::iterator::{IteratorDirection, KVPair};

/// SQLite-backed key-value store.
#[derive(Clone, Debug)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
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
        let db_file = path.join("data.sqlite");
        let conn = Connection::open(&db_file).map_err(|e| e.to_string())?;

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
        })
    }
}

impl StorageBackend for Store {
    type Batch = Batch;

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
}

enum BatchOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

/// Batch writer for SQLite. Operations are collected and committed in a single transaction.
pub struct Batch {
    conn: Arc<Mutex<Connection>>,
    operations: Vec<BatchOp>,
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
