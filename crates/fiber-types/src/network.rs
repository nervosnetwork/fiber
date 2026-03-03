//! Network state types.
//!
//! Contains the persistent network actor state that is stored in the node's database.

use crate::Pubkey;
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::Entry, HashMap};
use tentacle_multiaddr::Multiaddr;

/// The persistent state of the network actor.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PersistentNetworkActorState {
    // These addresses are saved by the user (e.g. the user sends a ConnectPeer rpc to the node),
    // we will then save these addresses to the peer store.
    saved_peer_addresses: HashMap<Pubkey, Vec<Multiaddr>>,
}

impl PersistentNetworkActorState {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn get_peer_addresses(&self, pubkey: &Pubkey) -> Vec<Multiaddr> {
        self.saved_peer_addresses
            .get(pubkey)
            .cloned()
            .unwrap_or_default()
    }

    /// Save a single peer address to the peer store. If this address for the peer does not exist,
    /// then return false, otherwise return true.
    pub fn save_peer_address(&mut self, pubkey: Pubkey, addr: Multiaddr) -> bool {
        match self.saved_peer_addresses.entry(pubkey) {
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

    pub fn num_of_saved_nodes(&self) -> usize {
        self.saved_peer_addresses.len()
    }

    pub fn sample_n_peers_to_connect(&self, n: usize) -> HashMap<Pubkey, Vec<Multiaddr>> {
        // TODO: we may need to shuffle the nodes before selecting the first n nodes,
        // to avoid some malicious nodes from being always selected.
        self.saved_peer_addresses
            .iter()
            .take(n)
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }
}
