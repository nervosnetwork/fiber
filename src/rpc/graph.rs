use crate::fiber::serde_utils::EntityHex;
use crate::fiber::types::{Hash256, Pubkey};
use crate::fiber::{
    config::AnnouncedNodeName,
    graph::{NetworkGraph, NetworkGraphStateStore},
};
use ckb_types::packed::OutPoint;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::sync::Arc;
use tentacle::multiaddr::MultiAddr;
use tokio::sync::RwLock;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphNodesParams {}

#[derive(Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    pub alias: AnnouncedNodeName,
    pub addresses: Vec<MultiAddr>,
    pub node_id: Pubkey,
    pub timestamp: u64,
    pub chain_hash: Hash256,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GraphNodesResult {
    pub nodes: Vec<NodeInfo>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphChannelsParams {}

#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
pub struct ChannelInfo {
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    pub funding_tx_block_number: u64,
    pub funidng_tx_index: u32,
    pub node1: Pubkey,
    pub node2: Pubkey,
    pub last_updated_timestamp: Option<u64>,
    pub created_timestamp: u64,
    pub fee_rate: u64,
    pub capacity: u128,
    pub chain_hash: Hash256,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GraphChannelsResult {
    pub channels: Vec<ChannelInfo>,
}

#[rpc(server)]
pub trait GraphRpc {
    #[method(name = "graph_nodes")]
    async fn graph_nodes(
        &self,
        params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned>;

    #[method(name = "graph_channels")]
    async fn graph_channels(
        &self,
        params: GraphChannelsParams,
    ) -> Result<GraphChannelsResult, ErrorObjectOwned>;
}

pub struct GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore,
{
    _store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
}

impl<S> GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore,
{
    pub fn new(network_graph: Arc<RwLock<NetworkGraph<S>>>, store: S) -> Self {
        GraphRpcServerImpl {
            _store: store,
            network_graph,
        }
    }
}

#[async_trait]
impl<S> GraphRpcServer for GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    async fn graph_nodes(
        &self,
        _params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned> {
        let network_graph = self.network_graph.read().await;
        let nodes = network_graph
            .nodes()
            .map(|node_info| NodeInfo {
                alias: node_info.anouncement_msg.alias,
                addresses: node_info.anouncement_msg.addresses.clone(),
                node_id: node_info.node_id,
                timestamp: node_info.timestamp,
                chain_hash: node_info.anouncement_msg.chain_hash,
            })
            .collect();
        Ok(GraphNodesResult { nodes })
    }

    async fn graph_channels(
        &self,
        _params: GraphChannelsParams,
    ) -> Result<GraphChannelsResult, ErrorObjectOwned> {
        let network_graph = self.network_graph.read().await;
        let chain_hash = network_graph.chain_hash();
        let channels = network_graph
            .channels()
            .map(|channel_info| {
                let mut res = vec![];
                if let Some(channel_update) = &channel_info.one_to_two {
                    res.push(ChannelInfo {
                        channel_outpoint: channel_info.out_point(),
                        funding_tx_block_number: channel_info.funding_tx_block_number,
                        funidng_tx_index: channel_info.funding_tx_index,
                        node1: channel_info.node1(),
                        node2: channel_info.node2(),
                        capacity: channel_info.capacity(),
                        last_updated_timestamp: channel_info.channel_update_one_to_two_timestamp(),
                        created_timestamp: channel_info.timestamp,
                        fee_rate: channel_update.fee_rate,
                        chain_hash,
                    })
                }
                if let Some(channel_update) = &channel_info.two_to_one {
                    res.push(ChannelInfo {
                        channel_outpoint: channel_info.out_point(),
                        funding_tx_block_number: channel_info.funding_tx_block_number,
                        funidng_tx_index: channel_info.funding_tx_index,
                        node1: channel_info.node1(),
                        node2: channel_info.node2(),
                        capacity: channel_info.capacity(),
                        last_updated_timestamp: channel_info.channel_update_two_to_one_timestamp(),
                        created_timestamp: channel_info.timestamp,
                        fee_rate: channel_update.fee_rate,
                        chain_hash,
                    })
                }
                res.into_iter()
            })
            .flatten()
            .collect::<Vec<_>>();
        Ok(GraphChannelsResult { channels })
    }
}
