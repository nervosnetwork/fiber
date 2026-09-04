//! JSON-RPC administration for the multi-tenant hosted LSP service.

use jsonrpsee::{proc_macros::rpc, types::ErrorObjectOwned};
use ractor::{call, ActorRef};

use crate::lsp::{
    HostedTenantRpcContext, HostedTenantStatus as InternalTenantStatus,
    LspPaymentDelivery as InternalPaymentDelivery,
    LspPaymentDeliveryStatus as InternalPaymentDeliveryStatus, LspServiceMessage,
    LspServiceStatus as InternalServiceStatus, TenantId,
    TenantRuntimeStatus as InternalTenantRuntimeStatus,
};
use crate::rpc::invoice::InvoiceRpcServerImpl;
use crate::rpc::payment::PaymentRpcServerImpl;
use crate::rpc::utils::{rpc_error, RpcResultExt};

pub use fiber_json_types::{
    GetInvoiceResult, GetLspInvoiceParams, GetLspPaymentParams, GetLspTenantRegistryNonceParams,
    GetLspTenantRegistryNonceResult, GetPaymentCommandResult, ListLspTenantsResult,
    LspPaymentDelivery, LspPaymentDeliveryStatus, LspPaymentHashParams, LspServiceStatus,
    LspTenantParams, LspTenantRuntimeStatus, LspTenantStatus, RegisterLspTenantParams,
    RegisterLspTenantResult,
};

/// RPC module for hosted LSP tenant and payment-delivery administration.
#[rpc(server)]
trait LspRpc {
    /// Returns a summary of the hosted LSP service.
    #[method(name = "lsp_get_status")]
    async fn lsp_get_status(&self) -> Result<LspServiceStatus, ErrorObjectOwned>;

    /// Issues and persists a fresh one-time tenant registration nonce.
    #[method(name = "lsp_get_tenant_registry_nonce")]
    async fn lsp_get_tenant_registry_nonce(
        &self,
        params: GetLspTenantRegistryNonceParams,
    ) -> Result<GetLspTenantRegistryNonceResult, ErrorObjectOwned>;

    /// Persistently registers a hosted tenant without starting its Fiber runtime.
    #[method(name = "lsp_register_tenant")]
    async fn lsp_register_tenant(
        &self,
        params: RegisterLspTenantParams,
    ) -> Result<RegisterLspTenantResult, ErrorObjectOwned>;

    /// Starts a registered tenant execution context if it is currently cold.
    #[method(name = "lsp_ensure_tenant")]
    async fn lsp_ensure_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned>;

    /// Stops a tenant execution context while retaining its persistent state and keys.
    #[method(name = "lsp_evict_tenant")]
    async fn lsp_evict_tenant(
        &self,
        params: LspTenantParams,
    ) -> Result<LspTenantStatus, ErrorObjectOwned>;

    /// Lists all persistently registered hosted tenants.
    #[method(name = "lsp_list_tenants")]
    async fn lsp_list_tenants(&self) -> Result<ListLspTenantsResult, ErrorObjectOwned>;

