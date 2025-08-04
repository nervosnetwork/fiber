use crate::fiber::network::PeerInfo;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::log_and_error;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;

use ractor::call;
use ractor::ActorRef;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
pub use tentacle::{multiaddr::MultiAddr, secio::PeerId};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConnectPeerParams {
    /// The address of the peer to connect to.
    pub address: MultiAddr,
    /// Whether to save the peer address to the peer store.
    pub save: Option<bool>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct DisconnectPeerParams {
    /// The peer ID of the peer to disconnect.
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
}

/// The result of the `list_peers` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPeersResult {
    /// A list of connected peers.
    pub peers: Vec<PeerInfo>,
}

/// RPC module for peer management.
#[cfg(not(target_arch = "wasm32"))]
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

pub struct PeerRpcServerImpl {
    actor: ActorRef<NetworkActorMessage>,
}

impl PeerRpcServerImpl {
    pub fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        PeerRpcServerImpl { actor }
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl PeerRpcServer for PeerRpcServerImpl {
    /// Connect to a peer.
    async fn connect_peer(&self, params: ConnectPeerParams) -> Result<(), ErrorObjectOwned> {
        self.connect_peer(params).await
    }

    /// Disconnect from a peer.
    async fn disconnect_peer(&self, params: DisconnectPeerParams) -> Result<(), ErrorObjectOwned> {
        self.disconnect_peer(params).await
    }

    /// List connected peers
    async fn list_peers(&self) -> Result<ListPeersResult, ErrorObjectOwned> {
        self.list_peers().await
    }
}

impl PeerRpcServerImpl {
    pub async fn connect_peer(&self, params: ConnectPeerParams) -> Result<(), ErrorObjectOwned> {
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

    pub async fn disconnect_peer(
        &self,
        params: DisconnectPeerParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::DisconnectPeer(
            params.peer_id.clone(),
        ));
        crate::handle_actor_cast!(self.actor, message, params)
    }

    pub async fn list_peers(&self) -> Result<ListPeersResult, ErrorObjectOwned> {
        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::ListPeers((), rpc_reply));

        crate::handle_actor_call!(self.actor, message, ()).map(|response| ListPeersResult {
            peers: response.clone(),
        })
    }
}
