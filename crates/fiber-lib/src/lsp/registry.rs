use bincode::{deserialize, serialize};
use fiber_store::backend::{BatchWriter, StorageBackend};
use std::sync::{Arc, Mutex};

use crate::store::{FiberStore, Store};

use super::{HostedTenantRecord, TenantId};

const TENANT_RECORD_PREFIX: &[u8] = b"\xf0lsp/tenant/";
const TENANT_NONCE_PREFIX: &[u8] = b"\xf0lsp/tenant-nonce/";
const ROOT_SIGNER_INDEX_PREFIX: &[u8] = b"\xf0lsp/root-signer/";
const TENANT_PUBKEY_INDEX_PREFIX: &[u8] = b"\xf0lsp/tenant-pubkey/";

/// Persistence interface for hosted tenant state boundaries.
pub trait TenantRegistryStore: Clone + Send + Sync + 'static {
    fn get_tenant(&self, tenant_id: &TenantId) -> Result<Option<HostedTenantRecord>, String>;
    fn put_tenant(&self, record: &HostedTenantRecord) -> Result<(), String>;
    fn list_tenants(&self) -> Result<Vec<HostedTenantRecord>, String>;
    fn get_registration_nonce(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<Option<[u8; 32]>, String>;
    fn put_registration_nonce(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
        nonce: [u8; 32],
    ) -> Result<(), String>;
    fn register_and_consume_nonce(
        &self,
        record: &HostedTenantRecord,
        expected_nonce: [u8; 32],
    ) -> Result<(), String>;
}

fn tenant_key(tenant_id: &TenantId) -> Vec<u8> {
    [TENANT_RECORD_PREFIX, tenant_id.as_str().as_bytes()].concat()
}

fn public_key_index_key(prefix: &[u8], public_key: &crate::fiber_types::Pubkey) -> Vec<u8> {
    [prefix, public_key.serialize().as_slice()].concat()
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

    fn get_registration_nonce(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<Option<[u8; 32]>, String> {
        self.get(public_key_index_key(
            TENANT_NONCE_PREFIX,
            root_signer_pubkey,
        ))
        .map(|bytes| deserialize(&bytes).map_err(|error| error.to_string()))
        .transpose()
    }

    fn put_registration_nonce(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
        nonce: [u8; 32],
    ) -> Result<(), String> {
        let bytes = serialize(&nonce).map_err(|error| error.to_string())?;
        self.put(
            public_key_index_key(TENANT_NONCE_PREFIX, root_signer_pubkey),
            bytes,
        );
        Ok(())
    }

    fn register_and_consume_nonce(
        &self,
        record: &HostedTenantRecord,
        expected_nonce: [u8; 32],
    ) -> Result<(), String> {
        let root_signer_pubkey = record.root_signer_pubkey.as_ref().ok_or_else(|| {
            "authenticated tenant record has no RootSigner public key".to_string()
        })?;
        if self.get_registration_nonce(root_signer_pubkey)? != Some(expected_nonce) {
            return Err("tenant registration nonce is missing, replaced, or consumed".to_string());
        }
        let record_bytes = serialize(record).map_err(|error| error.to_string())?;
        let tenant_id_bytes = serialize(&record.tenant_id).map_err(|error| error.to_string())?;
        let mut batch = self.batch();
        batch.put(tenant_key(&record.tenant_id), record_bytes);
        batch.put(
            public_key_index_key(ROOT_SIGNER_INDEX_PREFIX, root_signer_pubkey),
            &tenant_id_bytes,
        );
        batch.put(
            public_key_index_key(TENANT_PUBKEY_INDEX_PREFIX, &record.tenant_pubkey),
            tenant_id_bytes,
        );
        batch.delete(public_key_index_key(
            TENANT_NONCE_PREFIX,
            root_signer_pubkey,
        ));
        batch.commit();
        Ok(())
    }
}

/// Registry providing idempotent provisioning over the LSP service store.
#[derive(Clone)]
pub struct TenantRegistry<S> {
    store: S,
    registration_lock: Arc<Mutex<()>>,
}

impl<S: TenantRegistryStore> TenantRegistry<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            registration_lock: Arc::new(Mutex::new(())),
        }
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

    /// Replace the current one-time registration nonce for this RootSigner.
    pub fn issue_registration_nonce(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<[u8; 32], String> {
        let _guard = self
            .registration_lock
            .lock()
            .map_err(|_| "tenant registration lock is poisoned".to_string())?;
        let mut nonce = [0u8; 32];
        getrandom::fill(&mut nonce).map_err(|error| error.to_string())?;
        self.store
            .put_registration_nonce(root_signer_pubkey, nonce)?;
        Ok(nonce)
    }

    /// Read the current registration nonce, primarily for verification and tests.
    pub fn registration_nonce(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<Option<[u8; 32]>, String> {
        self.store.get_registration_nonce(root_signer_pubkey)
    }

    /// Atomically create an authenticated tenant and consume its one-time nonce.
    pub fn register_authenticated(
        &self,
        record: HostedTenantRecord,
        nonce: [u8; 32],
    ) -> Result<HostedTenantRecord, String> {
        let _guard = self
            .registration_lock
            .lock()
            .map_err(|_| "tenant registration lock is poisoned".to_string())?;
        let root_signer_pubkey = record.root_signer_pubkey.ok_or_else(|| {
            "authenticated tenant record has no RootSigner public key".to_string()
        })?;
        let derived = TenantId::from_root_signer_pubkey(&root_signer_pubkey);
        if record.tenant_id != derived {
            return Err("tenant id does not match its RootSigner public key".to_string());
        }
        if self.get(&record.tenant_id)?.is_some()
            || self
                .find_by_root_signer_pubkey(&root_signer_pubkey)?
                .is_some()
        {
            return Err("RootSigner is already registered; use credential recovery".to_string());
        }
        if let Some(existing) = self.find_by_tenant_pubkey(&record.tenant_pubkey)? {
            return Err(format!(
                "protocol key is already registered to tenant {}",
                existing.tenant_id
            ));
        }
        self.store.register_and_consume_nonce(&record, nonce)?;
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

    pub fn find_by_root_signer_pubkey(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<Option<HostedTenantRecord>, String> {
        Ok(self
            .list()?
            .into_iter()
            .find(|record| record.root_signer_pubkey.as_ref() == Some(root_signer_pubkey)))
    }
}
