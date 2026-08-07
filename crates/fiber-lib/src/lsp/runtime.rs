use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use ckb_types::packed::Script;
use ractor::{ActorCell, ActorRef};
use tokio::sync::{mpsc, RwLock};

use crate::ckb::{client::CkbRpcClient, CkbChainMessage};
use crate::fiber::{
    graph::NetworkGraph,
    network::{NetworkActorCommand, NetworkActorMessage, PeerConnectSource},
    types::pubkey_from_tentacle,
    FiberConfig,
};
use crate::fiber_types::Pubkey;
use crate::store::open_store;
use crate::tasks::new_tokio_task_tracker;
use crate::{start_network, NetworkServiceEvent};

use super::{HostedTenantRecord, LspConfig, TenantId};

/// A running hosted Fiber node. Dropping the handle does not stop it; callers
/// must use [`HostedTenantRuntime::stop`] when evicting it.
pub struct HostedTenantRuntime {
    pub node_id: Pubkey,
    pub network_actor: ActorRef<NetworkActorMessage>,
}

impl HostedTenantRuntime {
    pub fn stop(self) {
        self.network_actor
            .stop(Some("hosted tenant runtime evicted".to_string()));
    }
}

/// Factory boundary used by the supervisor and by lightweight unit tests.
#[async_trait]
pub trait TenantRuntimeFactory: Send + Sync {
    fn provision(&self, tenant_id: &TenantId) -> Result<HostedTenantRecord, String>;
    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String>;
}

/// Starts a complete Fiber network actor per tenant while sharing only the CKB
/// chain actor/client and process root with Public T.
pub struct FiberTenantRuntimeFactory {
    lsp_config: LspConfig,
    template_config: FiberConfig,
    chain_client: CkbRpcClient,
    chain_actor: ActorRef<CkbChainMessage>,
    public_network_actor: ActorRef<NetworkActorMessage>,
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
        root_actor: ActorCell,
        default_shutdown_script: Script,
    ) -> Self {
        Self {
            lsp_config,
            template_config,
            chain_client,
            chain_actor,
            public_network_actor,
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
        let node_id = pubkey_from_tentacle(config.public_key());
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            node_id,
            created_at: crate::now_timestamp_as_millis_u64(),
        })
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        let config = self.tenant_config(&record.tenant_id);
        let node_id = pubkey_from_tentacle(config.public_key());
        if node_id != record.node_id {
            return Err(format!(
                "tenant {} key does not match its registered node identity",
                record.tenant_id
            ));
        }

        let store = open_store(config.store_path())?;
        let graph = Arc::new(RwLock::new(NetworkGraph::new(
            store.clone(),
            node_id,
            false,
        )));
        let (event_sender, mut event_receiver) = mpsc::channel(1024);
        let actor = start_network(
            config,
            self.chain_client.clone(),
            self.chain_actor.clone(),
            event_sender,
            new_tokio_task_tracker(),
            self.root_actor.clone(),
            store,
            None,
            graph,
            self.default_shutdown_script.clone(),
        )
        .await;

        let activation_result = async {
            let listening_addr = tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(event) = event_receiver.recv().await {
                    if let NetworkServiceEvent::NetworkStarted(started_node_id, addresses, _) =
                        event
                    {
                        if started_node_id == node_id {
                            return addresses.into_iter().next();
                        }
                    }
                }
                None
            })
            .await
            .map_err(|_| {
                format!(
                    "tenant {} did not start listening in time",
                    record.tenant_id
                )
            })?
            .ok_or_else(|| format!("tenant {} has no listening address", record.tenant_id))?;

            let connect_result = ractor::call_t!(
                self.public_network_actor,
                |reply| NetworkActorMessage::new_command(NetworkActorCommand::ConnectPeer(
                    listening_addr,
                    false,
                    PeerConnectSource::Manual,
                    Some(reply),
                )),
                10_000
            )
            .map_err(|error| format!("failed to connect Public T to tenant: {error}"))?;
            connect_result
        }
        .await;
        if let Err(error) = activation_result {
            actor.stop(Some("hosted tenant activation failed".to_string()));
            return Err(error);
        }

        new_tokio_task_tracker()
            .spawn(async move { while event_receiver.recv().await.is_some() {} });

        Ok(HostedTenantRuntime {
            node_id,
            network_actor: actor,
        })
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
        if self.runtimes.contains_key(&record.tenant_id) {
            return Ok(());
        }
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

    pub fn evict(&mut self, tenant_id: &TenantId) -> bool {
        if let Some(runtime) = self.runtimes.remove(tenant_id) {
            runtime.stop();
            true
        } else {
            false
        }
    }

    pub fn is_active(&self, tenant_id: &TenantId) -> bool {
        self.runtimes.contains_key(tenant_id)
    }

    pub fn active_count(&self) -> usize {
        self.runtimes.len()
    }

    pub fn runtime(&self, tenant_id: &TenantId) -> Option<&HostedTenantRuntime> {
        self.runtimes.get(tenant_id)
    }
}
