use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};

use crate::fiber::network::NetworkActorMessage;
use crate::fiber_types::Pubkey;
use crate::store::Store;

use super::LspConfig;

/// Runtime dependencies of the LSP service container.
pub struct LspServiceArgs {
    pub config: LspConfig,
    pub public_node_id: Pubkey,
    pub public_network_actor: ActorRef<NetworkActorMessage>,
    pub store: Store,
}

/// Read-only status for callers that need to discover the hosted service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspServiceStatus {
    pub public_node_id: Pubkey,
    pub tenant_store_root: std::path::PathBuf,
}

/// Commands accepted by the LSP service container.
pub enum LspServiceMessage {
    GetStatus(RpcReplyPort<LspServiceStatus>),
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
        Ok(LspServiceState {
            config: args.config,
            public_node_id: args.public_node_id,
            public_network_actor: args.public_network_actor,
            store: args.store,
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
                let _ = reply.send(LspServiceStatus {
                    public_node_id: state.public_node_id,
                    tenant_store_root: state.config.tenant_store_root(),
                });
            }
        }
        Ok(())
    }
}
