use bincode::{deserialize, serialize};
use fiber_store::backend::StorageBackend;

use crate::store::{FiberStore, Store};

use super::{HostedTenantRecord, TenantId};

const TENANT_RECORD_PREFIX: &[u8] = b"\xf0lsp/tenant/";

/// Persistence interface for hosted tenant state boundaries.
pub trait TenantRegistryStore: Clone + Send + Sync + 'static {
    fn get_tenant(&self, tenant_id: &TenantId) -> Result<Option<HostedTenantRecord>, String>;
    fn put_tenant(&self, record: &HostedTenantRecord) -> Result<(), String>;
    fn list_tenants(&self) -> Result<Vec<HostedTenantRecord>, String>;
}

fn tenant_key(tenant_id: &TenantId) -> Vec<u8> {
    [TENANT_RECORD_PREFIX, tenant_id.as_str().as_bytes()].concat()
}

impl TenantRegistryStore for Store {
    fn get_tenant(&self, tenant_id: &TenantId) -> Result<Option<HostedTenantRecord>, String> {
        self.get(tenant_key(tenant_id))
            .map(|bytes| deserialize(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    fn put_tenant(&self, record: &HostedTenantRecord) -> Result<(), String> {
        let bytes = serialize(record).map_err(|error| error.to_string())?;
        self.put(tenant_key(&record.tenant_id), bytes);
        Ok(())
    }

    fn list_tenants(&self) -> Result<Vec<HostedTenantRecord>, String> {
        self.collect_by_prefix(TENANT_RECORD_PREFIX)
            .into_iter()
            .map(|pair| deserialize(&pair.value).map_err(|error| error.to_string()))
            .collect()
    }
}

/// Registry providing idempotent provisioning over the LSP service store.
#[derive(Clone)]
pub struct TenantRegistry<S> {
    store: S,
}

impl<S: TenantRegistryStore> TenantRegistry<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn get(&self, tenant_id: &TenantId) -> Result<Option<HostedTenantRecord>, String> {
        self.store.get_tenant(tenant_id)
    }

    pub fn register(&self, record: HostedTenantRecord) -> Result<HostedTenantRecord, String> {
        if let Some(existing) = self.get(&record.tenant_id)? {
            if existing.tenant_pubkey != record.tenant_pubkey {
                return Err(format!(
                    "tenant {} is already registered with another protocol key",
                    record.tenant_id
                ));
            }
            return Ok(existing);
        }
        if let Some(existing) = self.find_by_tenant_pubkey(&record.tenant_pubkey)? {
            return Err(format!(
                "protocol key is already registered to tenant {}",
                existing.tenant_id
            ));
        }
        self.store.put_tenant(&record)?;
        Ok(record)
    }

    pub fn list(&self) -> Result<Vec<HostedTenantRecord>, String> {
        self.store.list_tenants()
    }

    /// Bind the tenant to its single MVP private channel.
    pub fn bind_private_channel(
        &self,
        tenant_id: &TenantId,
        channel_id: crate::fiber_types::Hash256,
    ) -> Result<HostedTenantRecord, String> {
        let mut record = self
            .get(tenant_id)?
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))?;
        if record
            .private_channel_id
            .is_some_and(|existing| existing != channel_id)
        {
            return Err(format!(
                "tenant {tenant_id} is already bound to another private channel"
            ));
        }
        record.private_channel_id = Some(channel_id);
        self.store.put_tenant(&record)?;
        Ok(record)
    }

    pub fn find_by_tenant_pubkey(
        &self,
        tenant_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<Option<HostedTenantRecord>, String> {
        Ok(self
            .list()?
            .into_iter()
            .find(|record| &record.tenant_pubkey == tenant_pubkey))
    }
}
