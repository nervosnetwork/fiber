use crate::ckb::{NetworkActorCommand, NetworkActorMessage};
use crate::error;
use crate::log_and_error;
use jsonrpsee::{
    core::async_trait, proc_macros::rpc, types::error::CALL_EXECUTION_FAILED_CODE,
    types::ErrorObjectOwned,
};
use ractor::ActorRef;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::{multiaddr::MultiAddr, secio::PeerId};

#[derive(Serialize, Deserialize, Debug)]
pub struct ConnectPeerParams {
    pub address: MultiAddr,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct DisconnectPeerParams {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
}

#[rpc(server)]
pub trait PeerRpc {
    #[method(name = "connect_peer")]
    async fn connect_peer(&self, params: ConnectPeerParams) -> Result<(), ErrorObjectOwned>;

    #[method(name = "disconnect_peer")]
    async fn disconnect_peer(&self, params: DisconnectPeerParams) -> Result<(), ErrorObjectOwned>;
}

pub struct PeerRpcServerImpl {
    actor: ActorRef<NetworkActorMessage>,
}

impl PeerRpcServerImpl {
    pub fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        PeerRpcServerImpl { actor }
    }
}

#[async_trait]
impl PeerRpcServer for PeerRpcServerImpl {
    async fn connect_peer(&self, params: ConnectPeerParams) -> Result<(), ErrorObjectOwned> {
        let message =
            NetworkActorMessage::Command(NetworkActorCommand::ConnectPeer(params.address.clone()));
        crate::handle_actor_cast!(self.actor, message, params)
    }

    async fn disconnect_peer(&self, params: DisconnectPeerParams) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::DisconnectPeer(
            params.peer_id.clone(),
        ));
        crate::handle_actor_cast!(self.actor, message, params)
    }
}
