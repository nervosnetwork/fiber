use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use ckb_types::packed::Script;
use ractor::{ActorCell, ActorRef, ActorStatus};
use tokio::sync::{mpsc, RwLock};

use crate::ckb::{client::CkbRpcClient, CkbChainMessage};
use crate::fiber::{
    graph::NetworkGraph,
    network::{
        start_hosted_tenant_actor, FiberActorCommand, FiberActorMessage, FiberActorRef,
        NetworkActorMessage,
    },
    types::pubkey_from_tentacle,
    FiberConfig,
};
use crate::fiber_types::Pubkey;
use crate::store::{NodeNamespace, Store};

use super::{HostedTenantRecord, LspConfig, TenantId};

/// Tenant-scoped handles used by the authenticated LSP RPC facade.
#[doc(hidden)]
#[derive(Clone)]
pub struct HostedTenantRpcContext {
    pub(crate) tenant_id: TenantId,
    pub(crate) config: FiberConfig,
    pub(crate) fiber_actor: FiberActorRef,
    pub(crate) public_node_id: Pubkey,
    pub(crate) store: Store,
}

/// A running tenant-scoped Fiber runtime. It reuses the channel/payment
/// coordinator but has no listening P2P endpoint or gossip service.
pub struct HostedTenantRuntime {
    /// Local Fiber protocol identity. The runtime is not a public network node.
    pub tenant_pubkey: Pubkey,
    runtime_actor: FiberActorRef,
    pub public_network_actor: Option<ActorRef<NetworkActorMessage>>,
    rpc_context: Option<HostedTenantRpcContext>,
}

