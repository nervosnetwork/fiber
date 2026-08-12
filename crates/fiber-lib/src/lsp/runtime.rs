use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use ckb_types::packed::Script;
use ractor::{Actor, ActorCell, ActorProcessingErr, ActorRef, ActorStatus, RpcReplyPort};
use tokio::sync::{mpsc, RwLock};

use crate::ckb::{client::CkbRpcClient, CkbChainMessage};
use crate::fiber::{
    graph::NetworkGraph,
    network::{HostedTenantActivity, NetworkActorCommand, NetworkActorMessage},
    types::pubkey_from_tentacle,
    FiberConfig,
};
use crate::fiber_types::Pubkey;
use crate::store::{NodeNamespace, Store};
use crate::tasks::new_tokio_task_tracker;
use crate::{start_network, NetworkServiceEvent};

use super::dispatcher::{HostedTenantEndpoint, HostedTenantEndpointArgs, TenantMessageDispatcher};
use super::{HostedTenantRecord, LspConfig, TenantId};

pub(crate) struct HostedTenantTransport {
    tenant_id: TenantId,
    dispatcher: TenantMessageDispatcher,
    endpoint: ActorRef<NetworkActorMessage>,
}

/// Tenant-scoped handles used by the authenticated LSP RPC facade.
#[doc(hidden)]
#[derive(Clone)]
pub struct HostedTenantRpcContext {
    pub(crate) tenant_id: TenantId,
    pub(crate) config: FiberConfig,
    pub(crate) network_actor: ActorRef<NetworkActorMessage>,
    pub(crate) public_node_id: Pubkey,
    pub(crate) store: Store,
}

pub(crate) enum HostedTenantRuntimeMessage {
    FiberMessage {
        source: Pubkey,
        message: crate::fiber::types::FiberMessage,
    },
    GetActivity(RpcReplyPort<Result<HostedTenantActivity, String>>),
}

struct NetworkBackedHostedTenantRuntime;

#[async_trait]
impl Actor for NetworkBackedHostedTenantRuntime {
    type Msg = HostedTenantRuntimeMessage;
    type State = ActorRef<NetworkActorMessage>;
    type Arguments = ActorRef<NetworkActorMessage>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        network_actor: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(network_actor)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        network_actor: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            HostedTenantRuntimeMessage::FiberMessage { source, message } => network_actor
                .send_message(NetworkActorMessage::new_event(
                    crate::fiber::network::NetworkActorEvent::FiberMessage(source, message, None),
                ))
                .map_err(|error| anyhow::Error::msg(error.to_string()))?,
            HostedTenantRuntimeMessage::GetActivity(reply) => {
                let activity = ractor::call_t!(
                    network_actor,
                    |reply| NetworkActorMessage::new_command(
                        NetworkActorCommand::GetHostedTenantActivity(reply)
                    ),
                    5_000
                )
                .map_err(|error| error.to_string());
                let _ = reply.send(activity);
            }
        }
        Ok(())
    }
}

/// A running tenant-scoped Fiber runtime. It reuses the channel/payment
/// coordinator but has no listening P2P endpoint or gossip service.
pub struct HostedTenantRuntime {
    pub invoice_pubkey: Pubkey,
    runtime_actor: ActorRef<HostedTenantRuntimeMessage>,
    backend_actor: Option<ActorCell>,
    pub public_network_actor: Option<ActorRef<NetworkActorMessage>>,
    pub(crate) transport: Option<HostedTenantTransport>,
    rpc_context: Option<HostedTenantRpcContext>,
}

