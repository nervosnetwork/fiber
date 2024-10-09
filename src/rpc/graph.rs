use crate::ckb::config::UdtCfgInfos as ConfigUdtCfgInfos;
use crate::fiber::graph::{NetworkGraph, NetworkGraphStateStore};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::{U128Hex, U32Hex, U64Hex};
use crate::fiber::types::{Hash256, Pubkey};
use ckb_jsonrpc_types::{DepType, JsonBytes, Script, ScriptHashType};
use ckb_types::packed::OutPoint;
use ckb_types::H256;
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtScript {
    pub code_hash: H256,
    pub hash_type: ScriptHashType,
    pub args: String,
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCellDep {
    pub dep_type: DepType,
    pub tx_hash: H256,
    #[serde_as(as = "U32Hex")]
    pub index: u32,
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtArgInfo {
    pub name: String,
    pub script: UdtScript,
    #[serde_as(as = "Option<U128Hex>")]
    pub auto_accept_amount: Option<u128>,
    pub cell_deps: Vec<UdtCellDep>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCfgInfos(pub Vec<UdtArgInfo>);

impl From<ConfigUdtCfgInfos> for UdtCfgInfos {
    fn from(cfg: ConfigUdtCfgInfos) -> Self {
        UdtCfgInfos(
            cfg.0
                .into_iter()
                .map(|info| UdtArgInfo {
                    name: info.name,
                    script: UdtScript {
                        code_hash: info.script.code_hash,
                        hash_type: info.script.hash_type.into(),
                        args: info.script.args,
                    },
                    cell_deps: info
                        .cell_deps
                        .into_iter()
                        .map(|cell_dep| UdtCellDep {
                            dep_type: cell_dep.dep_type.into(),
                            tx_hash: cell_dep.tx_hash,
                            index: cell_dep.index,
                        })
                        .collect(),
                    auto_accept_amount: info.auto_accept_amount,
                })
                .collect::<Vec<UdtArgInfo>>(),
        )
    }
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    pub alias: String,
    pub addresses: Vec<MultiAddr>,
    pub node_id: Pubkey,
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    pub chain_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    pub auto_accept_min_ckb_funding_amount: u64,
    pub udt_cfg_infos: UdtCfgInfos,
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
    #[serde_as(as = "U64Hex")]
    pub funding_tx_block_number: u64,
    #[serde_as(as = "U32Hex")]
    pub funding_tx_index: u32,
    pub node1: Pubkey,
    pub node2: Pubkey,
    #[serde_as(as = "Option<U64Hex>")]
    pub last_updated_timestamp: Option<u64>,
    pub created_timestamp: u64,
    #[serde_as(as = "Option<U64Hex>")]
    pub node1_to_node2_fee_rate: Option<u64>,
    #[serde_as(as = "Option<U64Hex>")]
    pub node2_to_node1_fee_rate: Option<u64>,
    #[serde_as(as = "U128Hex")]
    pub capacity: u128,
    pub chain_hash: Hash256,
    pub udt_type_script: Option<Script>,
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
                udt_cfg_infos: node_info.anouncement_msg.udt_cfg_infos.clone().into(),
                auto_accept_min_ckb_funding_amount: node_info
                    .anouncement_msg
                    .auto_accept_min_ckb_funding_amount,
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
                funding_tx_index: channel_info.funding_tx_index,
                node1: channel_info.node1(),
                node2: channel_info.node2(),
                capacity: channel_info.capacity(),
                last_updated_timestamp: channel_info.channel_last_update_time(),
                created_timestamp: channel_info.timestamp,
                node1_to_node2_fee_rate: channel_info.node1_to_node2.as_ref().map(|cu| cu.fee_rate),
                node2_to_node1_fee_rate: channel_info.node2_to_node1.as_ref().map(|cu| cu.fee_rate),
                chain_hash,
                udt_type_script: channel_info
                    .announcement_msg
                    .udt_type_script
                    .clone()
                    .map(|s| s.into()),
            })
            .collect();
        Ok(GraphChannelsResult {
            channels,
            last_cursor,
        })
    }
}
