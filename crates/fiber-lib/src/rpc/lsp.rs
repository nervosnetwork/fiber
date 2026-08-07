//! JSON-RPC administration for the multi-tenant hosted LSP service.

use std::str::FromStr;

use jsonrpsee::{proc_macros::rpc, types::ErrorObjectOwned};
use ractor::{call, ActorRef};

use crate::invoice::CkbInvoice;
use crate::lsp::{
    HostedTenantStatus as InternalTenantStatus, LspInvoiceHint as InternalInvoiceHint,
    LspInvoiceRegistration as InternalInvoiceRegistration,
    LspPaymentDelivery as InternalPaymentDelivery,
    LspPaymentDeliveryStatus as InternalPaymentDeliveryStatus, LspServiceMessage,
    LspServiceStatus as InternalServiceStatus, TenantId,
    TenantRuntimeStatus as InternalTenantRuntimeStatus,
};
use crate::rpc::utils::{rpc_error, RpcResultExt};

pub use fiber_json_types::{
    ListLspTenantsResult, LspInvoiceHint, LspInvoiceRegistration, LspPaymentDelivery,
    LspPaymentDeliveryStatus, LspPaymentHashParams, LspServiceStatus, LspTenantParams,
    LspTenantRuntimeStatus, LspTenantStatus, RegisterLspInvoiceParams,
};

/// RPC module for hosted LSP tenant and payment-delivery administration.
#[rpc(server)]
trait LspRpc {
    /// Returns a summary of the hosted LSP service.
    #[method(name = "lsp_get_status")]
    async fn lsp_get_status(&self) -> Result<LspServiceStatus, ErrorObjectOwned>;

    /// Persistently registers a hosted tenant without starting its Fiber runtime.
    #[method(name = "lsp_register_tenant")]
    async fn lsp_register_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned>;

    /// Starts a registered tenant Fiber runtime if it is currently cold.
    #[method(name = "lsp_ensure_tenant")]
    async fn lsp_ensure_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned>;

    /// Stops a tenant Fiber runtime while retaining its persistent identity and state.
    #[method(name = "lsp_evict_tenant")]
    async fn lsp_evict_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned>;

    /// Lists all persistently registered hosted tenants.
    #[method(name = "lsp_list_tenants")]
    async fn lsp_list_tenants(&self) -> Result<ListLspTenantsResult, ErrorObjectOwned>;

    /// Registers a tenant-signed invoice and returns its authenticated LSP hint.
    #[method(name = "lsp_register_invoice")]
    async fn lsp_register_invoice(
        &self,
        params: RegisterLspInvoiceParams,
    ) -> Result<LspInvoiceRegistration, ErrorObjectOwned>;

    /// Retrieves a hosted invoice registration by payment hash.
    #[method(name = "lsp_get_invoice_registration")]
    async fn lsp_get_invoice_registration(
        &self,
        params: LspPaymentHashParams,
    ) -> Result<LspInvoiceRegistration, ErrorObjectOwned>;

    /// Retrieves durable delivery state for a hosted incoming payment.
    #[method(name = "lsp_get_payment_delivery")]
    async fn lsp_get_payment_delivery(
        &self,
        params: LspPaymentHashParams,
    ) -> Result<LspPaymentDelivery, ErrorObjectOwned>;
}

/// JSON-RPC adapter for an active hosted LSP service actor.
pub struct LspRpcServerImpl {
    actor: ActorRef<LspServiceMessage>,
}

impl LspRpcServerImpl {
    /// Construct an LSP RPC server backed by `actor`.
    pub fn new(actor: ActorRef<LspServiceMessage>) -> Self {
        Self { actor }
    }

    async fn register_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned> {
        let tenant_id = TenantId::new(params.tenant_id).rpc_err()?;
        call!(self.actor, LspServiceMessage::RegisterTenant, tenant_id)
            .rpc_err()?
            .rpc_err()
            .map(Into::into)
    }

    async fn ensure_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned> {
        let tenant_id = TenantId::new(params.tenant_id).rpc_err()?;
        call!(self.actor, LspServiceMessage::EnsureTenant, tenant_id)
            .rpc_err()?
            .rpc_err()
            .map(Into::into)
    }

    async fn evict_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned> {
        let tenant_id = TenantId::new(params.tenant_id).rpc_err()?;
        call!(self.actor, LspServiceMessage::EvictTenant, tenant_id)
            .rpc_err()?
            .rpc_err()
            .map(Into::into)
    }

    async fn register_invoice(
        &self,
        params: RegisterLspInvoiceParams,
    ) -> Result<LspInvoiceRegistration, ErrorObjectOwned> {
        let tenant_id = TenantId::new(params.tenant_id).rpc_err()?;
        let invoice = CkbInvoice::from_str(&params.invoice)
            .map_err(|error| rpc_error(format!("failed to parse hosted invoice: {error}")))?;
        call!(self.actor, |reply| LspServiceMessage::RegisterInvoice {
            tenant_id,
            invoice,
            buffer_duration_ms: params.buffer_duration_ms,
            reply,
        })
        .rpc_err()?
        .rpc_err()
        .map(Into::into)
    }

