//! Network state types.
//!
//! Contains the persistent network actor state that is stored in the node's database.

use crate::Pubkey;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::collections::HashMap;
use tentacle_multiaddr::Multiaddr;
use tentacle_secio::PeerId;

// ============================================================
// PersistentNetworkActorState
// ============================================================

/// The persistent state of the network actor.
///
/// This state is stored in the node's RocksDB and contains:
/// - A mapping from peer IDs to their public keys
/// - A mapping from peer IDs to their saved addresses
#[serde_as]
#[derive(Default, Clone, Serialize, Deserialize, Debug)]
pub struct PersistentNetworkActorState {
    /// Map from peer ID to their public key.
    #[serde_as(as = "Vec<(DisplayFromStr, _)>")]
    peer_pubkey_map: HashMap<PeerId, Pubkey>,
    /// Saved peer addresses (e.g., from ConnectPeer RPC calls).
    #[serde_as(as = "Vec<(DisplayFromStr, _)>")]
    saved_peer_addresses: HashMap<PeerId, Vec<Multiaddr>>,
}

impl PersistentNetworkActorState {
    /// Create a new empty persistent network actor state.
    pub fn new() -> Self {
        Default::default()
    }

    /// Get the saved addresses for a peer.
    pub fn get_peer_addresses(&self, peer_id: &PeerId) -> Vec<Multiaddr> {
        self.saved_peer_addresses
            .get(peer_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Save a single peer address to the peer store.
    ///
    /// Returns `true` if the address was newly added, `false` if it already existed.
    pub fn save_peer_address(&mut self, peer_id: PeerId, addr: Multiaddr) -> bool {
        use std::collections::hash_map::Entry;
        match self.saved_peer_addresses.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                if entry.get().contains(&addr) {
                    false
                } else {
                    entry.get_mut().push(addr);
                    true
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(vec![addr]);
                true
            }
        }
    }

    /// Get the public key for a peer.
    pub fn get_peer_pubkey(&self, peer_id: &PeerId) -> Option<Pubkey> {
        self.peer_pubkey_map.get(peer_id).copied()
    }

    /// Save a peer's public key.
    ///
    /// Returns `true` if the pubkey was different from the existing one or didn't exist.
    pub fn save_peer_pubkey(&mut self, peer_id: PeerId, pubkey: Pubkey) -> bool {
        match self.peer_pubkey_map.insert(peer_id, pubkey) {
            Some(old_pubkey) => old_pubkey != pubkey,
            None => true,
        }
    }

    /// Get the number of saved nodes.
    pub fn num_of_saved_nodes(&self) -> usize {
        self.saved_peer_addresses.len()
    }

    /// Sample up to `n` peers to connect to.
    pub fn sample_n_peers_to_connect(&self, n: usize) -> HashMap<PeerId, Vec<Multiaddr>> {
        // TODO: we may need to shuffle the nodes before selecting the first n nodes,
        // to avoid some malicious nodes from being always selected.
        self.saved_peer_addresses
            .iter()
            .take(n)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}
