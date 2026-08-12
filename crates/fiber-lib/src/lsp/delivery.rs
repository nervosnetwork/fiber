use bincode::{deserialize, serialize};
use fiber_store::backend::{BatchWriter, StorageBackend};
use serde::{Deserialize, Serialize};

use crate::fiber_types::{Hash256, PaymentStatus, TlcErrorCode};
use crate::store::{FiberStore, Store};

use super::{LspInvoiceRegistration, TenantId, TrampolineForwardingRequest};

const PAYMENT_DELIVERY_PREFIX: &[u8] = b"\xf2lsp/delivery/";
const PAYMENT_DELIVERY_HASH_INDEX_PREFIX: &[u8] = b"\xf2lsp/delivery-by-hash/";
/// Time retained between the end of buffering and the downstream expiry budget.
pub const LSP_DELIVERY_SAFETY_MARGIN_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug)]
pub struct LspPaymentDeliveryLimits {
    pub max_pending_deliveries: usize,
    pub max_pending_deliveries_per_tenant: usize,
}

/// Identifies one concrete incoming TLC handled by the hosted delivery service.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct LspPaymentDeliveryKey {
    /// Channel on which Public T received the upstream TLC.
    pub incoming_channel_id: Hash256,
    /// Identifier of the upstream TLC within `incoming_channel_id`.
    pub incoming_tlc_id: u64,
}

impl LspPaymentDeliveryKey {
    /// Derives the execution identity from the request's upstream TLC.
    pub fn from_request(request: &TrampolineForwardingRequest) -> Self {
        Self {
            incoming_channel_id: request.previous_tlc.prev_channel_id,
            incoming_tlc_id: request.previous_tlc.prev_tlc_id,
        }
    }
}

impl Default for LspPaymentDeliveryLimits {
    fn default() -> Self {
        Self {
            max_pending_deliveries: super::config::DEFAULT_MAX_PENDING_DELIVERIES,
            max_pending_deliveries_per_tenant:
                super::config::DEFAULT_MAX_PENDING_DELIVERIES_PER_TENANT,
        }
    }
}

/// Durable lifecycle of one hosted incoming payment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LspPaymentDeliveryStatus {
    /// The upstream TLC is buffered, waiting for the tenant to become ready or for a retry.
    Deferred,
    /// The service is creating the downstream payment.
    ///
    /// Persisting this state before dispatch prevents duplicate payments after a restart.
    Dispatching,
    /// The downstream payment exists and is still running.
    InFlight,
    /// The downstream payment succeeded and the upstream TLC was fulfilled. Final state.
    Succeeded,
    /// Delivery ended unsuccessfully. Final state.
    Failed { reason: String },
    /// The downstream result is known, but Public T is still fulfilling or failing the upstream
    /// TLC.
    ///
    /// This variant is appended to preserve the bincode discriminants of records written by
    /// earlier hosted-LSP prototypes.
    SettlingUpstream {
        payment_status: PaymentStatus,
        failure: Option<String>,
    },
}

impl LspPaymentDeliveryStatus {
    /// Checks whether `next` is a valid state transition.
    ///
    /// Upstream and downstream are named relative to Public T:
    ///
    /// ```text
    /// Payer P                   Public T / LSP                 Hosted Tenant U
    ///    |                            |                               |
    ///    |------ upstream TLC ------->|                               |
    ///    |                            |  Deferred                     |
    ///    |                            |  Dispatching                  |
    ///    |                            |------ downstream payment ---->|
    ///    |                            |                               |
    ///    |                            |            InFlight           |
    ///    |                            |                               |
    ///    |                            |<----- preimage / failure -----|
    ///    |                            |  SettlingUpstream             |
    ///    |<-- fulfill / fail TLC -----|                               |
    ///    |                            |  Succeeded / Failed           |
    /// ```
    ///
    /// Transient dispatch or payment failures return to `Deferred` for retry. If the upstream TLC
    /// disappears before dispatch, the delivery transitions directly to `Failed`.
    ///
    /// Buffer timeout and permanent errors enter `SettlingUpstream(Failed)` while Public T fails
    /// the upstream TLC. Recovery may enter `SettlingUpstream` directly or return from it to
    /// `InFlight` when a concurrent downstream payment is discovered.
    ///
    /// Re-entering the same state is handled as an idempotent update by the delivery manager and
    /// therefore is not considered a transition here. Final states have no outgoing transitions.
    pub(crate) fn check_next_valid(&self, next: &Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Deferred,
                Self::Dispatching | Self::Failed { .. } | Self::SettlingUpstream { .. }
            ) | (
                Self::Dispatching,
                Self::Deferred
                    | Self::InFlight
                    | Self::Failed { .. }
                    | Self::SettlingUpstream { .. }
            ) | (
                Self::InFlight,
                Self::Deferred | Self::SettlingUpstream { .. }
            ) | (
                Self::SettlingUpstream { .. },
                Self::InFlight | Self::Succeeded | Self::Failed { .. }
            )
        )
    }

    pub fn is_final(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed { .. })
    }
}

