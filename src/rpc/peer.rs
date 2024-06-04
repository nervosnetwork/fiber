use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use ractor::ActorRef;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use tentacle::{multiaddr::MultiAddr, secio::PeerId};
use crate::ckb::{NetworkActorCommand, NetworkActorMessage};

#[derive(Deserialize)]
pub struct ConnectPeerParams {
    pub address: MultiAddr,
}

#[serde_as]
#[derive(Deserialize)]
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
            NetworkActorMessage::Command(NetworkActorCommand::ConnectPeer(params.address));
        self.actor.cast(message).unwrap();
        Ok(())
    }

    async fn disconnect_peer(&self, params: DisconnectPeerParams) -> Result<(), ErrorObjectOwned> {
        let message =
            NetworkActorMessage::Command(NetworkActorCommand::DisconnectPeer(params.peer_id));
        self.actor.cast(message).unwrap();
        Ok(())
    }
}
