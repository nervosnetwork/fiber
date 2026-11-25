use crate::errors::{Error, Result};
use crate::fiber::channel::ChannelActorStateStore;
use crate::store::{open_store, Store};
use fiber_types::ChannelState;
use std::path::Path;
use tracing::info;

#[allow(unused_variables)]
pub fn restore(
    restore_path: &Path,
    base_path: &Path,
    fiber_key_path: &Path,
    ckb_key_path: &Path,
    tenant_store_root: Option<&Path>,
) -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    if base_path.exists() {
        // Probe the existing store before overwriting keys or moving `base_path`
        // so a running node holding the store lock aborts restore. Drop the
        // handle before `restore_db` swaps the directory so closing it cannot
        // touch restored SST files.
        drop(fiber_store::Store::open_db(base_path).map_err(Error::DBInternalError)?);
    }
    #[cfg(not(target_arch = "wasm32"))]
    restore_node_keys(
        restore_path,
        fiber_key_path,
        ckb_key_path,
        tenant_store_root,
    )?;
    fiber_store::Store::restore_db(restore_path, base_path)?;
    let store = open_store(base_path).map_err(Error::DBInternalError)?;

    info!("Scanning stale channels.");
    mark_stale_channels(&store);
    for namespace in store.hosted_tenant_namespaces() {
        let tenant_store = store.namespaced(namespace);
        mark_stale_channels(&tenant_store);
    }
    Ok(())
}

fn mark_stale_channels(store: &Store) {
    for mut channel in store.get_all_channel_states() {
        if channel.is_risk_of_penalty() {
            channel.update_state(ChannelState::Stale);
            store.insert_channel_actor_state(channel);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn restore_node_keys(
    restore_path: &Path,
    fiber_key_path: &Path,
    ckb_key_path: &Path,
    tenant_store_root: Option<&Path>,
) -> Result<()> {
    let backup_fiber_key = restore_path.join("sk");
    let backup_ckb_key = restore_path.join("key");
    if backup_fiber_key.is_file() && backup_ckb_key.is_file() {
        copy_private_key_file(&backup_fiber_key, fiber_key_path)
            .map_err(|e| Error::DBInternalError(format!("Failed to restore fiber key: {e}")))?;
        copy_private_key_file(&backup_ckb_key, ckb_key_path)
            .map_err(|e| Error::DBInternalError(format!("Failed to restore CKB key: {e}")))?;
        info!("Successfully restored node keys from backup.");
    } else {
        info!("No node keys found in backup, skipping key restoration.");
    }
    restore_tenant_keys(restore_path, tenant_store_root)?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn restore_tenant_keys(restore_path: &Path, tenant_store_root: Option<&Path>) -> Result<()> {
    let backup_tenants_dir = restore_path.join("tenants");
    if !backup_tenants_dir.is_dir() {
        return Ok(());
    }
    let mut tenant_keys = Vec::new();
    let entries = std::fs::read_dir(&backup_tenants_dir).map_err(|e| {
        Error::DBInternalError(format!(
            "Failed to read backed up tenant keys directory {:?}: {e}",
            backup_tenants_dir
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            Error::DBInternalError(format!("Failed to read tenant backup entry: {e}"))
        })?;
        let file_type = entry.file_type().map_err(|e| {
            Error::DBInternalError(format!(
                "Failed to inspect tenant backup entry {:?}: {e}",
                entry.path()
            ))
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let src_key = entry.path().join("sk");
        if src_key.is_file() {
            tenant_keys.push((entry.file_name(), src_key));
        }
    }
    if tenant_keys.is_empty() {
        return Ok(());
    }
    let Some(tenant_store_root) = tenant_store_root else {
        return Err(Error::DBInternalError(
            "Backup contains hosted tenant keys, but LSP tenant_store_root is not configured"
                .to_string(),
        ));
    };
    create_private_dir_all(tenant_store_root).map_err(|e| {
        Error::DBInternalError(format!(
            "Failed to create tenant store root {:?}: {e}",
            tenant_store_root
        ))
    })?;
    for (tenant_dir_name, src_key) in &tenant_keys {
        let dest_dir = tenant_store_root.join(tenant_dir_name);
        create_private_dir_all(&dest_dir).map_err(|e| {
            Error::DBInternalError(format!(
                "Failed to create tenant directory {:?}: {e}",
                dest_dir
            ))
        })?;
        let dest_key = dest_dir.join("sk");
        copy_private_key_file(src_key, &dest_key).map_err(|e| {
            Error::DBInternalError(format!("Failed to restore tenant key {:?}: {e}", dest_key))
        })?;
    }
    info!(
        "Successfully restored {} hosted tenant key(s) from backup.",
        tenant_keys.len()
    );
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn copy_private_key_file(src: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            create_private_dir_all(parent)?;
        }
    }
    std::fs::copy(src, dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32"), not(feature = "sqlite")))]
mod tests {
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
}
