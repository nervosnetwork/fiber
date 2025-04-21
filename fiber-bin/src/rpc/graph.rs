use ckb_jsonrpc_types::JsonBytes;
use fnn::fiber::channel::ChannelActorStateStore;
use fnn::fiber::gossip::GossipMessageStore;
use fnn::fiber::graph::{NetworkGraph, NetworkGraphStateStore};
use fnn::fiber::types::Cursor;
use fnn::rpc_types::graph::{
    GraphChannelsParams, GraphChannelsResult, GraphNodesParams, GraphNodesResult,
};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use std::sync::Arc;
use tokio::sync::RwLock;

/// RPC module for graph management.
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

pub(crate) struct GraphRpcServerImpl<S>
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
    pub(crate) fn new(network_graph: Arc<RwLock<NetworkGraph<S>>>, store: S) -> Self {
        GraphRpcServerImpl {
            _store: store,
            network_graph,
        }
    }
}

#[async_trait]
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
    async fn graph_nodes(
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
        let nodes = nodes.into_iter().map(Into::into).collect();

        Ok(GraphNodesResult { nodes, last_cursor })
    }

    async fn graph_channels(
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

        let channels = channels.into_iter().map(Into::into).collect();
        Ok(GraphChannelsResult {
            channels,
            last_cursor,
        })
    }
}
