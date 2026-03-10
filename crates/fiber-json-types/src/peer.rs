//! Peer management types for the Fiber Network JSON-RPC API.

use crate::serde_utils::Pubkey;
use serde::{Deserialize, Serialize};

/// Parameters for connecting to a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConnectPeerParams {
    /// The address of the peer to connect to (as a multiaddr string).
    /// Either `address` or `pubkey` must be provided.
    pub address: Option<String>,
    /// The public key of the peer to connect to.
    /// The node resolves the address from locally synced graph data.
    pub pubkey: Option<Pubkey>,
    /// Whether to save the peer address to the peer store.
    pub save: Option<bool>,
}

/// Parameters for disconnecting from a peer.
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

    /// The multi-address associated with the connecting peer (as a string).
    /// Note: this is only the address which used for connecting to the peer, not all addresses of the peer.
    /// The `graph_nodes` in Graph rpc module will return all addresses of the peer.
    pub address: String,
}

/// The result of the `list_peers` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPeersResult {
    /// A list of connected peers.
    pub peers: Vec<PeerInfo>,
}
