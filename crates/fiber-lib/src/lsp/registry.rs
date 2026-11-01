use bincode::{deserialize, serialize};
use fiber_store::backend::{BatchWriter, StorageBackend};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::store::{FiberStore, Store};

use super::{HostedTenantRecord, TenantChannelRecord, TenantId};

const TENANT_RECORD_PREFIX: &[u8] = b"\xf0lsp/tenant/";
const TENANT_REQUEST_PREFIX: &[u8] = b"\xf0lsp/tenant-request/";
const ROOT_SIGNER_INDEX_PREFIX: &[u8] = b"\xf0lsp/root-signer/";
const TENANT_PUBKEY_INDEX_PREFIX: &[u8] = b"\xf0lsp/tenant-pubkey/";
const REGISTRATION_REQUEST_CONSUMED: &str = "tenant registration request was already consumed";
const TENANT_CHANNEL_PREFIX: &[u8] = b"\xf0lsp/tenant-channel/";

/// Persistence interface for hosted tenant state boundaries.
pub trait TenantRegistryStore: Clone + Send + Sync + 'static {
    fn get_tenant(&self, tenant_id: &TenantId) -> Result<Option<HostedTenantRecord>, String>;
    fn put_tenant(&self, record: &HostedTenantRecord) -> Result<(), String>;
    fn list_tenants(&self) -> Result<Vec<HostedTenantRecord>, String>;
    fn get_tenant_channel(
        &self,
        tenant_id: &TenantId,
        channel_id: &crate::fiber_types::Hash256,
    ) -> Result<Option<TenantChannelRecord>, String>;
    fn put_tenant_channel(
        &self,
        tenant_id: &TenantId,
        channel: &TenantChannelRecord,
    ) -> Result<(), String>;
    fn list_tenant_channels(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<TenantChannelRecord>, String>;
    fn registration_request_consumed(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
        request_id: &[u8; 32],
    ) -> Result<bool, String>;
    fn consume_registration_request(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
        request_id: &[u8; 32],
    ) -> Result<(), String>;
    fn register_and_consume_request(
        &self,
        record: &HostedTenantRecord,
        request_id: &[u8; 32],
    ) -> Result<(), String>;
}

fn tenant_key(tenant_id: &TenantId) -> Vec<u8> {
    [TENANT_RECORD_PREFIX, tenant_id.as_str().as_bytes()].concat()
}

fn public_key_index_key(prefix: &[u8], public_key: &crate::fiber_types::Pubkey) -> Vec<u8> {
    [prefix, public_key.serialize().as_slice()].concat()
}

fn registration_request_key(
    root_signer_pubkey: &crate::fiber_types::Pubkey,
    request_id: &[u8; 32],
) -> Vec<u8> {
    [
        TENANT_REQUEST_PREFIX,
        root_signer_pubkey.serialize().as_slice(),
        request_id.as_slice(),
    ]
    .concat()
}

fn tenant_channel_prefix(tenant_id: &TenantId) -> Vec<u8> {
    [TENANT_CHANNEL_PREFIX, tenant_id.as_str().as_bytes(), b"/"].concat()
}

