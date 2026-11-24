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
