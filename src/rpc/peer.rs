use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::log_and_error;
use jsonrpsee::{
    core::async_trait, proc_macros::rpc, types::error::CALL_EXECUTION_FAILED_CODE,
    types::ErrorObjectOwned,
};
use ractor::ActorRef;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::{multiaddr::MultiAddr, secio::PeerId};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct ConnectPeerParams {
    address: MultiAddr,
    save: Option<bool>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct DisconnectPeerParams {
    #[serde_as(as = "DisplayFromStr")]
    peer_id: PeerId,
}

#[rpc(server)]
trait PeerRpc {
    #[method(name = "connect_peer")]
    async fn connect_peer(&self, params: ConnectPeerParams) -> Result<(), ErrorObjectOwned>;

    #[method(name = "disconnect_peer")]
    async fn disconnect_peer(&self, params: DisconnectPeerParams) -> Result<(), ErrorObjectOwned>;
}

pub(crate) struct PeerRpcServerImpl {
    actor: ActorRef<NetworkActorMessage>,
}

impl PeerRpcServerImpl {
    pub(crate) fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        PeerRpcServerImpl { actor }
    }
}

#[async_trait]
impl PeerRpcServer for PeerRpcServerImpl {
    async fn connect_peer(&self, params: ConnectPeerParams) -> Result<(), ErrorObjectOwned> {
        let message =
            NetworkActorMessage::Command(NetworkActorCommand::ConnectPeer(params.address.clone()));
        if params.save.unwrap_or(true) {
            crate::handle_actor_cast!(
                self.actor,
                NetworkActorMessage::Command(NetworkActorCommand::SavePeerAddress(
                    params.address.clone()
                )),
                params.clone()
            )?;
        }
        crate::handle_actor_cast!(self.actor, message, params)
    }

    async fn disconnect_peer(&self, params: DisconnectPeerParams) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::DisconnectPeer(
            params.peer_id.clone(),
        ));
        crate::handle_actor_cast!(self.actor, message, params)
    }
}
