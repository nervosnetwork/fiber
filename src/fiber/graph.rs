use super::{
    channel::NetworkGraphStateStore,
    serde_utils::{EntityHex, SliceHex},
};
use ckb_types::packed::OutPoint;
use ckb_types::prelude::Entity;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;

use super::types::Hash256;

#[serde_as]
/// A user-defined name for a node, which may be used when displaying the node in a graph.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeName(#[serde_as(as = "SliceHex")] pub [u8; 32]);

/// The size (in bytes) of a serialized public key.
pub const PUBLIC_KEY_SIZE: usize = 33;

#[serde_as]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(#[serde_as(as = "SliceHex")] [u8; PUBLIC_KEY_SIZE]);

impl AsRef<[u8]> for NodeId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Signature(#[serde_as(as = "SliceHex")] [u8; 64]);

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(#[serde_as(as = "EntityHex")] OutPoint);
impl AsRef<[u8]> for ChannelId {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Details about a node in the network, known from the network announcement.
pub struct NodeInfo {
    pub node_id: NodeId,
    /// All valid channels a node has announced
    #[serde_as(as = "Vec<EntityHex>")]
    pub channel_short_ids: Vec<OutPoint>,

    /// Protocol features the node announced support for
    pub features: u64,

    /// When the last known update to the node state was issued.
    /// Value is opaque, as set in the announcement.
    pub timestamp: u64,

    pub node_name: NodeName,
    pub signature: Signature,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub node_a: NodeId,
    pub node_b: NodeId,
    pub node_a_signature: Signature,
    pub node_b_signature: Signature,
    pub ckb_signature: Signature,
    pub channel_id: ChannelId,
    pub capacity: u64,
    pub features: u64,
    pub last_update: u32,
    #[serde_as(as = "EntityHex")]
    pub channel_output: OutPoint,
    pub cltv_expiry_delta: u64,
    pub htlc_minimum_value: u128,
    // Timestamp of last updated
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct NetworkGraph<S> {
    chain_hash: Hash256,
    channels: HashMap<ChannelId, ChannelInfo>,
    nodes: Vec<NodeInfo>,
    store: S,
}

impl<S> NetworkGraph<S>
where
    S: NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    pub fn new(store: S) -> Self {
        let mut network_graph = Self {
            chain_hash: Hash256::from([0; 32]),
            channels: HashMap::new(),
            nodes: vec![],
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
        self.nodes = self.store.get_nodes(None);
    }

    pub fn add_node(&mut self, node_id: NodeId, node_info: NodeInfo) {
        self.nodes.push(node_info);
    }

    pub fn add_channel(&mut self, channel_id: ChannelId, channel_info: ChannelInfo) {
        self.channels.insert(channel_id, channel_info);
    }

    pub fn get_node(&self, node_id: NodeId) -> Option<&NodeInfo> {
        self.nodes.iter().find(|node| node.node_id == node_id)
    }

    pub fn get_channel(&self, channel_id: ChannelId) -> Option<&ChannelInfo> {
        self.channels.get(&channel_id)
    }
}
