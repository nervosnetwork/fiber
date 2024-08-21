use super::types::{ChannelUpdate, Hash256, NodeAnnouncement};
use super::{serde_utils::EntityHex, types::Pubkey};
use ckb_types::packed::OutPoint;
use secp256k1::schnorr::Signature as SchnorrSignature;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::{HashMap, HashSet};
use tentacle::multiaddr::Multiaddr;
use tentacle::secio::PeerId;

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Details about a node in the network, known from the network announcement.
pub struct NodeInfo {
    pub node_id: Pubkey,
    /// All valid channels a node has announced
    #[serde_as(as = "HashSet<EntityHex>")]
    pub channel_short_ids: HashSet<OutPoint>,

    /// When the last known update to the node state was issued.
    /// Value is opaque, as set in the announcement.
    pub timestamp: u128,

    pub anouncement_msg: Option<NodeAnnouncement>,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub chain_hash: Hash256,
    pub node_1: Pubkey,
    pub node_2: Pubkey,
    pub ckb_signature: SchnorrSignature,
    //pub channel_id: ChannelId,
    pub capacity: u128,
    pub features: u64,
    #[serde_as(as = "EntityHex")]
    pub channel_output: OutPoint,
    pub one_to_two: Option<ChannelUpdateInfo>,
    pub two_to_one: Option<ChannelUpdateInfo>,
    // Timestamp of last updated
    pub timestamp: u128,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelUpdateInfo {
    pub last_update: u64,
    /// Whether the channel can be currently used for payments (in this one direction).
    pub enabled: bool,
    /// The difference in CLTV values that you must have when routing through this channel.
    pub cltv_expiry_delta: u64,
    /// The minimum value, which must be relayed to the next hop via the channel
    pub htlc_minimum_value: u128,
    /// The maximum value which may be relayed to the next hop via the channel.
    pub htlc_maximum_value: u128,
    pub fee_rate: u64,
    /// Most recent update for the channel received from the network
    /// Mostly redundant with the data we store in fields explicitly.
    /// Everything else is useful only for sending out for initial routing sync.
    /// Not stored if contains excess data to prevent DoS.
    pub last_update_message: Option<ChannelUpdate>,
}

#[derive(Clone, Debug, Default)]
pub struct NetworkGraph<S> {
    channels: HashMap<OutPoint, ChannelInfo>,
    // when we restarting a node, we will reconnect to these peers
    connected_peer_addresses: HashMap<PeerId, Multiaddr>,
    nodes: HashMap<Pubkey, NodeInfo>,
    store: S,
    chain_hash: Hash256,
}

impl<S> NetworkGraph<S>
where
    S: NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    pub fn new(store: S) -> Self {
        let mut network_graph = Self {
            channels: HashMap::new(),
            nodes: HashMap::new(),
            connected_peer_addresses: HashMap::new(),
            store,
            chain_hash: Hash256::default(),
        };
        network_graph.load_from_store();
        network_graph
    }

    fn load_from_store(&mut self) {
        let channels = self.store.get_channels(None);
        for channel in channels.iter() {
            self.channels
                .insert(channel.channel_output.clone(), channel.clone());
        }
        let nodes = self.store.get_nodes(None);
        for node in nodes.iter() {
            self.nodes.insert(node.node_id, node.clone());
        }
        for (peer, addr) in self.store.get_connected_peer(None) {
            self.connected_peer_addresses.insert(peer, addr);
        }
    }

    pub fn add_node(&mut self, node_info: NodeInfo) {
        let node_id = node_info.node_id;
        self.nodes.insert(node_id, node_info.clone());
        self.store.insert_node(node_info);
    }

