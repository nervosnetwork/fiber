use super::config::AnnouncedNodeName;
use super::types::{EcdsaSignature, Hash256};
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    pub cltv_expiry_delta: u64,
    pub htlc_minimum_value: u128,
    // Timestamp of last updated
    pub timestamp: u128,
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
        let nodes = self.store.get_nodes(None);
        for node in nodes.iter() {
            self.nodes.insert(node.node_id, node.clone());
        }
    }

    pub fn add_node(&mut self, node_id: Pubkey, node_info: NodeInfo) {
        self.nodes.insert(node_id, node_info.clone());
        self.store.insert_node(node_info);
    }

    pub fn add_channel(&mut self, channel_id: ChannelId, channel_info: ChannelInfo) {
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

    pub fn get_channel(&self, channel_id: ChannelId) -> Option<&ChannelInfo> {
        self.channels.get(&channel_id)
    }
}