/// Durable record that keeps the upstream TLC recoverable across process restart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LspPaymentDelivery {
    pub payment_hash: Hash256,
    pub tenant_id: TenantId,
    pub private_channel_id: Hash256,
    pub request: TrampolineForwardingRequest,
    pub buffer_deadline: u64,
    pub status: LspPaymentDeliveryStatus,
    /// Number of times Public T has tried to create the downstream payment.
    pub attempt_count: u64,
    /// Most recent downstream dispatch or payment error.
    pub last_error: Option<String>,
    /// Structured TLC failure code paired with `last_error`, when available.
    pub last_error_code: Option<TlcErrorCode>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl LspPaymentDelivery {
    pub fn key(&self) -> LspPaymentDeliveryKey {
        LspPaymentDeliveryKey::from_request(&self.request)
    }
}

/// Persistence interface for hosted payment delivery records.
pub trait LspPaymentDeliveryStore: Clone + Send + Sync + 'static {
    fn get_lsp_payment_delivery(
        &self,
        key: &LspPaymentDeliveryKey,
    ) -> Result<Option<LspPaymentDelivery>, String>;
    fn put_lsp_payment_delivery(&self, delivery: &LspPaymentDelivery) -> Result<(), String>;
    fn list_lsp_payment_deliveries(&self) -> Result<Vec<LspPaymentDelivery>, String>;
    fn list_lsp_payment_deliveries_by_payment_hash(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Vec<LspPaymentDelivery>, String>;
}

fn append_execution_key(bytes: &mut Vec<u8>, key: &LspPaymentDeliveryKey) {
    bytes.extend_from_slice(key.incoming_channel_id.as_ref());
    bytes.extend_from_slice(&key.incoming_tlc_id.to_be_bytes());
}

fn delivery_key(key: &LspPaymentDeliveryKey) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(PAYMENT_DELIVERY_PREFIX.len() + 40);
    bytes.extend_from_slice(PAYMENT_DELIVERY_PREFIX);
    append_execution_key(&mut bytes, key);
    bytes
}

fn payment_hash_index_prefix(payment_hash: &Hash256) -> Vec<u8> {
    [PAYMENT_DELIVERY_HASH_INDEX_PREFIX, payment_hash.as_ref()].concat()
}

fn payment_hash_index_key(payment_hash: &Hash256, key: &LspPaymentDeliveryKey) -> Vec<u8> {
    let mut bytes = payment_hash_index_prefix(payment_hash);
    append_execution_key(&mut bytes, key);
    bytes
}

fn parse_indexed_execution_key(bytes: &[u8]) -> Result<LspPaymentDeliveryKey, String> {
    let key_offset = PAYMENT_DELIVERY_HASH_INDEX_PREFIX.len() + 32;
    if bytes.len() != key_offset + 40 {
        return Err(format!(
            "invalid hosted payment delivery index key length: {}",
            bytes.len()
        ));
    }
    let incoming_channel_id = Hash256::try_from(&bytes[key_offset..key_offset + 32])
        .map_err(|error| error.to_string())?;
    let incoming_tlc_id = u64::from_be_bytes(
        bytes[key_offset + 32..]
            .try_into()
            .map_err(|error: std::array::TryFromSliceError| error.to_string())?,
    );
    Ok(LspPaymentDeliveryKey {
        incoming_channel_id,
        incoming_tlc_id,
    })
}

