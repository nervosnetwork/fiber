use crate::backend::StorageBackend;
use std::fs;
#[allow(unused_imports)]
use std::path::Path;
use tempfile::tempdir;

// --- RocksDB (Native) Tests ---
#[cfg(all(
    not(target_arch = "wasm32"),
    feature = "rocksdb",
    not(feature = "sqlite")
))]
mod testrocksdb_tests {
    use super::*;
    use crate::native::Store as RocksdbStore;

    #[test]
    fn test_rocksdb_backup_and_restore() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("rocksdb_data");
        let backup_dir = dir.path().join("rocksdb_backup");

        // Initialize and put data
        let store = RocksdbStore::open_db(&db_path).expect("Open RocksDB failed");
        store.put(b"key1", b"value1");

        // Perform Backup
        // RocksDB stores actual data in backup_dir/db
        store
            .backup_now(&backup_dir)
            .expect("RocksDB backup failed");
        assert!(backup_dir.join("db").exists());

        // Simulate data change
        store.put(b"key1", b"new_value");
        assert_eq!(store.get(b"key1").unwrap(), b"new_value");

        // Perform Restore
        // In physical restore, we drop the store to release file locks
        drop(store);
        let store_for_restore = RocksdbStore::open_db(&db_path).unwrap();
        store_for_restore
            .restore(&backup_dir, &db_path)
            .expect("Restore failed");
        drop(store_for_restore);

        // Verify restored data
        let restored_store = RocksdbStore::open_db(&db_path).unwrap();
        assert_eq!(restored_store.get(b"key1").unwrap(), b"value1");
    }

    #[test]
    fn test_rocksdb_restore_rollback_mechanism() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("rocksdb_rollback");
        let invalid_backup = dir.path().join("non_existent_source");

        // Setup original database
        let store = RocksdbStore::open_db(&db_path).unwrap();
        store.put(b"critical", b"data");
        drop(store);

        // Attempt restore from invalid path
        let store_for_restore = RocksdbStore::open_db(&db_path).unwrap();
        let result = store_for_restore.restore(&invalid_backup, &db_path);
        assert!(result.is_err());
        drop(store_for_restore);

        // Verify original data was preserved (safety backup worked)
        let restored_store = RocksdbStore::open_db(&db_path).unwrap();
        assert_eq!(restored_store.get(b"critical").unwrap(), b"data");
    }

    #[test]
    fn test_perform_key_backup_logic() {
        let dir = tempdir().unwrap();
        let base_dir = dir.path();
        let backup_dir = base_dir.join("key_backup");
        fs::create_dir_all(&backup_dir).unwrap();

        // Create mock key files
        let ckb_key_path = base_dir.join("ckb_key");
        let fiber_key_path = base_dir.join("fiber_key");
        fs::write(&ckb_key_path, b"ckb_secret_content").unwrap();
        fs::write(&fiber_key_path, b"fiber_secret_content").unwrap();

        // Execute key backup
        perform_key_backup_internal(&backup_dir, &ckb_key_path, &fiber_key_path)
            .expect("Key backup failed");

        // Assert keys are copied with correct names
        assert_eq!(
            fs::read(backup_dir.join("key")).unwrap(),
            b"ckb_secret_content"
        );
        assert_eq!(
            fs::read(backup_dir.join("sk")).unwrap(),
            b"fiber_secret_content"
        );
    }

    fn perform_key_backup_internal(
        target_dir: &Path,
        ckb_key_path: &Path,
        fiber_key_path: &Path,
    ) -> Result<(), String> {
        let keys_to_copy = [(ckb_key_path, "key"), (fiber_key_path, "sk")];
        for (src_file, dest_name) in keys_to_copy {
            if src_file.exists() {
                let dest_file = target_dir.join(dest_name);
                std::fs::copy(src_file, &dest_file).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

// --- SQLite Tests ---
#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
mod sqlite_tests {
    use super::*;
    use crate::sqlite::Store as SqliteStore;

    #[test]
    fn test_sqlite_backup_and_restore() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("sqlite_data");
        let backup_dir = dir.path().join("sqlite_backup");

        // Initialize and put data
        let store = SqliteStore::open_db(&db_path).expect("Open SQLite failed");
        store.put(b"sql_key", b"sql_value");

        // Perform Backup
        // SQLite stores actual data in backup_dir/data.sqlite
        store.backup_now(&backup_dir).expect("SQLite backup failed");
        assert!(backup_dir.join("data.sqlite").exists());

        // Modify data
        store.put(b"sql_key", b"corrupted");

        // Perform Restore
        drop(store);
        let store_for_restore = SqliteStore::open_db(&db_path).unwrap();
        store_for_restore
            .restore(&backup_dir, &db_path)
            .expect("SQLite Restore failed");

        // WAL and SHM files should be cleaned up during restore
        assert!(!db_path.join("data.sqlite-wal").exists());
        assert!(!db_path.join("data.sqlite-shm").exists());

        drop(store_for_restore);

        // Verify restored data and sidecar files removal
        let restored_store = SqliteStore::open_db(&db_path).unwrap();
        assert_eq!(restored_store.get(b"sql_key").unwrap(), b"sql_value");
    }

    #[test]
    fn test_sqlite_backup_already_exists_error() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("sqlite_err");
        let backup_dir = dir.path().join("already_exists");

        fs::create_dir_all(&backup_dir).unwrap();
        let store = SqliteStore::open_db(&db_path).unwrap();

        let result = store.backup_now(&backup_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }
}
