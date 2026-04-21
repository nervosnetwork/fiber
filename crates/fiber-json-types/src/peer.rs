//! Peer management types for the Fiber Network JSON-RPC API.

use crate::schema_helpers::*;
use crate::serde_utils::Pubkey;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The type of transport to filter by when resolving peer addresses.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransportType {
    /// TCP transport (e.g. /ip4/1.2.3.4/tcp/8080)
    Tcp,
    /// WebSocket transport (e.g. /ip4/1.2.3.4/tcp/8080/ws)
    Ws,
    /// WebSocket Secure transport (e.g. /dns/example.com/tcp/443/wss)
    Wss,
}

/// Parameters for connecting to a peer.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct ConnectPeerParams {
    /// The address of the peer to connect to (as a multiaddr string).
    /// Either `address` or `pubkey` must be provided.
    #[schemars(schema_with = "schema_as_string_optional")]
    pub address: Option<String>,
    /// The public key of the peer to connect to.
    /// The node resolves the address from locally synced graph data.
    pub pubkey: Option<Pubkey>,
    /// Whether to save the peer address to the peer store.
    pub save: Option<bool>,
    /// Filter addresses by transport type when connecting by pubkey.
    /// If not specified, a random address from all available addresses is chosen.
    pub addr_type: Option<TransportType>,
}

/// Parameters for disconnecting from a peer.
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
pub struct DisconnectPeerParams {
    /// The public key of the peer to disconnect.
    pub pubkey: Pubkey,
}

/// The information about a peer connected to the node.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerInfo {
    /// The identity public key of the peer.
    pub pubkey: Pubkey,

    /// The multi-address associated with the connecting peer (as a string).
    /// Note: this is only the address which used for connecting to the peer, not all addresses of the peer.
    /// The `graph_nodes` in Graph rpc module will return all addresses of the peer.
    #[schemars(schema_with = "schema_as_string")]
    pub address: String,
}

/// The result of the `list_peers` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListPeersResult {
    /// A list of connected peers.
    pub peers: Vec<PeerInfo>,
}