    async fn get_invoice_registration(
        &self,
        params: LspPaymentHashParams,
    ) -> Result<LspInvoiceRegistration, ErrorObjectOwned> {
        let payment_hash = params.payment_hash.into();
        call!(
            self.actor,
            LspServiceMessage::GetInvoiceRegistration,
            payment_hash
        )
        .rpc_err()?
        .rpc_err()?
        .map(Into::into)
        .ok_or_else(|| rpc_error("hosted invoice registration not found"))
    }

    async fn get_payment_delivery(
        &self,
        params: LspPaymentHashParams,
    ) -> Result<LspPaymentDelivery, ErrorObjectOwned> {
        let payment_hash = params.payment_hash.into();
        call!(
            self.actor,
            LspServiceMessage::GetPaymentDelivery,
            payment_hash
        )
        .rpc_err()?
        .rpc_err()?
        .map(Into::into)
        .ok_or_else(|| rpc_error("hosted payment delivery not found"))
    }
}

#[async_trait::async_trait]
impl LspRpcServer for LspRpcServerImpl {
    async fn lsp_get_status(&self) -> Result<LspServiceStatus, ErrorObjectOwned> {
        call!(self.actor, LspServiceMessage::GetStatus)
            .rpc_err()
            .map(Into::into)
    }

    async fn lsp_register_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned> {
        self.register_tenant(params).await
    }

    async fn lsp_ensure_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned> {
        self.ensure_tenant(params).await
    }

    async fn lsp_evict_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned> {
        self.evict_tenant(params).await
    }

    async fn lsp_list_tenants(&self) -> Result<ListLspTenantsResult, ErrorObjectOwned> {
        call!(self.actor, LspServiceMessage::ListTenants)
            .rpc_err()?
            .rpc_err()
            .map(|tenants| ListLspTenantsResult {
                tenants: tenants.into_iter().map(Into::into).collect(),
            })
    }

    async fn lsp_register_invoice(
        &self,
        params: RegisterLspInvoiceParams,
    ) -> Result<LspInvoiceRegistration, ErrorObjectOwned> {
        self.register_invoice(params).await
    }

    async fn lsp_get_invoice_registration(
        &self,
        params: LspPaymentHashParams,
    ) -> Result<LspInvoiceRegistration, ErrorObjectOwned> {
        self.get_invoice_registration(params).await
    }

    async fn lsp_get_payment_delivery(
        &self,
        params: LspPaymentHashParams,
    ) -> Result<LspPaymentDelivery, ErrorObjectOwned> {
        self.get_payment_delivery(params).await
    }
}

impl From<InternalServiceStatus> for LspServiceStatus {
    fn from(status: InternalServiceStatus) -> Self {
        Self {
            public_node_id: status.public_node_id.into(),
            tenant_store_root: status.tenant_store_root.display().to_string(),
            registered_tenants: status.registered_tenants as u64,
            active_tenants: status.active_tenants as u64,
        }
    }
}

impl From<InternalTenantStatus> for LspTenantStatus {
    fn from(status: InternalTenantStatus) -> Self {
        let runtime_status = match status.runtime_status {
            InternalTenantRuntimeStatus::Cold => LspTenantRuntimeStatus::Cold,
            InternalTenantRuntimeStatus::Active => LspTenantRuntimeStatus::Active,
        };
        Self {
            tenant_id: status.record.tenant_id.to_string(),
            node_id: status.record.node_id.into(),
            created_at: status.record.created_at,
            runtime_status,
            channel_online: status.channel_online,
        }
    }
}

impl From<InternalInvoiceHint> for LspInvoiceHint {
    fn from(hint: InternalInvoiceHint) -> Self {
        let payload = hint.payload;
        Self {
            version: payload.version,
            lsp_node_id: payload.lsp_node_id.into(),
            tenant_node_id: payload.tenant_node_id.into(),
            payment_hash: payload.payment_hash.into(),
            invoice_digest: payload.invoice_digest.into(),
            buffer_duration_ms: payload.buffer_duration_ms,
            expires_at: payload.expires_at,
            signature: format!("0x{}", hex::encode(hint.signature.0.serialize_compact())),
        }
    }
}

impl From<InternalInvoiceRegistration> for LspInvoiceRegistration {
    fn from(registration: InternalInvoiceRegistration) -> Self {
        Self {
            tenant_id: registration.tenant_id.to_string(),
            invoice: registration.invoice.to_string(),
            hint: registration.hint.into(),
        }
    }
}

impl From<InternalPaymentDelivery> for LspPaymentDelivery {
    fn from(delivery: InternalPaymentDelivery) -> Self {
        let (status, failure_reason) = match delivery.status {
            InternalPaymentDeliveryStatus::Deferred => (LspPaymentDeliveryStatus::Deferred, None),
            InternalPaymentDeliveryStatus::Dispatching => {
                (LspPaymentDeliveryStatus::Dispatching, None)
            }
            InternalPaymentDeliveryStatus::InFlight => (LspPaymentDeliveryStatus::InFlight, None),
            InternalPaymentDeliveryStatus::Succeeded => (LspPaymentDeliveryStatus::Succeeded, None),
            InternalPaymentDeliveryStatus::Failed { reason } => {
                (LspPaymentDeliveryStatus::Failed, Some(reason))
            }
        };
        Self {
            payment_hash: delivery.payment_hash.into(),
            tenant_id: delivery.tenant_id.to_string(),
            buffer_deadline: delivery.buffer_deadline,
            status,
            failure_reason,
            created_at: delivery.created_at,
            updated_at: delivery.updated_at,
        }
    }
}
