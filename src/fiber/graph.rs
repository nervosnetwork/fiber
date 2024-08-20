use super::config::AnnouncedNodeName;
use super::types::{ChannelUpdate, EcdsaSignature, Hash256};
use super::{
    channel::NetworkGraphStateStore,
    serde_utils::{EntityHex, SliceHex},
    types::Pubkey,
};
use ckb_types::packed::OutPoint;
use ckb_types::prelude::Entity;
use secp256k1::schnorr::Signature as SchnorrSignature;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;

#[serde_as]
/// A user-defined name for a node, which may be used when displaying the node in a graph.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeName(#[serde_as(as = "SliceHex")] pub [u8; 32]);

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(#[serde_as(as = "EntityHex")] OutPoint);
impl AsRef<[u8]> for ChannelId {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl From<OutPoint> for ChannelId {
    fn from(out_point: OutPoint) -> Self {
        ChannelId(out_point)
    }
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Details about a node in the network, known from the network announcement.
pub struct NodeInfo {
    pub node_id: Pubkey,
    /// All valid channels a node has announced
    #[serde_as(as = "Vec<EntityHex>")]
    pub channel_short_ids: Vec<OutPoint>,

    /// Protocol features the node announced support for
    pub features: u64,

    /// When the last known update to the node state was issued.
    /// Value is opaque, as set in the announcement.
    pub timestamp: u128,

    pub node_name: AnnouncedNodeName,
    pub signature: EcdsaSignature,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub chain_hash: Hash256,
    pub node_1: Pubkey,
    pub node_2: Pubkey,
    pub ckb_signature: SchnorrSignature,
    pub channel_id: ChannelId,
    pub capacity: u64,
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
    channels: HashMap<ChannelId, ChannelInfo>,
    nodes: HashMap<Pubkey, NodeInfo>,
    store: S,
}

impl<S> NetworkGraph<S>
where
    S: NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    pub fn new(store: S) -> Self {
        let mut network_graph = Self {
            channels: HashMap::new(),
            nodes: HashMap::new(),
            store,
        };
        network_graph.load_from_store();
        network_graph
    }

    fn load_from_store(&mut self) {
        let channels = self.store.get_channels(None);
        for channel in channels.iter() {
            self.channels
                .insert(channel.channel_id.clone(), channel.clone());
        }
        eprintln!("load_from_store channels: {:?}", self.channels);
        let nodes = self.store.get_nodes(None);
        for node in nodes.iter() {
            self.nodes.insert(node.node_id, node.clone());
        }
    }

    pub fn add_node(&mut self, node_id: Pubkey, node_info: NodeInfo) {
        self.nodes.insert(node_id, node_info.clone());
        self.store.insert_node(node_info);
    }

    pub fn add_channel(&mut self, channel_info: ChannelInfo) {
        let channel_id = channel_info.channel_id.clone();
        self.channels.insert(channel_id, channel_info.clone());
        if let Some(node) = self.nodes.get_mut(&channel_info.node_1) {
            node.channel_short_ids
                .push(channel_info.channel_output.clone());
            self.store.insert_node(node.clone());
        }
        self.store.insert_channel(channel_info);
    }

    pub fn get_node(&self, node_id: Pubkey) -> Option<&NodeInfo> {
        self.nodes.get(&node_id)
    }

    pub fn get_channel(&self, channel_id: &ChannelId) -> Option<&ChannelInfo> {
        self.channels.get(channel_id)
    }

    pub fn process_channel_update(&mut self, channel_id: ChannelId, update: ChannelUpdate) {
        let channel = self.channels.get_mut(&channel_id).unwrap();
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

    #[cfg(test)]
    pub fn reset(&mut self) {
        self.channels.clear();
        self.nodes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use secp256k1::Message;
    use secp256k1::{PublicKey, Secp256k1, SecretKey};

    #[test]
    fn test_channel_id() {
        let out_point = OutPoint::default();
        let channel_id = ChannelId::from(out_point.clone());
        assert_eq!(channel_id.0, out_point);
    }

    #[test]
    fn test_channel_info() {
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
            channel_id: ChannelId::from(OutPoint::default()),
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
    fn test_network_graph() {
        let temp_path = tempfile::tempdir().unwrap();
        let store = Store::new(temp_path.path());
        let mut network_graph = NetworkGraph::new(store);

        let channel_id = ChannelId::from(OutPoint::from_slice(&[0x01; 36]).unwrap());

        let secp = Secp256k1::new();
        let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let public_key1 = PublicKey::from_secret_key(&secp, &secret_key1);

        let secret_key2 = SecretKey::from_slice(&[0xab; 32]).expect("32 bytes, within curve order");
        let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);

        let ckb_signature = SchnorrSignature::from_slice(&[0x01; 64]).expect("64 bytes");

        let secp = Secp256k1::new();
        let (secret_key, _public_key) = secp.generate_keypair(&mut rand::thread_rng());
        let message = Message::from_digest_slice(&[0xab; 32]).expect("32 bytes");
        let sig1 = secp.sign_ecdsa(&message, &secret_key);
        let sig2 = secp.sign_ecdsa(&message, &secret_key);

        let node1 = NodeInfo {
            node_id: public_key1.into(),
            channel_short_ids: vec![],
            features: 0,
            timestamp: 0,
            node_name: AnnouncedNodeName::default(),
            signature: EcdsaSignature::from(sig1),
        };

        let node2 = NodeInfo {
            node_id: public_key2.into(),
            channel_short_ids: vec![],
            features: 0,
            timestamp: 0,
            node_name: AnnouncedNodeName::default(),
            signature: EcdsaSignature::from(sig2),
        };
        network_graph.add_node(public_key1.into(), node1.clone());
        assert_eq!(network_graph.get_node(public_key1.into()), Some(&node1));
        network_graph.add_node(public_key2.into(), node2.clone());
        assert_eq!(network_graph.get_node(public_key2.into()), Some(&node2));

        network_graph.reset();
        assert_eq!(network_graph.get_channel(&channel_id), None);
        network_graph.load_from_store();
        network_graph.add_node(public_key1.into(), node1.clone());
        assert_eq!(network_graph.get_node(public_key1.into()), Some(&node1));
        network_graph.add_node(public_key2.into(), node2.clone());
        assert_eq!(network_graph.get_node(public_key2.into()), Some(&node2));

        let channel_info = ChannelInfo {
            chain_hash: Hash256::default(),
            node_1: public_key1.into(),
            node_2: public_key2.into(),
            ckb_signature,
            channel_id: channel_id.clone(),
            capacity: 0,
            features: 0,
            channel_output: OutPoint::default(),
            one_to_two: None,
            two_to_one: None,
            timestamp: 0,
        };
        network_graph.add_channel(channel_info.clone());
        assert_eq!(network_graph.get_channel(&channel_id), Some(&channel_info));

        network_graph.reset();
        assert_eq!(network_graph.get_channel(&channel_id), None);
        network_graph.load_from_store();

        assert_eq!(network_graph.get_channel(&channel_id), Some(&channel_info));
    }
}
