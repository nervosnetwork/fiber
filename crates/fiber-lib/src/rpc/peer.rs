use crate::fiber::network::PeerDisconnectReason;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::log_and_error;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
use jsonrpsee::types::ErrorObjectOwned;

use ractor::call;
use ractor::ActorRef;
pub use tentacle::multiaddr::MultiAddr;

pub use fiber_json_types::{ConnectPeerParams, DisconnectPeerParams, ListPeersResult, PeerInfo};

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
        if let Some(address) = params.address.clone() {
            let save = params.save.unwrap_or(true);
            let message =
                NetworkActorMessage::Command(NetworkActorCommand::ConnectPeer(address, save));
            return crate::handle_actor_cast!(self.actor, message, params);
        }

        if let Some(pubkey) = params.pubkey {
            let message = |rpc_reply| {
                NetworkActorMessage::Command(NetworkActorCommand::ConnectPeerWithPubkey(
                    pubkey, rpc_reply,
                ))
            };
            return crate::handle_actor_call!(self.actor, message, params);
        }

        Err(ErrorObjectOwned::owned(
            CALL_EXECUTION_FAILED_CODE,
            "either `address` or `pubkey` is required",
            Some(params),
        ))
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
