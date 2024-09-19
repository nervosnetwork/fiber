use jsonrpsee::{core::async_trait, proc_macros::rpc, types::ErrorObjectOwned};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::sync::Arc;
use tentacle::multiaddr::MultiAddr;
use tokio::sync::RwLock;

use crate::fiber::{
    config::AnnouncedNodeName,
    graph::{NetworkGraph, NetworkGraphStateStore},
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphNodesParams {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    pub alias: AnnouncedNodeName,
    pub addresses: Vec<MultiAddr>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
pub struct GraphNodesResult {
    pub nodes: Vec<NodeInfo>,
}

#[rpc(server)]
pub trait GraphRpc {
    #[method(name = "nodes")]
    async fn get_nodes(
        &self,
        params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned>;
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
    async fn get_nodes(
        &self,
        _params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned> {
        let network_graph = self.network_graph.read().await;
        let nodes = network_graph
            .nodes()
            .map(|node_info| NodeInfo {
                alias: node_info.anouncement_msg.alias,
                addresses: node_info.anouncement_msg.addresses.clone(),
            })
            .collect();
        Ok(GraphNodesResult { nodes })
    }
}
