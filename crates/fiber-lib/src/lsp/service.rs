use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};

use crate::fiber::network::NetworkActorMessage;
use crate::fiber_types::{Hash256, Privkey, Pubkey};
use crate::invoice::CkbInvoice;
use crate::store::Store;

use super::{
    HostedTenantStatus, LspConfig, LspInvoiceRegistration, LspInvoiceRegistry, TenantId,
    TenantRegistry, TenantRuntimeFactory, TenantRuntimeStatus, TenantSupervisor,
};

/// Runtime dependencies of the LSP service container.
pub struct LspServiceArgs {
    pub config: LspConfig,
    pub public_node_id: Pubkey,
    pub public_network_actor: ActorRef<NetworkActorMessage>,
    pub store: Store,
    pub runtime_factory: Arc<dyn TenantRuntimeFactory>,
    pub signing_key: Privkey,
}

/// Read-only status for callers that need to discover the hosted service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspServiceStatus {
    pub public_node_id: Pubkey,
    pub tenant_store_root: std::path::PathBuf,
    pub registered_tenants: usize,
    pub active_tenants: usize,
}

/// Commands accepted by the LSP service container.
pub enum LspServiceMessage {
    GetStatus(RpcReplyPort<LspServiceStatus>),
    RegisterTenant(TenantId, RpcReplyPort<Result<HostedTenantStatus, String>>),
    EnsureTenant(TenantId, RpcReplyPort<Result<HostedTenantStatus, String>>),
    EvictTenant(TenantId, RpcReplyPort<Result<HostedTenantStatus, String>>),
    ListTenants(RpcReplyPort<Result<Vec<HostedTenantStatus>, String>>),
    GetTenantNetworkActor(
        TenantId,
        RpcReplyPort<Option<ActorRef<NetworkActorMessage>>>,
    ),
    RegisterInvoice {
        tenant_id: TenantId,
        invoice: CkbInvoice,
        buffer_duration_ms: Option<u64>,
        reply: RpcReplyPort<Result<LspInvoiceRegistration, String>>,
    },
    GetInvoiceRegistration(
        Hash256,
        RpcReplyPort<Result<Option<LspInvoiceRegistration>, String>>,
    ),
}

/// Top-level container for the multi-tenant LSP subsystem.
pub struct LspService;

/// State owned by the LSP service. Tenant components are added behind this
/// boundary rather than sharing Public T's network actor or database.
pub struct LspServiceState {
    pub config: LspConfig,
    pub public_node_id: Pubkey,
    pub public_network_actor: ActorRef<NetworkActorMessage>,
    pub store: Store,
    pub registry: TenantRegistry<Store>,
    pub invoice_registry: LspInvoiceRegistry<Store>,
    pub supervisor: TenantSupervisor,
    pub signing_key: Privkey,
}

impl LspServiceState {
    fn tenant_status(&self, record: crate::lsp::HostedTenantRecord) -> HostedTenantStatus {
        let runtime_status = if self.supervisor.is_active(&record.tenant_id) {
            TenantRuntimeStatus::Active
        } else {
            TenantRuntimeStatus::Cold
        };
        HostedTenantStatus {
            record,
            runtime_status,
        }
    }

    fn get_tenant_status(&self, tenant_id: &TenantId) -> Result<HostedTenantStatus, String> {
        self.registry
            .get(tenant_id)?
            .map(|record| self.tenant_status(record))
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))
    }
}

#[async_trait::async_trait]
impl Actor for LspService {
    type Msg = LspServiceMessage;
    type State = LspServiceState;
    type Arguments = LspServiceArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        args.config.validate().map_err(anyhow::Error::msg)?;
        let registry = TenantRegistry::new(args.store.clone());
        let invoice_registry = LspInvoiceRegistry::new(args.store.clone());
        let supervisor =
            TenantSupervisor::new(args.runtime_factory.clone(), args.config.max_active_tenants);
        for tenant in &args.config.tenants {
            let tenant_id = TenantId::new(tenant.clone()).map_err(anyhow::Error::msg)?;
            let record = supervisor
                .provision(&tenant_id)
                .map_err(anyhow::Error::msg)?;
            registry.register(record).map_err(anyhow::Error::msg)?;
        }
        Ok(LspServiceState {
            config: args.config,
            public_node_id: args.public_node_id,
            public_network_actor: args.public_network_actor,
            store: args.store,
            registry,
            invoice_registry,
            supervisor,
            signing_key: args.signing_key,
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            LspServiceMessage::GetStatus(reply) => {
                let registered_tenants = state.registry.list().map_or(0, |tenants| tenants.len());
                let _ = reply.send(LspServiceStatus {
                    public_node_id: state.public_node_id,
                    tenant_store_root: state.config.tenant_store_root(),
                    registered_tenants,
                    active_tenants: state.supervisor.active_count(),
                });
            }
            LspServiceMessage::RegisterTenant(tenant_id, reply) => {
                let result = state
                    .supervisor
                    .provision(&tenant_id)
                    .and_then(|record| state.registry.register(record))
                    .map(|record| state.tenant_status(record));
                let _ = reply.send(result);
            }
            LspServiceMessage::EnsureTenant(tenant_id, reply) => {
                let result = match state.registry.get(&tenant_id) {
                    Ok(Some(record)) => state
                        .supervisor
                        .ensure(&record)
                        .await
                        .map(|()| state.tenant_status(record)),
                    Ok(None) => Err(format!("tenant {tenant_id} is not registered")),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            LspServiceMessage::EvictTenant(tenant_id, reply) => {
                state.supervisor.evict(&tenant_id);
                let _ = reply.send(state.get_tenant_status(&tenant_id));
            }
            LspServiceMessage::ListTenants(reply) => {
                let result = state.registry.list().map(|records| {
                    records
                        .into_iter()
                        .map(|record| state.tenant_status(record))
                        .collect()
                });
                let _ = reply.send(result);
            }
            LspServiceMessage::GetTenantNetworkActor(tenant_id, reply) => {
                let actor = state
                    .supervisor
                    .runtime(&tenant_id)
                    .map(|runtime| runtime.network_actor.clone());
                let _ = reply.send(actor);
            }
            LspServiceMessage::RegisterInvoice {
                tenant_id,
                invoice,
                buffer_duration_ms,
                reply,
            } => {
                let result = match state.registry.get(&tenant_id) {
                    Ok(Some(tenant)) => state.invoice_registry.register(
                        &tenant,
                        invoice,
                        buffer_duration_ms,
                        state.public_node_id,
                        &state.signing_key,
                    ),
                    Ok(None) => Err(format!("tenant {tenant_id} is not registered")),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            LspServiceMessage::GetInvoiceRegistration(payment_hash, reply) => {
                let _ = reply.send(state.invoice_registry.get(&payment_hash));
            }
        }
        Ok(())
    }
}
