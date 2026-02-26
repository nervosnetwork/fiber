//! Peer-related types for the Fiber Network Node RPC API.

use crate::Pubkey;

use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::multiaddr::MultiAddr;
use tentacle::secio::PeerId;

// ============================================================
// Internal peer types exposed via RPC
// ============================================================

/// Information about a connected peer.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PeerInfo {
    pub pubkey: Pubkey,
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub address: MultiAddr,
}

// ============================================================
// RPC param/result types
// ============================================================

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