fn tenant_channel_key(tenant_id: &TenantId, channel_id: &crate::fiber_types::Hash256) -> Vec<u8> {
    [
        tenant_channel_prefix(tenant_id).as_slice(),
        channel_id.as_ref(),
    ]
    .concat()
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

    fn get_tenant_channel(
        &self,
        tenant_id: &TenantId,
        channel_id: &crate::fiber_types::Hash256,
    ) -> Result<Option<TenantChannelRecord>, String> {
        self.get(tenant_channel_key(tenant_id, channel_id))
            .map(|bytes| deserialize(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    fn put_tenant_channel(
        &self,
        tenant_id: &TenantId,
        channel: &TenantChannelRecord,
    ) -> Result<(), String> {
        let bytes = serialize(channel).map_err(|error| error.to_string())?;
        self.put(tenant_channel_key(tenant_id, &channel.channel_id), bytes);
        Ok(())
    }

    fn list_tenant_channels(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<TenantChannelRecord>, String> {
        self.collect_by_prefix(&tenant_channel_prefix(tenant_id))
            .into_iter()
            .map(|pair| deserialize(&pair.value).map_err(|error| error.to_string()))
            .collect()
    }

    fn registration_request_consumed(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
        request_id: &[u8; 32],
    ) -> Result<bool, String> {
        Ok(self
            .get(registration_request_key(root_signer_pubkey, request_id))
            .is_some())
    }

    fn consume_registration_request(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
        request_id: &[u8; 32],
    ) -> Result<(), String> {
        let key = registration_request_key(root_signer_pubkey, request_id);
        if self.get(key.clone()).is_some() {
            return Err(REGISTRATION_REQUEST_CONSUMED.to_string());
        }
        let mut batch = self.batch();
        batch.put(key, []);
        batch.commit();
        Ok(())
    }

    fn register_and_consume_request(
        &self,
        record: &HostedTenantRecord,
        request_id: &[u8; 32],
    ) -> Result<(), String> {
        let root_signer_pubkey = record.root_signer_pubkey.as_ref().ok_or_else(|| {
            "authenticated tenant record has no RootSigner public key".to_string()
        })?;
        let request_key = registration_request_key(root_signer_pubkey, request_id);
        if self.get(request_key.clone()).is_some() {
            return Err(REGISTRATION_REQUEST_CONSUMED.to_string());
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
        batch.put(request_key, []);
        batch.commit();
        Ok(())
    }
}

/// Tenants by protocol key and by RootSigner key, read from the store once and kept with each
/// registration. Without it a lookup or a count deserialized every tenant record on the service
/// actor: about 50 ms each with 10,000 tenants, and channels going online or offline look a tenant
/// up by key.
#[derive(Default)]
struct TenantIndex {
    by_tenant_pubkey: HashMap<crate::fiber_types::Pubkey, TenantId>,
    by_root_signer_pubkey: HashMap<crate::fiber_types::Pubkey, TenantId>,
}

impl TenantIndex {
    fn insert(&mut self, record: &HostedTenantRecord) {
        self.by_tenant_pubkey
            .insert(record.tenant_pubkey, record.tenant_id.clone());
        if let Some(root) = record.root_signer_pubkey {
            self.by_root_signer_pubkey
                .insert(root, record.tenant_id.clone());
        }
    }
}

/// Registry providing idempotent provisioning over the LSP service store.
#[derive(Clone)]
pub struct TenantRegistry<S> {
    store: S,
    registration_lock: Arc<Mutex<()>>,
    // Built on first use, so a store that cannot be read is an error there and not an empty index.
    index: Arc<RwLock<Option<TenantIndex>>>,
}

impl<S: TenantRegistryStore> TenantRegistry<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            registration_lock: Arc::new(Mutex::new(())),
            index: Arc::new(RwLock::new(None)),
        }
    }

    fn with_index<R>(&self, read: impl FnOnce(&TenantIndex) -> R) -> Result<R, String> {
        {
            let index = self
                .index
                .read()
                .map_err(|_| "tenant index lock is poisoned".to_string())?;
            if let Some(index) = index.as_ref() {
                return Ok(read(index));
            }
        }
        let mut index = self
            .index
            .write()
            .map_err(|_| "tenant index lock is poisoned".to_string())?;
        if index.is_none() {
            let mut built = TenantIndex::default();
            for record in self.store.list_tenants()? {
                built.insert(&record);
            }
            *index = Some(built);
        }
        Ok(read(index.as_ref().expect("built above")))
    }

    fn index_insert(&self, record: &HostedTenantRecord) -> Result<(), String> {
        // Building it reads the store, which holds the record already; inserting again is a no-op.
        self.with_index(|_| ())?;
        let mut index = self
            .index
            .write()
            .map_err(|_| "tenant index lock is poisoned".to_string())?;
        if let Some(index) = index.as_mut() {
            index.insert(record);
        }
        Ok(())
    }

    /// How many tenants are registered, without reading their records.
    pub fn count(&self) -> Result<usize, String> {
        self.with_index(|index| index.by_tenant_pubkey.len())
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
        self.index_insert(&record)?;
        Ok(record)
    }

    fn ensure_registration_request_fresh(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
        request_id: &[u8; 32],
    ) -> Result<(), String> {
        if self
            .store
            .registration_request_consumed(root_signer_pubkey, request_id)?
        {
            return Err(REGISTRATION_REQUEST_CONSUMED.to_string());
        }
        Ok(())
    }

    /// Atomically create an authenticated tenant and consume its request id.
    pub fn register_authenticated(
        &self,
        record: HostedTenantRecord,
        request_id: [u8; 32],
    ) -> Result<HostedTenantRecord, String> {
        let _guard = self
            .registration_lock
            .lock()
            .map_err(|_| "tenant registration lock is poisoned".to_string())?;
        let root_signer_pubkey = record.root_signer_pubkey.ok_or_else(|| {
            "authenticated tenant record has no RootSigner public key".to_string()
        })?;
        self.ensure_registration_request_fresh(&root_signer_pubkey, &request_id)?;
        let derived = TenantId::from_root_signer_pubkey(&root_signer_pubkey);
        if record.tenant_id != derived {
            return Err("tenant id does not match its RootSigner public key".to_string());
        }
        if self.get(&record.tenant_id)?.is_some()
            || self
                .find_by_root_signer_pubkey(&root_signer_pubkey)?
                .is_some()
        {
            return Err("RootSigner is already registered; use credential refresh".to_string());
        }
        if let Some(existing) = self.find_by_tenant_pubkey(&record.tenant_pubkey)? {
            return Err(format!(
                "protocol key is already registered to tenant {}",
                existing.tenant_id
            ));
        }
        self.store
            .register_and_consume_request(&record, &request_id)?;
        self.index_insert(&record)?;
        Ok(record)
    }

    /// Refresh credentials for an existing tenant after verifying a fresh RootSigner proof.
    ///
    /// Consume the request id without replacing protocol keys, channel bindings, or
    /// creation metadata. A consumed proof cannot be reused to mint another token.
    pub fn refresh_authenticated(
        &self,
        root_signer_pubkey: crate::fiber_types::Pubkey,
        request_id: [u8; 32],
    ) -> Result<HostedTenantRecord, String> {
        let _guard = self
            .registration_lock
            .lock()
            .map_err(|_| "tenant registration lock is poisoned".to_string())?;
        self.ensure_registration_request_fresh(&root_signer_pubkey, &request_id)?;
        let tenant_id = TenantId::from_root_signer_pubkey(&root_signer_pubkey);
        let record = self
            .get(&tenant_id)?
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))?;
        if record.root_signer_pubkey != Some(root_signer_pubkey) {
            return Err("tenant is not registered to this RootSigner".to_string());
        }
        self.store
            .consume_registration_request(&root_signer_pubkey, &request_id)?;
        Ok(record)
    }

    pub fn list(&self) -> Result<Vec<HostedTenantRecord>, String> {
        self.store.list_tenants()
    }

    /// Bind one of the tenant's private channels.
    ///
    /// A tenant may bind multiple channels, one per asset (and eventually more
    /// than one for the same asset). Re-binding the same channel id is
    /// idempotent as long as the asset does not change.
    pub fn bind_channel(
        &self,
        tenant_id: &TenantId,
        channel_id: crate::fiber_types::Hash256,
        funding_udt_type_script: Option<ckb_types::packed::Script>,
    ) -> Result<HostedTenantRecord, String> {
        let record = self
            .get(tenant_id)?
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))?;
        if let Some(existing) = self.store.get_tenant_channel(tenant_id, &channel_id)? {
            if existing.funding_udt_type_script != funding_udt_type_script {
                return Err(format!(
                    "tenant {tenant_id} channel {channel_id} is already bound with a different asset"
                ));
            }
        } else {
            self.store.put_tenant_channel(
                tenant_id,
                &TenantChannelRecord {
                    channel_id,
                    funding_udt_type_script,
                    bound_at: crate::now_timestamp_as_millis_u64(),
                },
            )?;
        }
        Ok(record)
    }

    pub fn list_channels(&self, tenant_id: &TenantId) -> Result<Vec<TenantChannelRecord>, String> {
        self.store.list_tenant_channels(tenant_id)
    }

    pub fn find_by_tenant_pubkey(
        &self,
        tenant_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<Option<HostedTenantRecord>, String> {
        let Some(tenant_id) =
            self.with_index(|index| index.by_tenant_pubkey.get(tenant_pubkey).cloned())?
        else {
            return Ok(None);
        };
        Ok(self
            .get(&tenant_id)?
            .filter(|record| &record.tenant_pubkey == tenant_pubkey))
    }

    pub fn find_by_root_signer_pubkey(
        &self,
        root_signer_pubkey: &crate::fiber_types::Pubkey,
    ) -> Result<Option<HostedTenantRecord>, String> {
        let Some(tenant_id) =
            self.with_index(|index| index.by_root_signer_pubkey.get(root_signer_pubkey).cloned())?
        else {
            return Ok(None);
        };
        Ok(self
            .get(&tenant_id)?
            .filter(|record| record.root_signer_pubkey.as_ref() == Some(root_signer_pubkey)))
    }
}