impl HostedTenantRuntime {
    pub(crate) async fn network_backed(
        invoice_pubkey: Pubkey,
        network_actor: ActorRef<NetworkActorMessage>,
    ) -> Result<Self, String> {
        let backend_actor = network_actor.get_cell();
        let runtime_actor = Actor::spawn(None, NetworkBackedHostedTenantRuntime, network_actor)
            .await
            .map_err(|error| format!("failed to start hosted tenant runtime adapter: {error}"))?
            .0;
        Ok(Self {
            invoice_pubkey,
            runtime_actor,
            backend_actor: Some(backend_actor),
            public_network_actor: None,
            transport: None,
            rpc_context: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn actor(&self) -> ActorRef<HostedTenantRuntimeMessage> {
        self.runtime_actor.clone()
    }

    #[cfg(test)]
    pub(crate) fn with_rpc_context(mut self, rpc_context: HostedTenantRpcContext) -> Self {
        self.rpc_context = Some(rpc_context);
        self
    }

    fn is_running(&self) -> bool {
        self.runtime_actor.get_status() < ActorStatus::Stopping
            && self
                .backend_actor
                .as_ref()
                .is_none_or(|actor| actor.get_status() < ActorStatus::Stopping)
    }

    async fn ensure_idle(&self) -> Result<(), String> {
        let activity = ractor::call_t!(
            self.runtime_actor,
            HostedTenantRuntimeMessage::GetActivity,
            5_000
        )
        .map_err(|error| format!("failed to inspect hosted tenant activity: {error}"))??;
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
                NetworkActorCommand::UnregisterInProcessPeer(self.invoice_pubkey),
            ));
        }
        if let Some(transport) = self.transport {
            transport
                .dispatcher
                .unregister_runtime(&transport.tenant_id, &self.runtime_actor);
            transport
                .endpoint
                .stop(Some("hosted tenant transport stopped".to_string()));
        }
        self.runtime_actor
            .stop(Some("hosted tenant runtime evicted".to_string()));
        if let Some(backend_actor) = self.backend_actor {
            backend_actor.stop(Some("hosted tenant backend evicted".to_string()));
        }
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
    dispatcher: TenantMessageDispatcher,
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
            dispatcher: TenantMessageDispatcher::default(),
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
        let invoice_pubkey = pubkey_from_tentacle(config.public_key());
        Ok(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            invoice_pubkey,
            private_channel_id: None,
            created_at: crate::now_timestamp_as_millis_u64(),
        })
    }

    async fn start(&self, record: &HostedTenantRecord) -> Result<HostedTenantRuntime, String> {
        let config = self.tenant_config(&record.tenant_id);
        let invoice_pubkey = pubkey_from_tentacle(config.public_key());
        let tenant_features = config.gen_node_features();
        let public_node_id = pubkey_from_tentacle(self.template_config.public_key());
        let public_features = self.template_config.gen_node_features();
        if invoice_pubkey != record.invoice_pubkey {
            return Err(format!(
                "tenant {} key does not match its registered invoice key",
                record.tenant_id
            ));
        }

        let store = self
            .tenant_store
            .namespaced(NodeNamespace::hosted_tenant(record.tenant_id.as_str()));
        store.ensure_current_schema()?;
        let graph = Arc::new(RwLock::new(NetworkGraph::new(
            store.clone(),
            invoice_pubkey,
            false,
        )));
        let (event_sender, mut event_receiver) = mpsc::channel(1024);
        let actor = start_network(
            config.clone(),
            self.chain_client.clone(),
            self.chain_actor.clone(),
            event_sender,
            new_tokio_task_tracker(),
            self.root_actor.clone(),
            store.clone(),
            None,
            graph,
            self.default_shutdown_script.clone(),
        )
        .await;

        let started = match tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = event_receiver.recv().await {
                if let NetworkServiceEvent::NetworkStarted(started_endpoint, _, _) = event {
                    if started_endpoint == invoice_pubkey {
                        return true;
                    }
                }
            }
            false
        })
        .await
        {
            Ok(started) => started,
            Err(_) => {
                actor.stop(Some("hosted tenant activation timed out".to_string()));
                return Err(format!(
                    "tenant {} runtime did not start in time",
                    record.tenant_id
                ));
            }
        };
        if !started {
            actor.stop(Some("hosted tenant stopped during activation".to_string()));
            return Err(format!(
                "tenant {} runtime stopped before it became ready",
                record.tenant_id
            ));
        }

        let mut runtime =
            match HostedTenantRuntime::network_backed(invoice_pubkey, actor.clone()).await {
                Ok(runtime) => runtime,
                Err(error) => {
                    actor.stop(Some("hosted tenant adapter failed".to_string()));
                    return Err(error);
                }
            };
        if let Err(error) = self.dispatcher.register_runtime(
            record.tenant_id.clone(),
            invoice_pubkey,
            runtime.runtime_actor.clone(),
        ) {
            runtime.stop();
            return Err(error);
        }
        let endpoint = match Actor::spawn_linked(
            None,
            HostedTenantEndpoint,
            HostedTenantEndpointArgs {
                tenant_id: record.tenant_id.clone(),
                invoice_pubkey,
                public_node_id,
                public_network_actor: self.public_network_actor.clone(),
                dispatcher: self.dispatcher.clone(),
            },
            self.root_actor.clone(),
        )
        .await
        {
            Ok((endpoint, _)) => endpoint,
            Err(error) => {
                self.dispatcher
                    .unregister_runtime(&record.tenant_id, &runtime.runtime_actor);
                runtime.stop();
                return Err(format!("failed to start hosted tenant endpoint: {error}"));
            }
        };

        let activation_result = async {
            ractor::call_t!(
                actor,
                |reply| NetworkActorMessage::new_command(
                    NetworkActorCommand::RegisterInProcessPeer {
                        pubkey: public_node_id,
                        actor: endpoint.clone(),
                        features: public_features.clone(),
                        reply,
                    },
                ),
                10_000
            )
            .map_err(|error| format!("failed to register Public T with tenant: {error}"))??;

            ractor::call_t!(
                self.public_network_actor,
                |reply| NetworkActorMessage::new_command(
                    NetworkActorCommand::RegisterInProcessPeer {
                        pubkey: invoice_pubkey,
                        actor: endpoint.clone(),
                        features: tenant_features,
                        reply,
                    },
                ),
                10_000
            )
            .map_err(|error| format!("failed to register tenant with Public T: {error}"))??;

            ractor::call_t!(
                actor,
                |reply| NetworkActorMessage::new_command(
                    NetworkActorCommand::ActivateInProcessPeer(public_node_id, reply,)
                ),
                10_000
            )
            .map_err(|error| format!("failed to activate Public T with tenant: {error}"))??;

            ractor::call_t!(
                self.public_network_actor,
                |reply| NetworkActorMessage::new_command(
                    NetworkActorCommand::ActivateInProcessPeer(invoice_pubkey, reply,)
                ),
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
                    NetworkActorCommand::UnregisterInProcessPeer(invoice_pubkey),
                ));
            endpoint.stop(Some("hosted tenant activation failed".to_string()));
            self.dispatcher
                .unregister_runtime(&record.tenant_id, &runtime.runtime_actor);
            runtime.stop();
            return Err(error);
        }

        new_tokio_task_tracker()
            .spawn(async move { while event_receiver.recv().await.is_some() {} });

        runtime.public_network_actor = Some(self.public_network_actor.clone());
        runtime.rpc_context = Some(HostedTenantRpcContext {
            tenant_id: record.tenant_id.clone(),
            config,
            network_actor: actor,
            public_node_id,
            store,
        });
        runtime.transport = Some(HostedTenantTransport {
            tenant_id: record.tenant_id.clone(),
            dispatcher: self.dispatcher.clone(),
            endpoint,
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