impl HostedTenantRuntime {
    #[cfg(test)]
    pub(crate) fn network_backed(
        tenant_pubkey: Pubkey,
        network_actor: ActorRef<NetworkActorMessage>,
    ) -> Self {
        Self {
            tenant_pubkey,
            runtime_actor: FiberActorRef::from_network(&network_actor),
            public_network_actor: None,
            rpc_context: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_rpc_context(mut self, rpc_context: HostedTenantRpcContext) -> Self {
        self.rpc_context = Some(rpc_context);
        self
    }

    fn is_running(&self) -> bool {
        self.runtime_actor.get_status() < ActorStatus::Stopping
    }

    async fn ensure_idle(&self) -> Result<(), String> {
        let activity = ractor::call_t!(
            self.runtime_actor,
            |reply| FiberActorMessage::new_command(FiberActorCommand::GetHostedTenantActivity(
                reply
            )),
            5_000
        )
        .map_err(|error| format!("failed to inspect hosted tenant activity: {error}"))?;
        if activity.is_idle() {
            Ok(())
        } else {
            Err(format!(
                "hosted tenant runtime is busy: {} in-flight payments, {} active TLCs, {} pending channel operations",
                activity.inflight_payments,
                activity.active_tlcs,
                activity.pending_channel_operations
            ))
        }
    }

    fn rpc_context(&self) -> Result<HostedTenantRpcContext, String> {
        self.rpc_context
            .clone()
            .ok_or_else(|| "hosted tenant runtime has no RPC context".to_string())
    }

    pub fn stop(self) {
        if let Some(public_network_actor) = self.public_network_actor {
            let _ = public_network_actor.send_message(NetworkActorMessage::new_command(
                FiberActorCommand::UnregisterInProcessPeer(self.tenant_pubkey),
            ));
        }
        self.runtime_actor
            .stop(Some("hosted tenant runtime evicted".to_string()));
    }
}

/// Factory boundary used by the supervisor and by lightweight unit tests.
#[async_trait]
pub trait TenantRuntimeFactory: Send + Sync {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String>;
    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String>;
}

/// Starts a tenant-scoped Fiber channel/payment runtime and wires it to Public
/// T through the in-process transport.
pub struct FiberTenantRuntimeFactory {
    lsp_config: LspConfig,
    template_config: FiberConfig,
    chain_client: CkbRpcClient,
    chain_actor: ActorRef<CkbChainMessage>,
    public_network_actor: ActorRef<NetworkActorMessage>,
    tenant_store: Store,
    root_actor: ActorCell,
    default_shutdown_script: Script,
}

impl FiberTenantRuntimeFactory {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lsp_config: LspConfig,
        template_config: FiberConfig,
        chain_client: CkbRpcClient,
        chain_actor: ActorRef<CkbChainMessage>,
        public_network_actor: ActorRef<NetworkActorMessage>,
        tenant_store: Store,
        root_actor: ActorCell,
        default_shutdown_script: Script,
    ) -> Self {
        Self {
            lsp_config,
            template_config,
            chain_client,
            chain_actor,
            public_network_actor,
            tenant_store,
            root_actor,
            default_shutdown_script,
        }
    }

    fn tenant_config(&self, tenant_id: &TenantId) -> FiberConfig {
        self.template_config
            .hosted_tenant_config(self.lsp_config.tenant_store_root().join(tenant_id.as_str()))
    }
}

#[async_trait]
impl TenantRuntimeFactory for FiberTenantRuntimeFactory {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        let config = self.tenant_config(tenant_id);
        let tenant_pubkey = pubkey_from_tentacle(config.public_key());
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            root_signer_pubkey: None,
            tenant_pubkey,
            private_channel_id: None,
            created_at: crate::now_timestamp_as_millis_u64(),
        })
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        let config = self.tenant_config(&record.tenant_id);
        let tenant_pubkey = pubkey_from_tentacle(config.public_key());
        let tenant_features = config.gen_node_features();
        let public_node_id = pubkey_from_tentacle(self.template_config.public_key());
        let public_features = self.template_config.gen_node_features();
        if tenant_pubkey != record.tenant_pubkey {
            return Err(format!(
                "tenant {} key does not match its registered protocol key",
                record.tenant_id
            ));
        }

        let store = self
            .tenant_store
            .namespaced(NodeNamespace::hosted_tenant(record.tenant_id.as_str()));
        store.ensure_current_schema()?;
        let graph = Arc::new(RwLock::new(NetworkGraph::new_owned_channels(
            store.clone(),
            tenant_pubkey,
        )));
        let (event_sender, mut event_receiver) = mpsc::channel(1024);
        let actor = start_hosted_tenant_actor(
            config.clone(),
            self.chain_client.clone(),
            self.chain_actor.clone(),
            event_sender,
            self.root_actor.clone(),
            store.clone(),
            None,
            graph,
            self.default_shutdown_script.clone(),
        )
        .await?;
        // Live updates while the tenant is running. Evict stops this loop but
        // must not remove the host watch row; PeriodicCheck keeps scanning it.
        // LspService also ensures the row exists on TenantChannelOnline.
        #[cfg(feature = "watchtower")]
        {
            let watchtower_store = self.tenant_store.clone();
            let tenant_node_id = super::tenant_watchtower_node_id(&tenant_pubkey);
            tokio::spawn(async move {
                while let Some(event) = event_receiver.recv().await {
                    crate::event_handler::forward_event_to_watchtower_store(
                        event,
                        &watchtower_store,
                        tenant_node_id.clone(),
                    );
                }
            });
        }
        #[cfg(not(feature = "watchtower"))]
        tokio::spawn(async move { while event_receiver.recv().await.is_some() {} });

        let mut runtime = HostedTenantRuntime {
            tenant_pubkey,
            runtime_actor: actor.clone(),
            public_network_actor: None,
            rpc_context: None,
        };

        let activation_result = async {
            ractor::call_t!(
                actor,
                |reply| FiberActorMessage::new_command(FiberActorCommand::RegisterInProcessPeer {
                    pubkey: public_node_id,
                    actor: FiberActorRef::from_network(&self.public_network_actor),
                    features: public_features,
                    reply,
                },),
                10_000
            )
            .map_err(|error| format!("failed to register Public T with tenant: {error}"))??;

            ractor::call_t!(
                self.public_network_actor,
                |reply| NetworkActorMessage::new_command(
                    FiberActorCommand::RegisterInProcessPeer {
                        pubkey: tenant_pubkey,
                        actor: actor.clone(),
                        features: tenant_features,
                        reply,
                    },
                ),
                10_000
            )
            .map_err(|error| format!("failed to register tenant with Public T: {error}"))??;

            ractor::call_t!(
                actor,
                |reply| FiberActorMessage::new_command(FiberActorCommand::ActivateInProcessPeer(
                    public_node_id,
                    reply,
                )),
                10_000
            )
            .map_err(|error| format!("failed to activate Public T with tenant: {error}"))??;

            ractor::call_t!(
                self.public_network_actor,
                |reply| NetworkActorMessage::new_command(FiberActorCommand::ActivateInProcessPeer(
                    tenant_pubkey,
                    reply,
                )),
                10_000
            )
            .map_err(|error| format!("failed to activate tenant with Public T: {error}"))??;
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = activation_result {
            let _ = self
                .public_network_actor
                .send_message(NetworkActorMessage::new_command(
                    FiberActorCommand::UnregisterInProcessPeer(tenant_pubkey),
                ));
            runtime.stop();
            return Err(error);
        }

        runtime.public_network_actor = Some(self.public_network_actor.clone());
        runtime.rpc_context = Some(HostedTenantRpcContext {
            tenant_id: record.tenant_id.clone(),
            config,
            fiber_actor: actor,
            public_node_id,
            store,
        });
        Ok(runtime)
    }
}

