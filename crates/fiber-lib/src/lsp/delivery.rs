use bincode::{deserialize, serialize};
use fiber_store::backend::StorageBackend;
use serde::{Deserialize, Serialize};

use crate::fiber::trampoline::TrampolineForwardingRequest;
use crate::fiber_types::Hash256;
use crate::store::{FiberStore, Store};

use super::{LspInvoiceRegistration, TenantId};

const PAYMENT_DELIVERY_PREFIX: &[u8] = b"\xf2lsp/delivery/";
/// Time retained between the end of buffering and the downstream expiry budget.
pub const LSP_DELIVERY_SAFETY_MARGIN_MS: u64 = 30_000;

/// Durable lifecycle of one hosted incoming payment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LspPaymentDeliveryStatus {
    Deferred,
    Dispatching,
    InFlight,
    Succeeded,
    Failed { reason: String },
}

impl LspPaymentDeliveryStatus {
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
}

impl<S: LspPaymentDeliveryStore> LspPaymentDeliveryManager<S> {
    pub fn new(store: S) -> Self {
        Self { store }
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
        if self.get(&request.payment_hash)?.is_some() {
            return Err("hosted payment delivery already exists".to_string());
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
        if delivery.status.is_final() && delivery.status != status {
            return Err("hosted payment delivery is already final".to_string());
        }
        delivery.status = status;
        delivery.updated_at = now;
        self.store.put_lsp_payment_delivery(&delivery)?;
        Ok(delivery)
    }
}