    pub fn add_channel(&mut self, channel_info: ChannelInfo) {
        assert_ne!(channel_info.node_1, channel_info.node_2);
        match self.channels.get(&channel_info.channel_output) {
            Some(channel) => {
                // If the channel already exists, we don't need to update it
                // FIXME: if other fields is different, we should consider it as malioucious and ban the node?
                if channel.one_to_two.is_some() || channel.two_to_one.is_some() {
                    return;
                }
            }
            None => {}
        }
        let outpoint = channel_info.channel_output.clone();
        self.channels.insert(outpoint.clone(), channel_info.clone());
        if let Some(node) = self.nodes.get_mut(&channel_info.node_1) {
            node.channel_short_ids.insert(outpoint.clone());
            self.store.insert_node(node.clone());
        }
        if let Some(node) = self.nodes.get_mut(&channel_info.node_2) {
            node.channel_short_ids.insert(outpoint.clone());
            self.store.insert_node(node.clone());
        }
        self.store.insert_channel(channel_info);
    }

    pub fn get_node(&self, node_id: Pubkey) -> Option<&NodeInfo> {
        self.nodes.get(&node_id)
    }

    pub fn get_channel(&self, outpoint: &OutPoint) -> Option<&ChannelInfo> {
        self.channels.get(outpoint)
    }

    pub fn get_channels_by_peer(&self, node_id: Pubkey) -> impl Iterator<Item = &ChannelInfo> {
        self.channels
            .values()
            .filter(move |channel| channel.node_1 == node_id || channel.node_2 == node_id)
    }

    pub fn process_channel_update(&mut self, channel_outpoint: OutPoint, update: ChannelUpdate) {
        let channel = self.channels.get_mut(&channel_outpoint).unwrap();
        let update_info = match update.message_flags & 1 == 1 {
            true => &mut channel.one_to_two,
            false => &mut channel.two_to_one,
        };
        update_info.get_or_insert(ChannelUpdateInfo {
            last_update: update.timestamp,
            enabled: true,
            cltv_expiry_delta: update.cltv_expiry_delta,
            htlc_minimum_value: update.htlc_minimum_value,
            htlc_maximum_value: update.htlc_maximum_value,
            fee_rate: 0,
            last_update_message: None,
        });
        self.store.insert_channel(channel.to_owned());
        return;
    }

    pub fn check_chain_hash(&self, chain_hash: Hash256) -> bool {
        self.chain_hash == chain_hash
    }

    pub fn add_connected_peer(&mut self, peer_id: &PeerId, address: Multiaddr) {
        self.connected_peer_addresses
            .insert(peer_id.clone(), address.clone());
        self.store.insert_connected_peer(peer_id.clone(), address);
    }

    pub fn get_connected_peers(&self) -> Vec<(&PeerId, &Multiaddr)> {
        self.connected_peer_addresses.iter().collect()
    }

    pub fn remove_connected_peer(&mut self, peer_id: &PeerId) {
        self.connected_peer_addresses.remove(peer_id);
        self.store.remove_connected_peer(peer_id);
    }

    #[cfg(test)]
    pub fn reset(&mut self) {
        self.channels.clear();
        self.nodes.clear();
        self.connected_peer_addresses.clear();
    }
}

pub trait NetworkGraphStateStore {
    fn get_channels(&self, outpoint: Option<OutPoint>) -> Vec<ChannelInfo>;
    fn get_nodes(&self, peer_id: Option<Pubkey>) -> Vec<NodeInfo>;
    fn insert_channel(&self, channel: ChannelInfo);
    fn insert_node(&self, node: NodeInfo);
    fn insert_connected_peer(&self, peer_id: PeerId, multiaddr: Multiaddr);
    fn get_connected_peer(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Multiaddr)>;
    fn remove_connected_peer(&self, peer_id: &PeerId);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use secp256k1::{PublicKey, Secp256k1, SecretKey};

    #[test]
    fn test_graph_channel_info() {
        let secp = Secp256k1::new();
        let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let public_key1 = PublicKey::from_secret_key(&secp, &secret_key1);

        let secret_key2 = SecretKey::from_slice(&[0xab; 32]).expect("32 bytes, within curve order");
        let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);

