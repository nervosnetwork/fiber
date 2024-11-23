use crate::ckb::config::UdtCfgInfos as ConfigUdtCfgInfos;
use crate::fiber::channel::ChannelActorStateStore;
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

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct GraphNodesParams {
    #[serde_as(as = "Option<U64Hex>")]
    /// The maximum number of nodes to return.
    limit: Option<u64>,
    /// The cursor to start returning nodes from.
    after: Option<JsonBytes>,
}

/// The UDT script which is used to identify the UDT configuration for a Fiber Node
#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtScript {
    /// The code hash of the script.
    code_hash: H256,
    /// The hash type of the script.
    hash_type: ScriptHashType,
    /// The arguments of the script.
    args: String,
}

/// The UDT cell dep which is used to identify the UDT configuration for a Fiber Node
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtCellDep {
    /// The type of the cell dep.
    dep_type: DepType,
    /// The transaction hash of the cell dep.
    tx_hash: H256,
    /// The index of the cell dep.
    #[serde_as(as = "U32Hex")]
    index: u32,
}

/// The UDT argument info which is used to identify the UDT configuration
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct UdtArgInfo {
    /// The name of the UDT.
    name: String,
    /// The script of the UDT.
    script: UdtScript,
    #[serde_as(as = "Option<U128Hex>")]
    /// The minimum amount of the UDT that can be automatically accepted.
    auto_accept_amount: Option<u128>,
    /// The cell deps of the UDT.
    cell_deps: Vec<UdtCellDep>,
}

/// A list of UDT configuration infos.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct UdtCfgInfos(
    /// The list of UDT configuration infos.
    Vec<UdtArgInfo>,
);

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

/// The Node information.
#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
struct NodeInfo {
    /// The alias of the node.
    alias: String,
    /// The addresses of the node.
    addresses: Vec<MultiAddr>,
    /// The node ID.
    node_id: Pubkey,
    #[serde_as(as = "U64Hex")]
    /// The timestamp of the node.
    timestamp: u64,
    /// The chain hash of the node.
    chain_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    /// The minimum CKB funding amount for automatically accepting open channel requests.
    auto_accept_min_ckb_funding_amount: u64,
    /// The UDT configuration infos of the node.
    udt_cfg_infos: UdtCfgInfos,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct GraphNodesResult {
    /// The list of nodes.
    nodes: Vec<NodeInfo>,
    /// The last cursor.
    last_cursor: JsonBytes,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct GraphChannelsParams {
    /// The maximum number of channels to return.
    #[serde_as(as = "Option<U64Hex>")]
    limit: Option<u64>,
    /// The cursor to start returning channels from.
    after: Option<JsonBytes>,
}

/// The Channel information.
#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
struct ChannelInfo {
    /// The outpoint of the channel.
    #[serde_as(as = "EntityHex")]
    channel_outpoint: OutPoint,
    /// The block number of the funding transaction.
    #[serde_as(as = "U64Hex")]
    funding_tx_block_number: u64,
    /// The index of the funding transaction.
    #[serde_as(as = "U32Hex")]
    funding_tx_index: u32,
    /// The node ID of the first node.
    node1: Pubkey,
    /// The node ID of the second node.
    node2: Pubkey,
    /// The last updated timestamp of the channel, milliseconds since UNIX epoch.
    #[serde_as(as = "Option<U64Hex>")]
    last_updated_timestamp: Option<u64>,
    /// The created timestamp of the channel, milliseconds since UNIX epoch.
    #[serde_as(as = "U64Hex")]
    created_timestamp: u64,
    #[serde_as(as = "Option<U64Hex>")]
    /// The fee rate from node 1 to node 2.
    node1_to_node2_fee_rate: Option<u64>,
    /// The fee rate from node 2 to node 1.
    #[serde_as(as = "Option<U64Hex>")]
    node2_to_node1_fee_rate: Option<u64>,
    /// The capacity of the channel.
    #[serde_as(as = "U128Hex")]
    capacity: u128,
    /// The chain hash of the channel.
    chain_hash: Hash256,
    /// The UDT type script of the channel.
    udt_type_script: Option<Script>,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct GraphChannelsResult {
    /// A list of channels.
    channels: Vec<ChannelInfo>,
    /// The last cursor for pagination.
    last_cursor: JsonBytes,
}

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
    S: NetworkGraphStateStore,
{
    _store: S,
    network_graph: Arc<RwLock<NetworkGraph<S>>>,
}

impl<S> GraphRpcServerImpl<S>
where
    S: NetworkGraphStateStore,
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
    S: ChannelActorStateStore + NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    async fn graph_nodes(
        &self,
        params: GraphNodesParams,
    ) -> Result<GraphNodesResult, ErrorObjectOwned> {
        let network_graph = self.network_graph.read().await;
        let default_max_limit = 500;
        let (nodes, last_cursor) = network_graph.get_nodes_with_params(
            params.limit.unwrap_or(default_max_limit) as usize,
            params.after,
        );

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
        let (channels, last_cursor) = network_graph.get_channels_with_params(
            params.limit.unwrap_or(default_max_limit) as usize,
            params.after,
        );

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
