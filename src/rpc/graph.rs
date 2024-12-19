use crate::ckb::config::UdtCfgInfos as ConfigUdtCfgInfos;
use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::{NetworkGraph, NetworkGraphStateStore};
use crate::fiber::network::get_chain_hash;
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::{U128Hex, U32Hex, U64Hex};
use crate::fiber::types::{Cursor, Hash256, Pubkey};
use ckb_jsonrpc_types::{DepType, JsonBytes, Script, ScriptHashType};
use ckb_types::packed::OutPoint;
use ckb_types::H256;
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
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

impl From<super::super::fiber::graph::NodeInfo> for NodeInfo {
    fn from(value: super::super::fiber::graph::NodeInfo) -> Self {
        NodeInfo {
            alias: value.alias.to_string(),
            addresses: value.addresses,
            node_id: value.node_id,
            timestamp: value.timestamp,
            chain_hash: get_chain_hash(),
            auto_accept_min_ckb_funding_amount: value.auto_accept_min_ckb_funding_amount,
            udt_cfg_infos: value.udt_cfg_infos.clone().into(),
        }
    }
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
    /// The node ID of the first node.
    node1: Pubkey,
    /// The node ID of the second node.
    node2: Pubkey,
    /// The created timestamp of the channel, which is the block header timestamp of the block
    /// that contains the channel funding transaction.
    created_timestamp: u64,
    /// The timestamp of the last update to channel by node 1 (e.g. updating fee rate).
    #[serde_as(as = "Option<U64Hex>")]
    last_updated_timestamp_of_node1: Option<u64>,
    /// The timestamp of the last update to channel by node 2 (e.g. updating fee rate).
    #[serde_as(as = "Option<U64Hex>")]
    last_updated_timestamp_of_node2: Option<u64>,
    /// The fee rate set by node 1. This is the fee rate for node 1 to forward tlcs sent from node 2 to node 1.
    #[serde_as(as = "Option<U64Hex>")]
    fee_rate_of_node1: Option<u64>,
    #[serde_as(as = "Option<U64Hex>")]
    /// The fee rate set by node 2. This is the fee rate for node 2 to forward tlcs sent from node 1 to node 2.
    fee_rate_of_node2: Option<u64>,
    /// The capacity of the channel.
    #[serde_as(as = "U128Hex")]
    capacity: u128,
    /// The chain hash of the channel.
    chain_hash: Hash256,
    /// The UDT type script of the channel.
    udt_type_script: Option<Script>,
}

impl From<super::super::fiber::graph::ChannelInfo> for ChannelInfo {
    fn from(channel_info: super::super::fiber::graph::ChannelInfo) -> Self {
        ChannelInfo {
            channel_outpoint: channel_info.out_point().clone(),
            node1: channel_info.node1(),
            node2: channel_info.node2(),
            created_timestamp: channel_info.timestamp,
            last_updated_timestamp_of_node1: channel_info
                .update_of_node1
                .as_ref()
                .map(|cu| cu.timestamp),
            last_updated_timestamp_of_node2: channel_info
                .update_of_node2
                .as_ref()
                .map(|cu| cu.timestamp),
            fee_rate_of_node1: channel_info.update_of_node1.as_ref().map(|cu| cu.fee_rate),
            fee_rate_of_node2: channel_info.update_of_node2.as_ref().map(|cu| cu.fee_rate),
            capacity: channel_info.capacity(),
            chain_hash: get_chain_hash(),
            udt_type_script: channel_info.udt_type_script().clone().map(|s| s.into()),
        }
    }
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
