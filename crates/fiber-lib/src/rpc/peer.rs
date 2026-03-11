use crate::fiber::network::PeerDisconnectReason;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::log_and_error;
use crate::rpc::utils::{rpc_error, RpcResultExt};
use fiber_types::{Multiaddr, Pubkey};
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use std::convert::TryFrom;

use ractor::call;
use ractor::ActorRef;

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
        if let Some(address_str) = params.address.as_ref() {
            let address = address_str.parse::<Multiaddr>().rpc_err(&params)?;
            let save = params.save.unwrap_or(true);
            let message =
                NetworkActorMessage::Command(NetworkActorCommand::ConnectPeer(address, save));
            return crate::handle_actor_cast!(self.actor, message, params);
        }

        if let Some(pubkey_str) = params.pubkey {
            let pubkey = Pubkey::try_from(pubkey_str).rpc_err(&params)?;
            let message = |rpc_reply| {
                NetworkActorMessage::Command(NetworkActorCommand::ConnectPeerWithPubkey(
                    pubkey, rpc_reply,
                ))
            };
            return crate::handle_actor_call!(self.actor, message, params);
        }

        Err(rpc_error(
            "either `address` or `pubkey` is required",
            params,
        ))
    }

    pub async fn disconnect_peer(
        &self,
        params: DisconnectPeerParams,
    ) -> Result<(), ErrorObjectOwned> {
        let pubkey = Pubkey::try_from(params.pubkey).rpc_err(&params)?;
        let message = NetworkActorMessage::Command(NetworkActorCommand::DisconnectPeer(
            pubkey,
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
                    pubkey: peer.pubkey.into(),
                    address: peer.address.to_string(),
                })
                .collect(),
        })
    }
}
