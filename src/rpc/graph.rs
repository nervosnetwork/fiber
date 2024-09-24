use crate::fiber::graph::{NetworkGraph, NetworkGraphStateStore};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::types::{Hash256, Pubkey};
use ckb_jsonrpc_types::JsonBytes;
use ckb_types::packed::OutPoint;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::sync::Arc;
use tentacle::multiaddr::MultiAddr;
use tokio::sync::RwLock;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphNodesParams {
    limit: Option<usize>,
    after: Option<JsonBytes>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    pub alias: String,
    pub addresses: Vec<MultiAddr>,
    pub node_id: Pubkey,
    pub timestamp: u64,
    pub chain_hash: Hash256,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GraphNodesResult {
    pub nodes: Vec<NodeInfo>,
    pub last_cursor: JsonBytes,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphChannelsParams {
    limit: Option<usize>,
    after: Option<JsonBytes>,
}

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
    pub node1_to_node2_fee_rate: Option<u64>,
    pub node2_to_node1_fee_rate: Option<u64>,
    pub capacity: u128,
    pub chain_hash: Hash256,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GraphChannelsResult {
    pub channels: Vec<ChannelInfo>,
    pub last_cursor: JsonBytes,
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
        params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned> {
        let network_graph = self.network_graph.read().await;
        let default_max_limit = 500;
        let (nodes, last_cursor) = network_graph
            .get_nodes_with_params(params.limit.unwrap_or(default_max_limit), params.after);

        let nodes = nodes
            .iter()
            .map(|node_info| NodeInfo {
                alias: node_info.anouncement_msg.alias.as_str().to_string(),
                addresses: node_info.anouncement_msg.addresses.clone(),
                node_id: node_info.node_id,
                timestamp: node_info.timestamp,
                chain_hash: node_info.anouncement_msg.chain_hash,
            })
            .collect();
        Ok(GraphNodesResult { nodes, last_cursor })
    }

    async fn graph_channels(
        &self,
        params: GraphChannelsParams,
    ) -> Result<GraphChannelsResult, ErrorObjectOwned> {
        let default_max_limit = 500;
        let network_graph = self.network_graph.read().await;
        let chain_hash = network_graph.chain_hash();
        let (channels, last_cursor) = network_graph
            .get_channels_with_params(params.limit.unwrap_or(default_max_limit), params.after);

        let channels = channels
            .iter()
            .map(|channel_info| ChannelInfo {
                channel_outpoint: channel_info.out_point(),
                funding_tx_block_number: channel_info.funding_tx_block_number,
                funidng_tx_index: channel_info.funding_tx_index,
                node1: channel_info.node1(),
                node2: channel_info.node2(),
                capacity: channel_info.capacity(),
                last_updated_timestamp: channel_info.channel_last_update_time(),
                created_timestamp: channel_info.timestamp,
                node1_to_node2_fee_rate: channel_info.node1_to_node2.as_ref().map(|cu| cu.fee_rate),
                node2_to_node1_fee_rate: channel_info.node2_to_node1.as_ref().map(|cu| cu.fee_rate),
                chain_hash,
            })
            .collect();
        Ok(GraphChannelsResult {
            channels,
            last_cursor,
        })
    }
}
