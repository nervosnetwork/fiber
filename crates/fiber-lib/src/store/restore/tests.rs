use super::*;
use fiber_store::StorageBackend;

fn create_test_backup(root: &Path) -> std::path::PathBuf {
    let src_db = root.join("src_db");
    let backup = root.join("backup");
    let store = open_store(&src_db).expect("open source store");
    store.put(b"restore-marker", b"from-backup");
    store.backup(&backup).expect("create backup");
    drop(store);

    std::fs::write(backup.join("sk"), b"backup-fiber-sk").expect("write backup sk");
    std::fs::write(backup.join("key"), b"backup-ckb-key").expect("write backup key");
    std::fs::create_dir_all(backup.join("tenants").join("u1")).expect("create tenant backup");
    std::fs::write(
        backup.join("tenants").join("u1").join("sk"),
        b"backup-u1-sk",
    )
    .expect("write backup tenant sk");
    backup
}

#[test]
fn test_restore_refuses_running_node_holding_store_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let backup = create_test_backup(root.path());

    let base_path = root.path().join("live_db");
    let fiber_key_path = root.path().join("live_sk");
    let ckb_key_path = root.path().join("live_key");
    let tenant_store_root = root.path().join("live_tenants");
    std::fs::write(&fiber_key_path, b"live-fiber-sk").expect("write live sk");
    std::fs::write(&ckb_key_path, b"live-ckb-key").expect("write live key");
    std::fs::create_dir_all(tenant_store_root.join("u1")).expect("create live tenant dir");
    std::fs::write(tenant_store_root.join("u1").join("sk"), b"live-u1-sk")
        .expect("write live tenant sk");

    let live_store = open_store(&base_path).expect("open live store");
    live_store.put(b"restore-marker", b"live-value");

    // While `live_store` holds the RocksDB LOCK, restore must fail before
    // overwriting keys or renaming the live database directory.
    let err = restore(
        &backup,
        &base_path,
        &fiber_key_path,
        &ckb_key_path,
        Some(&tenant_store_root),
    )
    .expect_err("restore must refuse a running node holding the store lock");
    assert!(
        matches!(err, Error::DBInternalError(_)),
        "expected DBInternalError on locked store, got {err:?}"
    );

    // Live database and keys must remain completely untouched.
    assert_eq!(
        live_store.get(b"restore-marker").as_deref(),
        Some(b"live-value".as_slice())
    );
    assert_eq!(std::fs::read(&fiber_key_path).unwrap(), b"live-fiber-sk");
    assert_eq!(std::fs::read(&ckb_key_path).unwrap(), b"live-ckb-key");
    assert_eq!(
        std::fs::read(tenant_store_root.join("u1").join("sk")).unwrap(),
        b"live-u1-sk"
    );
    let safety_backups: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("live_db.bak.")
        })
        .collect();
    assert!(
        safety_backups.is_empty(),
        "locked restore must not move the live store directory"
    );

    // Once the running node releases the store lock, restore succeeds.
    drop(live_store);
    restore(
        &backup,
        &base_path,
        &fiber_key_path,
        &ckb_key_path,
        Some(&tenant_store_root),
    )
    .expect("restore should succeed after live store is dropped");

    assert_eq!(std::fs::read(&fiber_key_path).unwrap(), b"backup-fiber-sk");
    assert_eq!(std::fs::read(&ckb_key_path).unwrap(), b"backup-ckb-key");
    assert_eq!(
        std::fs::read(tenant_store_root.join("u1").join("sk")).unwrap(),
        b"backup-u1-sk"
    );
    let restored_store = open_store(&base_path).expect("reopen restored store");
    assert_eq!(
        restored_store.get(b"restore-marker").as_deref(),
        Some(b"from-backup".as_slice())
    );
}

#[test]
fn test_restore_into_fresh_and_older_schema_dirs_succeeds() {
    let root = tempfile::tempdir().expect("tempdir");
    let backup = create_test_backup(root.path());

    // Restoring into a fresh non-existent directory succeeds.
    let fresh_db = root.path().join("fresh_db");
    let fresh_sk = root.path().join("fresh_sk");
    let fresh_key = root.path().join("fresh_key");
    let fresh_tenants = root.path().join("fresh_tenants");
    restore(
        &backup,
        &fresh_db,
        &fresh_sk,
        &fresh_key,
        Some(&fresh_tenants),
    )
    .expect("restore into fresh directory should succeed");
    let restored = open_store(&fresh_db).expect("open freshly restored store");
    assert_eq!(
        restored.get(b"restore-marker").as_deref(),
        Some(b"from-backup".as_slice())
    );
    drop(restored);

    // Restoring over an unlocked database with an older/unmigratable schema also succeeds
    // because the lock probe opens the raw store without running migrations.
    let old_db = root.path().join("old_schema_db");
    {
        let raw = fiber_store::Store::open_db(&old_db).expect("open raw db");
        raw.put(
            fiber_store::migration::MIGRATION_VERSION_KEY,
            "00000000000000",
        );
    }
    let old_sk = root.path().join("old_sk");
    let old_key = root.path().join("old_key");
    let old_tenants = root.path().join("old_tenants");
    restore(&backup, &old_db, &old_sk, &old_key, Some(&old_tenants))
        .expect("restore over unlocked old-schema directory should succeed");
    let restored_old = open_store(&old_db).expect("open restored store");
    assert_eq!(
        restored_old.get(b"restore-marker").as_deref(),
        Some(b"from-backup".as_slice())
    );
}

#[test]
fn test_restore_missing_tenant_store_root_leaves_existing_node_keys_untouched() {
    let root = tempfile::tempdir().expect("tempdir");
    let backup = create_test_backup(root.path());

    let base_path = root.path().join("existing_db");
    {
        let existing_store = open_store(&base_path).expect("open existing store");
        existing_store.put(b"restore-marker", b"original-db-value");
    }
    let fiber_key_path = root.path().join("existing_sk");
    let ckb_key_path = root.path().join("existing_key");
    std::fs::write(&fiber_key_path, b"original-fiber-sk").expect("write original sk");
    std::fs::write(&ckb_key_path, b"original-ckb-key").expect("write original key");

    let err = restore(&backup, &base_path, &fiber_key_path, &ckb_key_path, None)
        .expect_err("restore must fail when backup has tenant keys and tenant_store_root is None");
    assert!(
        matches!(&err, Error::DBInternalError(msg) if msg.contains("tenant_store_root")),
        "expected DBInternalError mentioning tenant_store_root, got {err:?}"
    );

    assert_eq!(
        std::fs::read(&fiber_key_path).unwrap(),
        b"original-fiber-sk",
        "fiber key must not be overwritten before tenant_store_root validation"
    );
    assert_eq!(
        std::fs::read(&ckb_key_path).unwrap(),
        b"original-ckb-key",
        "ckb key must not be overwritten before tenant_store_root validation"
    );
    let existing_store = open_store(&base_path).expect("reopen existing store");
    assert_eq!(
        existing_store.get(b"restore-marker").as_deref(),
        Some(b"original-db-value".as_slice()),
        "existing database must remain untouched when restore fails validation"
    );
}
