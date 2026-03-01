use crate::fiber::network::PeerDisconnectReason;
use crate::fiber::types::Pubkey;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::log_and_error;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;

use ractor::call;
use ractor::ActorRef;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
pub use tentacle::multiaddr::MultiAddr;

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
    /// The public key of the peer to disconnect.
    pub pubkey: Pubkey,
}

/// The information about a peer connected to the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// The identity public key of the peer.
    pub pubkey: Pubkey,

    /// The multi-address associated with the connecting peer.
    /// Note: this is only the address which used for connecting to the peer, not all addresses of the peer.
    /// The `graph_nodes` in Graph rpc module will return all addresses of the peer.
    pub address: MultiAddr,
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
        let message = NetworkActorMessage::Command(NetworkActorCommand::ConnectPeer(
            params.address.clone(),
            params.save.unwrap_or(true),
        ));
        crate::handle_actor_cast!(self.actor, message, params)
    }

    pub async fn disconnect_peer(
        &self,
        params: DisconnectPeerParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::DisconnectPeer(
            params.pubkey,
            PeerDisconnectReason::Requested,
        ));
        crate::handle_actor_cast!(self.actor, message, params)
    }

    pub async fn list_peers(&self) -> Result<ListPeersResult, ErrorObjectOwned> {
        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::ListPeers((), rpc_reply));

        crate::handle_actor_call!(self.actor, message, ()).map(|response| ListPeersResult {
            peers: response
                .into_iter()
                .map(|peer| PeerInfo {
                    pubkey: peer.pubkey,
                    address: peer.address,
                })
                .collect(),
        })
    }
}
