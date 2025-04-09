use crate::ckb::config::{
    UdtArgInfo as ConfigUdtArgInfo, UdtCellDep as ConfigUdtCellDep,
    UdtCfgInfos as ConfigUdtCfgInfos, UdtDep as ConfigUdtDep, UdtScript as ConfigUdtScript,
};
use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::{ChannelUpdateInfo, NetworkGraph, NetworkGraphStateStore};
use crate::fiber::network::get_chain_hash;
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::{U128Hex, U64Hex};
use crate::fiber::types::{Cursor, Hash256, Pubkey};
use ckb_jsonrpc_types::{DepType, JsonBytes, OutPoint as OutPointWrapper, Script, ScriptHashType};
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
pub struct GraphNodesParams {
    #[serde_as(as = "Option<U64Hex>")]
    /// The maximum number of nodes to return.
    pub limit: Option<u64>,
    /// The cursor to start returning nodes from.
    pub after: Option<JsonBytes>,
}

/// The UDT script which is used to identify the UDT configuration for a Fiber Node
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtScript {
    /// The code hash of the script.
    pub code_hash: H256,
    /// The hash type of the script.
    pub hash_type: ScriptHashType,
    /// The arguments of the script.
    pub args: String,
}

impl From<ConfigUdtScript> for UdtScript {
    fn from(cfg: ConfigUdtScript) -> Self {
        UdtScript {
            code_hash: cfg.code_hash,
            hash_type: cfg.hash_type.into(),
            args: cfg.args,
        }
    }
}

/// Udt script on-chain dependencies.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtDep {
    /// cell dep described by out_point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_dep: Option<UdtCellDep>,
    /// cell dep described by type ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<Script>,
}

impl From<ConfigUdtDep> for UdtDep {
    fn from(cfg: ConfigUdtDep) -> Self {
        UdtDep {
            cell_dep: cfg.cell_dep.map(Into::into),
            type_id: cfg.type_id,
        }
    }
}
/// The UDT cell dep which is used to identify the UDT configuration for a Fiber Node
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCellDep {
    /// The out point of the cell dep.
    pub out_point: OutPointWrapper,
    /// The type of the cell dep.
    pub dep_type: DepType,
}

impl From<ConfigUdtCellDep> for UdtCellDep {
    fn from(cfg: ConfigUdtCellDep) -> Self {
        UdtCellDep {
            dep_type: cfg.dep_type.into(),
            out_point: cfg.out_point,
        }
    }
}

/// The UDT argument info which is used to identify the UDT configuration
#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtArgInfo {
    /// The name of the UDT.
    pub name: String,
    /// The script of the UDT.
    pub script: UdtScript,
    #[serde_as(as = "Option<U128Hex>")]
    /// The minimum amount of the UDT that can be automatically accepted.
    pub auto_accept_amount: Option<u128>,
    /// The cell deps of the UDT.
    pub cell_deps: Vec<UdtDep>,
}

impl From<ConfigUdtArgInfo> for UdtArgInfo {
    fn from(cfg: ConfigUdtArgInfo) -> Self {
        UdtArgInfo {
            name: cfg.name,
            script: UdtScript {
                code_hash: cfg.script.code_hash,
                hash_type: cfg.script.hash_type.into(),
                args: cfg.script.args,
            },
            cell_deps: cfg.cell_deps.into_iter().map(Into::into).collect(),
            auto_accept_amount: cfg.auto_accept_amount,
        }
    }
}

/// A list of UDT configuration infos.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UdtCfgInfos(
    /// The list of UDT configuration infos.
    pub Vec<UdtArgInfo>,
);

impl From<ConfigUdtCfgInfos> for UdtCfgInfos {
    fn from(cfg: ConfigUdtCfgInfos) -> Self {
        UdtCfgInfos(cfg.0.into_iter().map(Into::into).collect())
    }
}

/// The Node information.
#[serde_as]
#[derive(Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    /// The name of the node.
    pub node_name: String,
    /// The addresses of the node.
    pub addresses: Vec<MultiAddr>,
    /// The identity public key of the node.
    pub node_id: Pubkey,
    #[serde_as(as = "U64Hex")]
    /// The latest timestamp set by the owner for the node announcement.
    /// When a Node is online this timestamp will be updated to the latest value.
    pub timestamp: u64,
    /// The chain hash of the node.
    pub chain_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    /// The minimum CKB funding amount for automatically accepting open channel requests.
    pub auto_accept_min_ckb_funding_amount: u64,
    /// The UDT configuration infos of the node.
    pub udt_cfg_infos: UdtCfgInfos,
}

impl From<super::super::fiber::graph::NodeInfo> for NodeInfo {
    fn from(value: super::super::fiber::graph::NodeInfo) -> Self {
        NodeInfo {
            node_name: value.node_name.to_string(),
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
pub struct GraphNodesResult {
    /// The list of nodes.
    pub nodes: Vec<NodeInfo>,
    /// The last cursor.
    pub last_cursor: JsonBytes,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GraphChannelsParams {
    /// The maximum number of channels to return.
    #[serde_as(as = "Option<U64Hex>")]
    pub limit: Option<u64>,
    /// The cursor to start returning channels from.
    pub after: Option<JsonBytes>,
}

/// The Channel information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelInfo {
    /// The outpoint of the channel.
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
    /// The identity public key of the first node.
    pub node1: Pubkey,
    /// The identity public key of the second node.
    pub node2: Pubkey,
    /// The created timestamp of the channel, which is the block header timestamp of the block
    /// that contains the channel funding transaction.
    #[serde_as(as = "U64Hex")]
    pub created_timestamp: u64,

    /// The update info from node1 to node2, e.g. timestamp, fee_rate, tlc_expiry_delta, tlc_minimum_value
    pub update_info_of_node1: Option<ChannelUpdateInfo>,

    /// The update info from node2 to node1, e.g. timestamp, fee_rate, tlc_expiry_delta, tlc_minimum_value
    pub update_info_of_node2: Option<ChannelUpdateInfo>,

    /// The capacity of the channel.
    #[serde_as(as = "U128Hex")]
    pub capacity: u128,
    /// The chain hash of the channel.
    pub chain_hash: Hash256,
    /// The UDT type script of the channel.
    pub udt_type_script: Option<Script>,
}

impl From<super::super::fiber::graph::ChannelInfo> for ChannelInfo {
    fn from(channel_info: super::super::fiber::graph::ChannelInfo) -> Self {
        ChannelInfo {
            channel_outpoint: channel_info.out_point().clone(),
            node1: channel_info.node1(),
            node2: channel_info.node2(),
            created_timestamp: channel_info.timestamp,
            update_info_of_node1: channel_info.update_of_node1,
            update_info_of_node2: channel_info.update_of_node2,
            capacity: channel_info.capacity(),
            chain_hash: get_chain_hash(),
            udt_type_script: channel_info.udt_type_script().clone().map(|s| s.into()),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GraphChannelsResult {
    /// A list of channels.
    pub channels: Vec<ChannelInfo>,
    /// The last cursor for pagination.
    pub last_cursor: JsonBytes,
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
