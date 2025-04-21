use crate::fiber::network::PeerInfo;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::log_and_error;
use jsonrpsee::{
    core::async_trait, proc_macros::rpc, types::error::CALL_EXECUTION_FAILED_CODE,
    types::ErrorObjectOwned,
};
use ractor::call;
use ractor::ActorRef;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
pub use tentacle::{multiaddr::MultiAddr, secio::PeerId};

/// RPC module for peer management.
#[rpc(server)]
trait PeerRpc {
    /// Connect to a peer.
    #[method(name = "connect_peer")]
    async fn connect_peer(&self, params: ConnectPeerParams) -> Result<(), ErrorObjectOwned>;

    /// Disconnect from a peer.
    #[method(name = "disconnect_peer")]
    async fn disconnect_peer(&self, params: DisconnectPeerParams) -> Result<(), ErrorObjectOwned>;

    /// List connected peers
    #[method(name = "list_peers")]
    async fn list_peers(&self) -> Result<ListPeersResult, ErrorObjectOwned>;
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

    async fn list_peers(&self) -> Result<ListPeersResult, ErrorObjectOwned> {
        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::ListPeers((), rpc_reply));

        crate::handle_actor_call!(self.actor, message, ()).map(|response| ListPeersResult {
            peers: response.clone(),
        })
    }
}