    /// Retrieves an invoice from a hosted tenant's scoped store.
    #[method(name = "lsp_get_invoice")]
    async fn lsp_get_invoice(
        &self,
        params: GetLspInvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned>;

    /// Retrieves an outgoing payment owned by a hosted tenant runtime.
    #[method(name = "lsp_get_payment")]
    async fn lsp_get_payment(
        &self,
        params: GetLspPaymentParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned>;

    /// Retrieves durable delivery state for a hosted incoming payment.
    ///
    /// Returns the active incoming-TLC execution when present, otherwise the
    /// most recently updated final execution for the payment hash.
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

    async fn tenant_rpc_context(
        &self,
        tenant_id: TenantId,
    ) -> Result<HostedTenantRpcContext, ErrorObjectOwned> {
        call!(
            self.actor,
            LspServiceMessage::GetTenantRpcContext,
            tenant_id
        )
        .rpc_err()?
        .rpc_err()
    }

    async fn register_tenant(
        &self,
        params: RegisterLspTenantParams,
    ) -> Result<RegisterLspTenantResult, ErrorObjectOwned> {
        let root_signer_pubkey =
            crate::fiber_types::Pubkey::from_slice(&params.root_signer_pubkey.0).rpc_err()?;
        let nonce: crate::fiber_types::Hash256 = params.nonce.into();
        let signature_bytes = hex::decode(params.signature.trim_start_matches("0x")).rpc_err()?;
        let signature =
            crate::fiber_types::TenantRegistrySignature::from_slice(&signature_bytes).rpc_err()?;
        let status = call!(self.actor, LspServiceMessage::GetStatus).rpc_err()?;
        let payload = crate::fiber_types::TenantRegistryPayload::new(
            status.public_node_id,
            root_signer_pubkey,
            nonce.into(),
        );
        let registration = call!(self.actor, |reply| {
            LspServiceMessage::RegisterAuthenticatedTenant {
                payload,
                signature,
                reply,
            }
        })
        .rpc_err()?
        .rpc_err()?;
        Ok(RegisterLspTenantResult {
            tenant: registration.status.into(),
            access_token: registration.access_token,
        })
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

    async fn get_invoice(
        &self,
        params: GetLspInvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        let tenant_id = TenantId::new(params.tenant_id).rpc_err()?;
        let context = self.tenant_rpc_context(tenant_id).await?;
        InvoiceRpcServerImpl::new_fiber(
            context.store,
            Some(context.fiber_actor),
            Some(context.config),
        )
        .get_invoice(fiber_json_types::InvoiceParams {
            payment_hash: params.payment_hash,
        })
        .await
    }

    async fn get_payment(
        &self,
        params: GetLspPaymentParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        let tenant_id = TenantId::new(params.tenant_id).rpc_err()?;
        let context = self.tenant_rpc_context(tenant_id).await?;
        PaymentRpcServerImpl::new_fiber(context.fiber_actor, context.store)
            .get_payment(params.payment)
            .await
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

    async fn lsp_get_tenant_registry_nonce(
        &self,
        params: GetLspTenantRegistryNonceParams,
    ) -> Result<GetLspTenantRegistryNonceResult, ErrorObjectOwned> {
        let root_signer_pubkey =
            crate::fiber_types::Pubkey::from_slice(&params.root_signer_pubkey.0).rpc_err()?;
        let nonce = call!(
            self.actor,
            LspServiceMessage::IssueTenantRegistryNonce,
            root_signer_pubkey
        )
        .rpc_err()?
        .rpc_err()?;
        let status = call!(self.actor, LspServiceMessage::GetStatus).rpc_err()?;
        Ok(GetLspTenantRegistryNonceResult {
            lsp_node_id: status.public_node_id.into(),
            root_signer_pubkey: root_signer_pubkey.into(),
            nonce: crate::fiber_types::Hash256::from(nonce).into(),
        })
    }

    async fn lsp_register_tenant(
        &self,
        params: RegisterLspTenantParams,
    ) -> Result<RegisterLspTenantResult, ErrorObjectOwned> {
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

    async fn lsp_get_invoice(
        &self,
        params: GetLspInvoiceParams,
    ) -> Result<GetInvoiceResult, ErrorObjectOwned> {
        self.get_invoice(params).await
    }

    async fn lsp_get_payment(
        &self,
        params: GetLspPaymentParams,
    ) -> Result<GetPaymentCommandResult, ErrorObjectOwned> {
        self.get_payment(params).await
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
            root_signer_pubkey: status.record.root_signer_pubkey.map(Into::into),
            invoice_pubkey: status.record.tenant_pubkey.into(),
            private_channel_id: status.record.private_channel_id.map(Into::into),
            created_at: status.record.created_at,
            runtime_status,
            channel_online: status.channel_online,
        }
    }
}

impl From<InternalPaymentDelivery> for LspPaymentDelivery {
    fn from(delivery: InternalPaymentDelivery) -> Self {
        let execution_key = delivery.key();
        let (status, failure_reason) = match delivery.status {
            InternalPaymentDeliveryStatus::Deferred => (LspPaymentDeliveryStatus::Deferred, None),
            InternalPaymentDeliveryStatus::Dispatching => {
                (LspPaymentDeliveryStatus::Dispatching, None)
            }
            InternalPaymentDeliveryStatus::InFlight => (LspPaymentDeliveryStatus::InFlight, None),
            InternalPaymentDeliveryStatus::SettlingUpstream { .. } => {
                (LspPaymentDeliveryStatus::SettlingUpstream, None)
            }
            InternalPaymentDeliveryStatus::Succeeded => (LspPaymentDeliveryStatus::Succeeded, None),
            InternalPaymentDeliveryStatus::Failed { reason } => {
                (LspPaymentDeliveryStatus::Failed, Some(reason))
            }
        };
        Self {
            payment_hash: delivery.payment_hash.into(),
            incoming_channel_id: execution_key.incoming_channel_id.into(),
            incoming_tlc_id: execution_key.incoming_tlc_id,
            tenant_id: delivery.tenant_id.to_string(),
            private_channel_id: delivery.private_channel_id.into(),
            buffer_deadline: delivery.buffer_deadline,
            status,
            attempt_count: delivery.attempt_count,
            last_error: delivery.last_error,
            failure_reason,
            created_at: delivery.created_at,
            updated_at: delivery.updated_at,
        }
    }
}