        let channel_info = ChannelInfo {
            chain_hash: Hash256::default(),
            node_1: public_key1.into(),
            node_2: public_key2.into(),
            ckb_signature: SchnorrSignature::from_slice(&[0x01; 64]).expect("64 bytes"),
            capacity: 0,
            features: 0,
            channel_output: OutPoint::default(),
            one_to_two: None,
            two_to_one: None,
            timestamp: 0,
        };
        let channel_info_ser = serde_json::to_string(&channel_info).unwrap();
        let channel_info_de: ChannelInfo = serde_json::from_str(&channel_info_ser).unwrap();
        assert_eq!(channel_info, channel_info_de);
    }

    #[test]
    fn test_graph_connected_peers() {
        let temp_path = tempfile::tempdir().unwrap();
        let store = Store::new(temp_path.path());
        let mut network_graph = NetworkGraph::new(store);

        let peer_id = PeerId::random();
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/10000".parse().unwrap();
        network_graph.add_connected_peer(&peer_id, address.clone());

        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 1);
        assert_eq!(connected_peers[0], (&peer_id, &address));

        network_graph.reset();
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 0);

        // load from db
        network_graph.load_from_store();
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 1);
        assert_eq!(connected_peers[0], (&peer_id, &address));

        network_graph.remove_connected_peer(&peer_id);
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 0);

        network_graph.load_from_store();
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 0);
    }

    #[test]
    fn test_graph_network_graph() {
        use ckb_types::prelude::Entity;

        let temp_path = tempfile::tempdir().unwrap();
        let store = Store::new(temp_path.path());
        let mut network_graph = NetworkGraph::new(store);

        let channel_outpoint = OutPoint::from_slice(&[0x01; 36]).unwrap();

        let secp = Secp256k1::new();
        let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let public_key1 = PublicKey::from_secret_key(&secp, &secret_key1);

        let secret_key2 = SecretKey::from_slice(&[0xab; 32]).expect("32 bytes, within curve order");
        let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);

        let ckb_signature = SchnorrSignature::from_slice(&[0x01; 64]).expect("64 bytes");

        let node1 = NodeInfo {
            node_id: public_key1.into(),
            channel_short_ids: HashSet::new(),
            timestamp: 0,
            anouncement_msg: None,
        };

        let node2 = NodeInfo {
            node_id: public_key2.into(),
            channel_short_ids: HashSet::new(),
            timestamp: 0,
            anouncement_msg: None,
        };
        network_graph.add_node(node1.clone());
        assert_eq!(network_graph.get_node(public_key1.into()), Some(&node1));
        network_graph.add_node(node2.clone());
        assert_eq!(network_graph.get_node(public_key2.into()), Some(&node2));

        let node1_channels = network_graph.get_channels_by_peer(node1.node_id);
        assert_eq!(node1_channels.count(), 0);
        let node2_channels = network_graph.get_channels_by_peer(node2.node_id);
        assert_eq!(node2_channels.count(), 0);

        network_graph.reset();
        assert_eq!(network_graph.get_channel(&channel_outpoint), None);
        network_graph.load_from_store();

        network_graph.add_node(node1.clone());
        assert_eq!(network_graph.get_node(public_key1.into()), Some(&node1));
        network_graph.add_node(node2.clone());
        assert_eq!(network_graph.get_node(public_key2.into()), Some(&node2));

        let channel_info = ChannelInfo {
            chain_hash: Hash256::default(),
            node_1: public_key1.into(),
            node_2: public_key2.into(),
            ckb_signature,
            capacity: 0,
            features: 0,
            channel_output: channel_outpoint.clone(),
            one_to_two: None,
            two_to_one: None,
            timestamp: 0,
        };
        network_graph.add_channel(channel_info.clone());
        assert_eq!(
            network_graph.get_channel(&channel_outpoint),
            Some(&channel_info)
        );
        let node1_channels = network_graph.get_channels_by_peer(node1.node_id);
        assert_eq!(node1_channels.count(), 1);
        let node2_channels = network_graph.get_channels_by_peer(node2.node_id);
        assert_eq!(node2_channels.count(), 1);

        network_graph.reset();
        assert_eq!(network_graph.get_channel(&channel_outpoint), None);
        network_graph.load_from_store();

        assert_eq!(
            network_graph.get_channel(&channel_outpoint),
            Some(&channel_info)
        );
    }
}
