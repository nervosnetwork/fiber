use crate::rpc::{
    schema_as_hex_bytes, schema_as_string_array, schema_as_uint_hex, schema_as_uint_hex_optional,
};

use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::{NetworkGraph, NetworkGraphStateStore};
use crate::fiber::network::get_chain_hash;
use ckb_jsonrpc_types::JsonBytes;
use fiber_json_types::ChannelUpdateInfo as JsonChannelUpdateInfo;
use fiber_types::Cursor;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::{types::error::INVALID_PARAMS_CODE, types::ErrorObjectOwned};

use std::sync::Arc;
use tokio::sync::RwLock;

pub use fiber_json_types::{
    ChannelInfo, GraphChannelsParams, GraphChannelsResult, GraphNodesParams, GraphNodesResult,
    NodeInfo, UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtDep, UdtScript,
};

fn internal_node_info_to_json(value: crate::fiber::graph::NodeInfo) -> NodeInfo {
    NodeInfo {
        node_name: value.node_name.to_string(),
        version: value.version,
        addresses: value.addresses.iter().map(|a| a.to_string()).collect(),
        pubkey: value.node_id.into(),
        timestamp: value.timestamp,
        features: value.features.enabled_features_names(),
        chain_hash: get_chain_hash().into(),
        auto_accept_min_ckb_funding_amount: value.auto_accept_min_ckb_funding_amount,
        udt_cfg_infos: value.udt_cfg_infos.into(),
    }
}

fn internal_channel_info_to_json(channel_info: crate::fiber::graph::ChannelInfo) -> ChannelInfo {
    ChannelInfo {
        channel_outpoint: channel_info.out_point().clone(),
        node1: channel_info.node1().into(),
        node2: channel_info.node2().into(),
        created_timestamp: channel_info.timestamp,
        update_info_of_node1: channel_info
            .update_of_node1
            .map(JsonChannelUpdateInfo::from),
        update_info_of_node2: channel_info
            .update_of_node2
            .map(JsonChannelUpdateInfo::from),
        capacity: channel_info.capacity(),
        chain_hash: get_chain_hash().into(),
        udt_type_script: channel_info.udt_type_script().clone().map(|s| s.into()),
    }
}

/// RPC module for graph management.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait GraphRpc {
    /// Get the list of nodes in the network graph.
    #[method(name = "graph_nodes")]
    async fn graph_nodes(
        &self,
        params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned>;

    /// Get the list of channels in the network graph.
    #[method(name = "graph_channels")]
    async fn graph_channels(
        &self,
        params: GraphChannelsParams,
    ) -> Result<GraphChannelsResult, ErrorObjectOwned>;
}

pub struct GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore + GossipMessageStore,
{
    _store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
}

impl<S> GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore + GossipMessageStore,
{
    pub fn new(network_graph: Arc<RwLock<NetworkGraph<S>>>, store: S) -> Self {
        GraphRpcServerImpl {
            _store: store,
            network_graph,
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> GraphRpcServer for GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore
        + ChannelActorStateStore
        + GossipMessageStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Get the list of nodes in the network graph.
    async fn graph_nodes(
        &self,
        params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned> {
        self.graph_nodes(params).await
    }

    /// Get the list of channels in the network graph.
    async fn graph_channels(
        &self,
        params: GraphChannelsParams,
    ) -> Result<GraphChannelsResult, ErrorObjectOwned> {
        self.graph_channels(params).await
    }
}
impl<S> GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore
        + ChannelActorStateStore
        + GossipMessageStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub async fn graph_nodes(
        &self,
        params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned> {
        let network_graph = self.network_graph.read().await;
        let default_max_limit = 500;
        let limit = params.limit.unwrap_or(default_max_limit) as usize;
        let cursor = params
            .after
            .as_ref()
            .map(|cursor| Cursor::from_bytes(cursor.as_bytes()))
            .transpose()
            .map_err(|e| {
                ErrorObjectOwned::owned(INVALID_PARAMS_CODE, e.to_string(), Some(params))
            })?;
        let nodes = network_graph.get_nodes_with_params(limit, cursor);
        let last_cursor = nodes
            .last()
            .map(|node| JsonBytes::from_vec(node.cursor().to_bytes().into()))
            .unwrap_or_default();
        let nodes = nodes.into_iter().map(internal_node_info_to_json).collect();

        Ok(GraphNodesResult { nodes, last_cursor })
    }

    pub async fn graph_channels(
        &self,
        params: GraphChannelsParams,
    ) -> Result<GraphChannelsResult, ErrorObjectOwned> {
        let default_max_limit = 500;
        let network_graph = self.network_graph.read().await;
        let limit = params.limit.unwrap_or(default_max_limit) as usize;
        let cursor = params
            .after
            .as_ref()
            .map(|cursor| Cursor::from_bytes(cursor.as_bytes()))
            .transpose()
            .map_err(|e| {
                ErrorObjectOwned::owned(INVALID_PARAMS_CODE, e.to_string(), Some(params))
            })?;

        let channels = network_graph.get_channels_with_params(limit, cursor);
        let last_cursor = channels
            .last()
            .map(|node| JsonBytes::from_vec(node.cursor().to_bytes().into()))
            .unwrap_or_default();

        let channels = channels
            .into_iter()
            .map(internal_channel_info_to_json)
            .collect();
        Ok(GraphChannelsResult {
            channels,
            last_cursor,
        })
    }
}
