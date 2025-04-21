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
#[derive(Clone, Serialize, Deserialize)]
pub struct ListPeersResult {
    /// A list of connected peers.
    pub peers: Vec<PeerInfo>,
}
