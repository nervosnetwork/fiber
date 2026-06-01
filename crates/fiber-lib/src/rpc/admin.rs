use crate::store::actor::StoreActorMessage;
use crate::{handle_actor_call, log_and_error};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use ractor::{call, ActorRef};

pub struct AdminRpcServerImpl {
    store_actor: Option<ActorRef<StoreActorMessage>>,
}

impl AdminRpcServerImpl {
    pub fn new(store_actor: Option<ActorRef<StoreActorMessage>>) -> Self {
        AdminRpcServerImpl { store_actor }
    }
}

/// The RPC module for node administration.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait AdminRpc {
    /// Backup the node information.
    #[method(name = "backup")]
    async fn backup(&self) -> Result<(), ErrorObjectOwned>;
}

#[async_trait::async_trait]
#[cfg(not(target_arch = "wasm32"))]
impl AdminRpcServer for AdminRpcServerImpl {
    async fn backup(&self) -> Result<(), ErrorObjectOwned> {
        self.backup().await
    }
}

impl AdminRpcServerImpl {
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn backup(&self) -> Result<(), ErrorObjectOwned> {
        if let Some(ref store_actor) = self.store_actor {
            handle_actor_call!(store_actor, StoreActorMessage::ForceBackup, None::<()>)
        } else {
            log_and_error!(None::<()>, format!("Backup service is not initialized"))
        }
    }
}