impl LspPaymentDeliveryStore for Store {
    fn get_lsp_payment_delivery(
        &self,
        key: &LspPaymentDeliveryKey,
    ) -> Result<Option<LspPaymentDelivery>, String> {
        self.get(delivery_key(key))
            .map(|bytes| deserialize(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    fn put_lsp_payment_delivery(&self, delivery: &LspPaymentDelivery) -> Result<(), String> {
        let bytes = serialize(delivery).map_err(|error| error.to_string())?;
        let key = delivery.key();
        let mut batch = self.batch();
        batch.put(delivery_key(&key), bytes);
        batch.put(payment_hash_index_key(&delivery.payment_hash, &key), []);
        batch.commit();
        Ok(())
    }

    fn list_lsp_payment_deliveries(&self) -> Result<Vec<LspPaymentDelivery>, String> {
        self.collect_by_prefix(PAYMENT_DELIVERY_PREFIX)
            .into_iter()
            .map(|pair| deserialize(&pair.value).map_err(|error| error.to_string()))
            .collect()
    }

    fn list_lsp_payment_deliveries_by_payment_hash(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Vec<LspPaymentDelivery>, String> {
        self.collect_by_prefix(&payment_hash_index_prefix(payment_hash))
            .into_iter()
            .map(|pair| {
                let key = parse_indexed_execution_key(&pair.key)?;
                self.get_lsp_payment_delivery(&key)?.ok_or_else(|| {
                    format!(
                        "hosted payment delivery index points to missing execution {:?}",
                        key
                    )
                })
            })
            .collect()
    }
}

/// State transition and validation layer used by the LSP service actor.
#[derive(Clone)]
pub struct LspPaymentDeliveryManager<S> {
    store: S,
    limits: LspPaymentDeliveryLimits,
}

impl<S: LspPaymentDeliveryStore> LspPaymentDeliveryManager<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            limits: LspPaymentDeliveryLimits::default(),
        }
    }

    pub fn with_limits(store: S, limits: LspPaymentDeliveryLimits) -> Self {
        Self { store, limits }
    }

    pub fn get(&self, key: &LspPaymentDeliveryKey) -> Result<Option<LspPaymentDelivery>, String> {
        self.store.get_lsp_payment_delivery(key)
    }

    pub fn list_by_payment_hash(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Vec<LspPaymentDelivery>, String> {
        self.store
            .list_lsp_payment_deliveries_by_payment_hash(payment_hash)
    }

    pub fn get_active_by_payment_hash(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<LspPaymentDelivery>, String> {
        let mut active = self
            .list_by_payment_hash(payment_hash)?
            .into_iter()
            .filter(|delivery| !delivery.status.is_final());
        let delivery = active.next();
        if active.next().is_some() {
            return Err(format!(
                "multiple active hosted payment deliveries found for {payment_hash}"
            ));
        }
        Ok(delivery)
    }

    pub fn get_by_payment_hash(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<LspPaymentDelivery>, String> {
        let deliveries = self.list_by_payment_hash(payment_hash)?;
        let mut active = deliveries
            .iter()
            .filter(|delivery| !delivery.status.is_final());
        if let Some(delivery) = active.next() {
            if active.next().is_some() {
                return Err(format!(
                    "multiple active hosted payment deliveries found for {payment_hash}"
                ));
            }
            return Ok(Some(delivery.clone()));
        }
        Ok(deliveries
            .into_iter()
            .max_by_key(|delivery| (delivery.updated_at, delivery.created_at)))
    }

    pub fn list_pending(&self) -> Result<Vec<LspPaymentDelivery>, String> {
        Ok(self
            .store
            .list_lsp_payment_deliveries()?
            .into_iter()
            .filter(|delivery| !delivery.status.is_final())
            .collect())
    }

    pub fn has_pending_for_tenant(&self, tenant_id: &TenantId) -> Result<bool, String> {
        Ok(self
            .list_pending()?
            .iter()
            .any(|delivery| &delivery.tenant_id == tenant_id))
    }

    pub fn begin_dispatch(
        &self,
        key: &LspPaymentDeliveryKey,
        now: u64,
    ) -> Result<LspPaymentDelivery, String> {
        let mut delivery = self
            .get(key)?
            .ok_or_else(|| format!("hosted payment delivery {key:?} not found"))?;
        if !matches!(
            delivery.status,
            LspPaymentDeliveryStatus::Deferred | LspPaymentDeliveryStatus::Dispatching
        ) {
            return Err(format!(
                "cannot dispatch hosted payment delivery in state {:?}",
                delivery.status
            ));
        }
        delivery.attempt_count = delivery
            .attempt_count
            .checked_add(1)
            .ok_or_else(|| "hosted payment delivery attempt count overflow".to_string())?;
        delivery.status = LspPaymentDeliveryStatus::Dispatching;
        delivery.updated_at = now;
        self.store.put_lsp_payment_delivery(&delivery)?;
        Ok(delivery)
    }

    pub fn accept(
        &self,
        registration: &LspInvoiceRegistration,
        tenant: &super::HostedTenantRecord,
        mut request: TrampolineForwardingRequest,
        now: u64,
    ) -> Result<LspPaymentDelivery, String> {
        registration
            .hint
            .verify_for_invoice(&registration.invoice, now)?;
        if request.payment_hash != registration.hint.payload.payment_hash {
            return Err("delivery payment hash does not match LSP invoice hint".to_string());
        }
        if registration.tenant_id != tenant.tenant_id {
            return Err("hosted invoice registration does not match tenant".to_string());
        }
        let private_channel_id = tenant
            .private_channel_id
            .ok_or_else(|| "hosted tenant has no private channel".to_string())?;
        request.next_node_id = tenant.invoice_pubkey;
        request.clone().into_send_payment_data()?;
        if registration
            .invoice
            .amount()
            .is_some_and(|amount| amount != request.amount_to_forward)
        {
            return Err("delivery amount does not match hosted invoice".to_string());
        }
        if registration.invoice.udt_type_script() != request.udt_type_script.as_ref() {
            return Err("delivery asset does not match hosted invoice".to_string());
        }
        let key = LspPaymentDeliveryKey::from_request(&request);
        if let Some(existing) = self.get(&key)? {
            if existing.tenant_id == registration.tenant_id
                && existing.private_channel_id == private_channel_id
                && existing.request == request
            {
                return Ok(existing);
            }
            return Err("hosted payment execution already exists with different data".to_string());
        }
        if let Some(existing) = self.get_active_by_payment_hash(&request.payment_hash)? {
            return Err(format!(
                "hosted payment {} already has active execution {:?}; multiple active incoming TLCs per payment hash are not supported",
                request.payment_hash,
                existing.key()
            ));
        }
        if self
            .list_by_payment_hash(&request.payment_hash)?
            .iter()
            .any(|delivery| delivery.status == LspPaymentDeliveryStatus::Succeeded)
        {
            return Err(format!(
                "hosted payment {} was already delivered successfully",
                request.payment_hash
            ));
        }

        let pending = self.list_pending()?;
        if pending.len() >= self.limits.max_pending_deliveries {
            return Err("global pending hosted delivery limit reached".to_string());
        }
        if pending
            .iter()
            .filter(|delivery| delivery.tenant_id == tenant.tenant_id)
            .count()
            >= self.limits.max_pending_deliveries_per_tenant
        {
            return Err(format!(
                "pending hosted delivery limit reached for tenant {}",
                tenant.tenant_id
            ));
        }

        let buffer_cap = now
            .checked_add(registration.hint.payload.buffer_duration_ms)
            .ok_or_else(|| "LSP buffer deadline overflow".to_string())?;
        let expiry_budget_deadline = request
            .max_outgoing_tlc_expiry
            .checked_sub(request.tlc_expiry_delta)
            .and_then(|deadline| deadline.checked_sub(LSP_DELIVERY_SAFETY_MARGIN_MS))
            .ok_or_else(|| "incoming TLC has no safe hosted delivery window".to_string())?;
        let buffer_deadline = buffer_cap
            .min(registration.hint.payload.expires_at)
            .min(expiry_budget_deadline);
        if buffer_deadline <= now {
            return Err("incoming TLC has no remaining hosted delivery window".to_string());
        }

        let delivery = LspPaymentDelivery {
            payment_hash: request.payment_hash,
            tenant_id: registration.tenant_id.clone(),
            private_channel_id,
            request,
            buffer_deadline,
            status: LspPaymentDeliveryStatus::Deferred,
            attempt_count: 0,
            last_error: None,
            last_error_code: None,
            created_at: now,
            updated_at: now,
        };
        self.store.put_lsp_payment_delivery(&delivery)?;
        Ok(delivery)
    }

    pub fn transition(
        &self,
        key: &LspPaymentDeliveryKey,
        status: LspPaymentDeliveryStatus,
        now: u64,
    ) -> Result<LspPaymentDelivery, String> {
        self.transition_with_error(key, status, None, now)
    }

    pub fn transition_with_error(
        &self,
        key: &LspPaymentDeliveryKey,
        status: LspPaymentDeliveryStatus,
        error: Option<(String, Option<TlcErrorCode>)>,
        now: u64,
    ) -> Result<LspPaymentDelivery, String> {
        let mut delivery = self
            .get(key)?
            .ok_or_else(|| format!("hosted payment delivery {key:?} not found"))?;
        if delivery.status == status {
            if let Some((reason, error_code)) = error {
                delivery.last_error = Some(reason);
                delivery.last_error_code = error_code;
                delivery.updated_at = now;
                self.store.put_lsp_payment_delivery(&delivery)?;
            }
            return Ok(delivery);
        }
        if !delivery.status.check_next_valid(&status) {
            return Err(format!(
                "invalid hosted payment delivery transition from {:?} to {:?}",
                delivery.status, status
            ));
        }
        if let Some((reason, error_code)) = error {
            delivery.last_error = Some(reason);
            delivery.last_error_code = error_code;
        }
        delivery.status = status;
        delivery.updated_at = now;
        self.store.put_lsp_payment_delivery(&delivery)?;
        Ok(delivery)
    }
}
