use bincode::{deserialize, serialize};
use fiber_store::backend::StorageBackend;
use serde::{Deserialize, Serialize};

use crate::fiber::trampoline::TrampolineForwardingRequest;
use crate::fiber_types::{Hash256, PaymentStatus};
use crate::store::{FiberStore, Store};

use super::{LspInvoiceRegistration, TenantId};

const PAYMENT_DELIVERY_PREFIX: &[u8] = b"\xf2lsp/delivery/";
/// Time retained between the end of buffering and the downstream expiry budget.
pub const LSP_DELIVERY_SAFETY_MARGIN_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug)]
pub struct LspPaymentDeliveryLimits {
    pub max_pending_deliveries: usize,
    pub max_pending_deliveries_per_tenant: usize,
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
    Deferred,
    Dispatching,
    InFlight,
    Succeeded,
    Failed {
        reason: String,
    },
    /// The downstream outcome is durable and Public T is resolving the upstream TLC.
    ///
    /// This variant is appended to preserve the bincode discriminants of records written by
    /// earlier hosted-LSP prototypes.
    SettlingUpstream {
        payment_status: PaymentStatus,
        failure: Option<String>,
    },
    /// The upstream TLC disappeared before downstream dispatch started.
    ///
    /// This variant is appended to preserve persisted bincode discriminants.
    Cancelled {
        reason: String,
    },
    /// Public T is failing the upstream TLC because the buffering window elapsed.
    ///
    /// This variant is appended to preserve persisted bincode discriminants.
    ExpiringUpstream {
        reason: String,
    },
    /// The buffering window elapsed before downstream dispatch could start.
    ///
    /// This variant is appended to preserve persisted bincode discriminants.
    Expired {
        reason: String,
    },
}

impl LspPaymentDeliveryStatus {
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed { .. } | Self::Cancelled { .. } | Self::Expired { .. }
        )
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
    pub created_at: u64,
    pub updated_at: u64,
}

/// Persistence interface for hosted payment delivery records.
pub trait LspPaymentDeliveryStore: Clone + Send + Sync + 'static {
    fn get_lsp_payment_delivery(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<LspPaymentDelivery>, String>;
    fn put_lsp_payment_delivery(&self, delivery: &LspPaymentDelivery) -> Result<(), String>;
    fn list_lsp_payment_deliveries(&self) -> Result<Vec<LspPaymentDelivery>, String>;
}

fn delivery_key(payment_hash: &Hash256) -> Vec<u8> {
    [PAYMENT_DELIVERY_PREFIX, payment_hash.as_ref()].concat()
}

impl LspPaymentDeliveryStore for Store {
    fn get_lsp_payment_delivery(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<LspPaymentDelivery>, String> {
        self.get(delivery_key(payment_hash))
            .map(|bytes| deserialize(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    fn put_lsp_payment_delivery(&self, delivery: &LspPaymentDelivery) -> Result<(), String> {
        let bytes = serialize(delivery).map_err(|error| error.to_string())?;
        self.put(delivery_key(&delivery.payment_hash), bytes);
        Ok(())
    }

    fn list_lsp_payment_deliveries(&self) -> Result<Vec<LspPaymentDelivery>, String> {
        self.collect_by_prefix(PAYMENT_DELIVERY_PREFIX)
            .into_iter()
            .map(|pair| deserialize(&pair.value).map_err(|error| error.to_string()))
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

    pub fn get(&self, payment_hash: &Hash256) -> Result<Option<LspPaymentDelivery>, String> {
        self.store.get_lsp_payment_delivery(payment_hash)
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
        if request.max_parts.is_some_and(|max_parts| max_parts > 1) {
            return Err("buffered hosted delivery does not support MPP".to_string());
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
        if let Some(existing) = self.get(&request.payment_hash)? {
            if existing.tenant_id == registration.tenant_id
                && existing.private_channel_id == private_channel_id
                && existing.request == request
            {
                return Ok(existing);
            }
            return Err("hosted payment delivery already exists with different data".to_string());
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
            created_at: now,
            updated_at: now,
        };
        self.store.put_lsp_payment_delivery(&delivery)?;
        Ok(delivery)
    }

    pub fn transition(
        &self,
        payment_hash: &Hash256,
        status: LspPaymentDeliveryStatus,
        now: u64,
    ) -> Result<LspPaymentDelivery, String> {
        let mut delivery = self
            .get(payment_hash)?
            .ok_or_else(|| format!("hosted payment delivery {payment_hash} not found"))?;
        if delivery.status == status {
            return Ok(delivery);
        }
        let valid = matches!(
            (&delivery.status, &status),
            (
                LspPaymentDeliveryStatus::Deferred,
                LspPaymentDeliveryStatus::Dispatching
                    | LspPaymentDeliveryStatus::Failed { .. }
                    | LspPaymentDeliveryStatus::Cancelled { .. }
                    | LspPaymentDeliveryStatus::ExpiringUpstream { .. }
                    | LspPaymentDeliveryStatus::SettlingUpstream { .. }
            ) | (
                LspPaymentDeliveryStatus::Dispatching,
                LspPaymentDeliveryStatus::Deferred
                    | LspPaymentDeliveryStatus::InFlight
                    | LspPaymentDeliveryStatus::Failed { .. }
                    | LspPaymentDeliveryStatus::Cancelled { .. }
                    | LspPaymentDeliveryStatus::ExpiringUpstream { .. }
                    | LspPaymentDeliveryStatus::SettlingUpstream { .. }
            ) | (
                LspPaymentDeliveryStatus::InFlight,
                LspPaymentDeliveryStatus::SettlingUpstream { .. }
            ) | (
                LspPaymentDeliveryStatus::SettlingUpstream { .. },
                LspPaymentDeliveryStatus::InFlight
                    | LspPaymentDeliveryStatus::Succeeded
                    | LspPaymentDeliveryStatus::Failed { .. }
            ) | (
                LspPaymentDeliveryStatus::ExpiringUpstream { .. },
                LspPaymentDeliveryStatus::InFlight | LspPaymentDeliveryStatus::Expired { .. }
            )
        );
        if !valid {
            return Err(format!(
                "invalid hosted payment delivery transition from {:?} to {:?}",
                delivery.status, status
            ));
        }
        delivery.status = status;
        delivery.updated_at = now;
        self.store.put_lsp_payment_delivery(&delivery)?;
        Ok(delivery)
    }
}