/// Owns the bounded set of currently active tenant runtimes.
pub struct TenantSupervisor {
    factory: Arc<dyn TenantRuntimeFactory>,
    max_active_tenants: usize,
    runtimes: HashMap<TenantId, HostedTenantRuntime>,
}

impl TenantSupervisor {
    pub fn new(factory: Arc<dyn TenantRuntimeFactory>, max_active_tenants: usize) -> Self {
        Self {
            factory,
            max_active_tenants,
            runtimes: HashMap::new(),
        }
    }

    pub fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String> {
        self.factory.provision(tenant_id)
    }

    pub async fn ensure(&mut self, record: &HostedTenantRecord) -> Result<(), String> {
        if self
            .runtimes
            .get(&record.tenant_id)
            .is_some_and(HostedTenantRuntime::is_running)
        {
            return Ok(());
        }
        self.remove_stopped_runtimes();
        if self.runtimes.len() >= self.max_active_tenants {
            return Err(format!(
                "active tenant limit {} reached",
                self.max_active_tenants
            ));
        }
        let runtime = self.factory.start(record).await?;
        self.runtimes.insert(record.tenant_id.clone(), runtime);
        Ok(())
    }

    pub async fn evict(&mut self, tenant_id: &TenantId) -> Result<bool, String> {
        let Some(runtime) = self.runtimes.get(tenant_id) else {
            return Ok(false);
        };
        if runtime.is_running() {
            runtime.ensure_idle().await?;
        }
        if let Some(runtime) = self.runtimes.remove(tenant_id) {
            runtime.stop();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn is_active(&self, tenant_id: &TenantId) -> bool {
        self.runtimes
            .get(tenant_id)
            .is_some_and(HostedTenantRuntime::is_running)
    }

    pub fn active_count(&self) -> usize {
        self.runtimes
            .values()
            .filter(|runtime| runtime.is_running())
            .count()
    }

    pub fn rpc_context(&self, tenant_id: &TenantId) -> Result<HostedTenantRpcContext, String> {
        self.runtimes
            .get(tenant_id)
            .ok_or_else(|| format!("hosted tenant {tenant_id} runtime is not active"))?
            .rpc_context()
    }

    fn remove_stopped_runtimes(&mut self) {
        let stopped = self
            .runtimes
            .iter()
            .filter_map(|(tenant_id, runtime)| (!runtime.is_running()).then_some(tenant_id.clone()))
            .collect::<Vec<_>>();
        for tenant_id in stopped {
            if let Some(runtime) = self.runtimes.remove(&tenant_id) {
                runtime.stop();
            }
        }
    }
}
