use super::network::{get_chain_hash, SendPaymentData};
use super::path::NodeHeap;
use super::types::Pubkey;
use super::types::{ChannelAnnouncement, ChannelUpdate, Hash256, NodeAnnouncement};
use crate::fiber::path::{NodeHeapElement, ProbabilityEvaluator};
use crate::fiber::types::OnionInfo;
use crate::invoice::CkbInvoice;
use ckb_jsonrpc_types::JsonBytes;
use ckb_types::packed::{OutPoint, Script};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;
use tentacle::multiaddr::Multiaddr;
use tentacle::secio::PeerId;
use thiserror::Error;
use tracing::log::error;
use tracing::{debug, info, warn};

const DEFAULT_MIN_PROBABILITY: f64 = 0.01;

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Details about a node in the network, known from the network announcement.
pub struct NodeInfo {
    pub node_id: Pubkey,

    // The time when the node was last updated. This is the time of processing the message,
    // not the time of the NodeAnnouncement itself.
    pub timestamp: u64,

    pub anouncement_msg: NodeAnnouncement,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub funding_tx_block_number: u64,
    pub funding_tx_index: u32,
    pub announcement_msg: ChannelAnnouncement,
    pub node1_to_node2: Option<ChannelUpdateInfo>,
    pub node2_to_node1: Option<ChannelUpdateInfo>,
    // The time that the channel was announced to the network.
    pub timestamp: u64,
}

impl ChannelInfo {
    pub fn out_point(&self) -> OutPoint {
        self.announcement_msg.channel_outpoint.clone()
    }

    pub fn node1(&self) -> Pubkey {
        self.announcement_msg.node1_id
    }

    pub fn node2(&self) -> Pubkey {
        self.announcement_msg.node2_id
    }

    pub fn channel_annoucement_timestamp(&self) -> u64 {
        self.timestamp
    }

    pub fn node1_to_node2_channel_update_flags(&self) -> u8 {
        1
    }

    pub fn node2_to_node1_channel_update_flags(&self) -> u8 {
        0
    }

    pub fn channel_update_node1_to_node2_timestamp(&self) -> Option<u64> {
        self.node1_to_node2.as_ref().map(|x| x.timestamp)
    }

    pub fn channel_update_node2_to_node1_timestamp(&self) -> Option<u64> {
        self.node2_to_node1.as_ref().map(|x| x.timestamp)
    }

    pub fn channel_last_update_time(&self) -> Option<u64> {
        self.node1_to_node2
            .as_ref()
            .map(|n| n.timestamp)
            .max(self.node2_to_node1.as_ref().map(|n| n.timestamp))
    }

    pub fn is_enabled(&self) -> bool {
        self.node1_to_node2.is_some() && self.node2_to_node1.is_some()
    }

    pub fn capacity(&self) -> u128 {
        self.announcement_msg.capacity
    }

    pub fn funding_tx_block_number(&self) -> u64 {
        self.funding_tx_block_number
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelUpdateInfo {
    // The version is a number that represents the newness of the channel update.
    // It is set by the node that sends the channel update. Larger number means newer update.
    pub version: u64,
    // The timestamp is the time when the channel update was received by the node.
    pub timestamp: u64,
    /// Whether the channel can be currently used for payments (in this one direction).
    pub enabled: bool,
    /// The difference in CLTV values that you must have when routing through this channel.
    pub cltv_expiry_delta: u64,
    /// The minimum value, which must be relayed to the next hop via the channel
    pub htlc_minimum_value: u128,
    /// The maximum value which may be relayed to the next hop via the channel.
    pub htlc_maximum_value: u128,
    pub fee_rate: u64,
    /// Most recent update for the channel received from the network
    /// Mostly redundant with the data we store in fields explicitly.
    /// Everything else is useful only for sending out for initial routing sync.
    /// Not stored if contains excess data to prevent DoS.
    pub last_update_message: ChannelUpdate,
}

#[derive(Clone, Debug)]
pub struct NetworkGraph<S> {
    source: Pubkey,
    channels: HashMap<OutPoint, ChannelInfo>,
    // This is the best height of the network graph, every time the
    // node restarts, we will try to sync the graph from this height - ASSUME_MAX_CHANNEL_HEIGHT_GAP.
    // We assume that we have already synced the graph up to this height - ASSUME_MAX_CHANNEL_HEIGHT_GAP.
    best_height: u64,
    // Similar to the best_height, this is the last update time of the network graph.
    // We assume that we have already synced the graph up to this time - ASSUME_MAX_MESSAGE_TIMESTAMP_GAP.
    last_update_timestamp: u64,
    // when we restarting a node, we will reconnect to these peers
    connected_peer_addresses: HashMap<PeerId, Multiaddr>,
    nodes: HashMap<Pubkey, NodeInfo>,
    store: S,
    chain_hash: Hash256,
}

#[derive(Error, Debug)]
pub enum GraphError {
    #[error("Graph error: {0}")]
    Amount(String),
    #[error("PathFind error: {0}")]
    PathFind(String),
    #[error("Graph other error: {0}")]
    Other(String),
}

#[derive(Clone, Debug)]
pub struct PathEdge {
    pub target: Pubkey,
    pub channel_outpoint: OutPoint,
}

impl<S> NetworkGraph<S>
where
    S: NetworkGraphStateStore + Clone + Send + Sync + 'static,
{
    pub fn new(store: S, source: Pubkey) -> Self {
        let mut network_graph = Self {
            source,
            best_height: 0,
            last_update_timestamp: 0,
            channels: HashMap::new(),
            nodes: HashMap::new(),
            connected_peer_addresses: HashMap::new(),
            store,
            chain_hash: get_chain_hash(),
        };
        network_graph.load_from_store();
        network_graph
    }

    pub fn chain_hash(&self) -> Hash256 {
        self.chain_hash
    }

    fn load_from_store(&mut self) {
        let channels = self.store.get_channels(None);
        for channel in channels.iter() {
            if self.best_height < channel.funding_tx_block_number() {
                self.best_height = channel.funding_tx_block_number();
            }
            if self.last_update_timestamp < channel.timestamp {
                self.last_update_timestamp = channel.timestamp;
            }
            if let Some(channel_update) = channel.node1_to_node2.as_ref() {
                if self.last_update_timestamp < channel_update.timestamp {
                    self.last_update_timestamp = channel_update.timestamp;
                }
            }
            if let Some(channel_update) = channel.node2_to_node1.as_ref() {
                if self.last_update_timestamp < channel_update.timestamp {
                    self.last_update_timestamp = channel_update.timestamp;
                }
            }
            self.channels.insert(channel.out_point(), channel.clone());
        }
        let nodes = self.store.get_nodes(None);
        for node in nodes.iter() {
            if self.last_update_timestamp < node.timestamp {
                self.last_update_timestamp = node.timestamp;
            }
            self.nodes.insert(node.node_id, node.clone());
        }
        for (peer, addr) in self.store.get_connected_peer(None) {
            self.connected_peer_addresses.insert(peer, addr);
        }
    }

    pub fn get_best_height(&self) -> u64 {
        self.best_height
    }

    pub fn get_last_update_timestamp(&self) -> u64 {
        self.last_update_timestamp
    }

    pub fn add_node(&mut self, node_info: NodeInfo) {
        debug!("Adding node to network graph: {:?}", node_info);
        let node_id = node_info.node_id;
        if let Some(old_node) = self.nodes.get(&node_id) {
            if old_node.anouncement_msg.version > node_info.anouncement_msg.version {
                warn!(
                    "Ignoring adding an outdated node info because old node version {} > new node version {}, new node info {:?}, existing node {:?}",
                    old_node.anouncement_msg.version, node_info.anouncement_msg.version,
                    &node_info, &old_node
                );
                return;
            } else if old_node.anouncement_msg.version == node_info.anouncement_msg.version {
                debug!("Repeatedly adding node info, ignoring: {:?}", node_info);
                return;
            }
        }
        if self.last_update_timestamp < node_info.timestamp {
            self.last_update_timestamp = node_info.timestamp;
        }
        self.nodes.insert(node_id, node_info.clone());
        self.store.insert_node(node_info);
    }

    // TODO: If we are syncing with the peers for newest graph, we should
    // not process channels here. Because if the node may restart while syncing is
    // is still ongoing, the next time when the node starts, it may falsely believe
    // that we have already processed channels before the height of this channel.
    pub fn add_channel(&mut self, channel_info: ChannelInfo) {
        assert_ne!(channel_info.node1(), channel_info.node2());
        debug!("Adding channel to network graph: {:?}", channel_info);
        if self.best_height < channel_info.funding_tx_block_number {
            self.best_height = channel_info.funding_tx_block_number;
        }
        if self.last_update_timestamp < channel_info.timestamp {
            self.last_update_timestamp = channel_info.timestamp;
        }
        match self.channels.get(&channel_info.out_point()) {
            Some(channel) => {
                // If the channel already exists, we don't need to update it
                // FIXME: if other fields is different, we should consider it as malioucious and ban the node?
                if channel.node1_to_node2.is_some() || channel.node2_to_node1.is_some() {
                    debug!("channel already exists, ignoring: {:?}", &channel_info);
                    return;
                }
            }
            None => {
                debug!(
                    "Channel not found, saving it to database {:?}",
                    &channel_info
                );
            }
        }
        if let Some(node) = self.nodes.get_mut(&channel_info.node1()) {
            self.store.insert_node(node.clone());
        } else {
            // It is possible that the node announcement is after broadcasted after the channel announcement.
            // So don't just ignore the channel even if we didn't find the node info here.
            warn!("Node1 not found for channel {:?}", &channel_info);
        }
        if let Some(node) = self.nodes.get_mut(&channel_info.node2()) {
            self.store.insert_node(node.clone());
        } else {
            warn!("Node2 not found for channel {:?}", &channel_info);
        }

        let outpoint = channel_info.out_point();
        self.channels.insert(outpoint.clone(), channel_info.clone());
        self.store.insert_channel(channel_info);
        debug!("Successfully added channel {:?}", outpoint);
    }

    pub fn nodes(&self) -> impl Iterator<Item = &NodeInfo> {
        self.nodes.values()
    }

    pub fn get_nodes_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
    ) -> (Vec<NodeInfo>, JsonBytes) {
        self.store.get_nodes_with_params(limit, after, None)
    }

    pub fn get_channels_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
    ) -> (Vec<ChannelInfo>, JsonBytes) {
        self.store.get_channels_with_params(limit, after, None)
    }

    pub fn get_node(&self, node_id: Pubkey) -> Option<&NodeInfo> {
        self.nodes.get(&node_id)
    }

    pub fn channels(&self) -> impl Iterator<Item = &ChannelInfo> {
        self.channels.values()
    }

    pub fn get_channel(&self, outpoint: &OutPoint) -> Option<&ChannelInfo> {
        self.channels.get(outpoint)
    }

    pub fn get_channels_by_peer(&self, node_id: Pubkey) -> impl Iterator<Item = &ChannelInfo> {
        self.channels
            .values()
            .filter(move |channel| channel.node1() == node_id || channel.node2() == node_id)
    }

    pub fn get_channels_within_block_range(
        &self,
        start_block: u64,
        end_block: u64,
    ) -> (impl Iterator<Item = &ChannelInfo>, u64, bool) {
        (
            self.channels.values().filter(move |channel| {
                channel.funding_tx_block_number >= start_block
                    && channel.funding_tx_block_number < end_block
            }),
            end_block,
            self.channels
                .values()
                .any(|channel| channel.funding_tx_block_number >= end_block),
        )
    }

    pub fn process_channel_update(&mut self, update: ChannelUpdate) -> Result<(), GraphError> {
        debug!("Processing channel update: {:?}", &update);
        let channel_outpoint = &update.channel_outpoint;
        let Some(channel) = self.channels.get_mut(channel_outpoint) else {
            return Err(GraphError::Other("channel not found".to_string()));
        };
        debug!(
            "Found channel {:?} for channel update {:?}",
            &channel, &update
        );
        let update_info = if update.message_flags & 1 == 1 {
            &mut channel.node1_to_node2
        } else {
            &mut channel.node2_to_node1
        };

        if let Some(info) = update_info {
            if update.version <= info.version {
                // update.version == info.version happens most possibly because we received the
                // broadcast many times. Don't emit too many logs in that case.
                if update.version < info.version {
                    warn!(
                        "Ignoring updating with an outdated channel update {:?} for channel {:?}, current update info: {:?}",
                        &update, channel_outpoint, &info
                    );
                }
                return Ok(());
            }
        }
        *update_info = Some(ChannelUpdateInfo {
            version: update.version,
            timestamp: std::time::UNIX_EPOCH.elapsed().unwrap().as_millis() as u64,
            enabled: true,
            cltv_expiry_delta: update.tlc_locktime_expiry_delta,
            htlc_minimum_value: update.tlc_minimum_value,
            htlc_maximum_value: update.tlc_maximum_value,
            fee_rate: update.tlc_fee_proportional_millionths as u64,
            last_update_message: update.clone(),
        });

        self.store.insert_channel(channel.to_owned());
        debug!(
            "Processed channel update: channel {:?}, update {:?}",
            &channel, &update
        );
        Ok(())
    }

    pub fn check_chain_hash(&self, chain_hash: Hash256) -> bool {
        self.chain_hash == chain_hash
    }

    pub fn add_connected_peer(&mut self, peer_id: &PeerId, address: Multiaddr) {
        self.connected_peer_addresses
            .insert(peer_id.clone(), address.clone());
        self.store.insert_connected_peer(peer_id.clone(), address);
    }

    pub fn get_connected_peers(&self) -> Vec<(&PeerId, &Multiaddr)> {
        self.connected_peer_addresses.iter().collect()
    }

    pub fn get_peers_to_sync_network_graph(&self) -> Vec<(&PeerId, &Multiaddr)> {
        self.connected_peer_addresses.iter().take(3).collect()
    }

    pub fn remove_connected_peer(&mut self, peer_id: &PeerId) {
        self.connected_peer_addresses.remove(peer_id);
        self.store.remove_connected_peer(peer_id);
    }

    pub fn get_node_inbounds(
        &self,
        node_id: Pubkey,
    ) -> impl Iterator<Item = (Pubkey, &ChannelInfo, &ChannelUpdateInfo)> {
        self.channels.values().filter_map(move |channel| {
            if let Some(info) = channel.node1_to_node2.as_ref() {
                if info.enabled && channel.node2() == node_id {
                    return Some((channel.node1(), channel, info));
                }
            }

            if let Some(info) = channel.node2_to_node1.as_ref() {
                if info.enabled && channel.node1() == node_id {
                    return Some((channel.node2(), channel, info));
                }
            }
            None
        })
    }

    pub fn get_source_pubkey(&self) -> Pubkey {
        self.source
    }

    pub fn calculate_fee(&self, amount: u128, fee_proportational_millionths: u128) -> u128 {
        let fee = fee_proportational_millionths * amount;
        let base_fee = fee / 1_000_000;
        let remainder = fee % 1_000_000;
        if remainder > 0 {
            base_fee + 1
        } else {
            base_fee
        }
    }

    #[cfg(test)]
    pub fn reset(&mut self) {
        self.channels.clear();
        self.nodes.clear();
        self.connected_peer_addresses.clear();
    }

    pub fn build_route(
        &self,
        payment_request: SendPaymentData,
    ) -> Result<Vec<OnionInfo>, GraphError> {
        let source = self.get_source_pubkey();
        let target = payment_request.target_pubkey;
        let amount = payment_request.amount;
        let preimage = payment_request.preimage;
        let payment_hash = payment_request.payment_hash;
        let udt_type_script = payment_request.udt_type_script;
        let invoice = payment_request
            .invoice
            .map(|x| x.parse::<CkbInvoice>().unwrap());
        let hash_algorithm = invoice
            .as_ref()
            .and_then(|x| x.hash_algorithm().copied())
            .unwrap_or_default();

        info!(
            "build_route source: {:?} target: {:?} amount: {:?}, payment_hash: {:?}",
            source, target, amount, payment_hash
        );

        let route = self.find_route(
            source,
            target,
            amount,
            payment_request.max_fee_amount,
            udt_type_script,
        )?;
        assert!(!route.is_empty());

        let mut current_amount = amount;
        let mut current_expiry = 0;
        let mut onion_infos = vec![];
        for i in (0..route.len()).rev() {
            let is_last = i == route.len() - 1;
            let (next_hop, next_channel_outpoint) = if is_last {
                (None, None)
            } else {
                (
                    Some(route[i + 1].target),
                    Some(route[i + 1].channel_outpoint.clone()),
                )
            };
            let (fee, expiry) = if is_last {
                (0, 0)
            } else {
                let channel_info = self
                    .get_channel(&route[i + 1].channel_outpoint)
                    .expect("channel not found");
                let channel_update = &if channel_info.node1() == route[i + 1].target {
                    channel_info.node2_to_node1.as_ref()
                } else {
                    channel_info.node1_to_node2.as_ref()
                }
                .expect("channel_update is none");
                let fee_rate = channel_update.fee_rate;
                let fee = self.calculate_fee(current_amount, fee_rate as u128);
                let expiry = channel_update.cltv_expiry_delta;
                (fee, expiry)
            };

            // make sure the final hop's amount is the same as the payment amount
            // the last hop will check the amount from TLC and the amount from the onion packet
            onion_infos.push(OnionInfo {
                amount: current_amount,
                payment_hash,
                next_hop,
                tlc_hash_algorithm: hash_algorithm,
                expiry: current_expiry,
                channel_outpoint: next_channel_outpoint,
                preimage: if is_last { preimage } else { None },
            });
            current_amount += fee;
            current_expiry += expiry;
        }
        // add the first hop so that the logic for send HTLC can be reused
        let next_hop = if !route.is_empty() {
            Some(route[0].target)
        } else {
            None
        };
        onion_infos.push(OnionInfo {
            amount: current_amount,
            payment_hash,
            next_hop,
            tlc_hash_algorithm: hash_algorithm,
            expiry: current_expiry,
            channel_outpoint: Some(route[0].channel_outpoint.clone()),
            preimage: None,
        });
        onion_infos.reverse();
        assert_eq!(onion_infos.len(), route.len() + 1);
        assert_eq!(onion_infos[route.len()].amount, amount);
        Ok(onion_infos)
    }

    // the algorithm works from target-to-source to find the shortest path
    pub fn find_route(
        &self,
        source: Pubkey,
        target: Pubkey,
        amount: u128,
        max_fee_amount: Option<u128>,
        udt_type_script: Option<Script>,
    ) -> Result<Vec<PathEdge>, GraphError> {
        let started_time = std::time::Instant::now();
        let nodes_len = self.nodes.len();
        let mut result = vec![];
        let mut nodes_visited = 0;
        let mut edges_expanded = 0;
        let mut nodes_heap = NodeHeap::new(nodes_len);
        let mut distances = HashMap::<Pubkey, NodeHeapElement>::new();

        if amount == 0 {
            return Err(GraphError::Amount(
                "Amount must be greater than 0".to_string(),
            ));
        }

        if source == target {
            return Err(GraphError::PathFind(
                "source and target are the same".to_string(),
            ));
        }
        let Some(source_node) = self.nodes.get(&source) else {
            return Err(GraphError::PathFind("source node not found".to_string()));
        };
        let Some(_target_node) = self.nodes.get(&target) else {
            return Err(GraphError::PathFind("target node not found".to_string()));
        };
        // initialize the target node
        nodes_heap.push(NodeHeapElement {
            node_id: target,
            weight: 0,
            distance: 0,
            amount_received: amount,
            fee_charged: 0,
            probability: 1.0,
            next_hop: None,
            incoming_cltv_height: 0,
        });
        loop {
            nodes_visited += 1;
            let Some(cur_hop) = nodes_heap.pop() else {
                break;
            };

            if cur_hop.node_id == source {
                break;
            }

            for (from, channel_info, channel_update) in self.get_node_inbounds(cur_hop.node_id) {
                edges_expanded += 1;
                // if charge inbound fees for exit hop
                let fee_rate = channel_update.fee_rate;
                let next_hop_received_amount = cur_hop.amount_received;
                let fee = self.calculate_fee(next_hop_received_amount, fee_rate as u128);
                let amount_to_send = next_hop_received_amount + fee;

                debug!(
                    "fee_rate: {:?} next_hop_received_amount: {:?}, fee: {:?} amount_to_send: {:?} channel_capacity: {:?} htlc_max_value: {:?}",
                    fee_rate, next_hop_received_amount, fee, amount_to_send, channel_info.capacity(), channel_update.htlc_maximum_value
                );

                if udt_type_script != channel_info.announcement_msg.udt_type_script {
                    continue;
                }

                // if the amount to send is greater than the amount we have, skip this edge
                if let Some(max_fee_amount) = max_fee_amount {
                    if amount_to_send > amount + max_fee_amount {
                        debug!(
                            "amount_to_send: {:?} is greater than sum_amount sum_amount: {:?}",
                            amount_to_send,
                            amount + max_fee_amount
                        );
                        continue;
                    }
                }
                // check to make sure the current hop can send the amount
                // if `htlc_maximum_value` equals 0, it means there is no limit
                if amount_to_send > channel_info.capacity()
                    || (channel_update.htlc_maximum_value != 0
                        && amount_to_send > channel_update.htlc_maximum_value)
                {
                    debug!(
                        "amount_to_send is greater than channel capacity: {:?} capacity: {:?}, htlc_max_value: {:?}",
                        amount_to_send,
                        channel_info.capacity(),
                        channel_update.htlc_maximum_value
                    );
                    continue;
                }
                if amount_to_send < channel_update.htlc_minimum_value {
                    debug!(
                        "amount_to_send is less than htlc_minimum_value: {:?} min_value: {:?}",
                        amount_to_send, channel_update.htlc_minimum_value
                    );
                    continue;
                }
                let incomming_cltv = cur_hop.incoming_cltv_height
                    + if from == source {
                        0
                    } else {
                        channel_update.cltv_expiry_delta
                    };

                let probability = cur_hop.probability
                    * ProbabilityEvaluator::evaluate_probability(
                        from,
                        cur_hop.node_id,
                        amount_to_send,
                        channel_info.capacity(),
                    );

                if probability < DEFAULT_MIN_PROBABILITY {
                    debug!("probability is too low: {:?}", probability);
                    continue;
                }
                debug!("probability: {:?}", probability);
                let agg_weight =
                    self.edge_weight(amount_to_send, fee, channel_update.cltv_expiry_delta);
                let weight = cur_hop.weight + agg_weight;
                let distance = self.calculate_distance_based_probability(probability, weight);

                if let Some(node) = distances.get(&from) {
                    if distance >= node.distance {
                        continue;
                    }
                }
                // info!("weight: {:?} dist: {:?} fee: {:?}", weight, dist, fee);
                // TODO: update weight and distance here
                let node: NodeHeapElement = NodeHeapElement {
                    node_id: from,
                    weight,
                    distance,
                    amount_received: amount_to_send,
                    incoming_cltv_height: incomming_cltv,
                    fee_charged: fee,
                    probability,
                    next_hop: Some((cur_hop.node_id, channel_info.out_point())),
                };
                distances.insert(node.node_id, node.clone());
                nodes_heap.push_or_fix(node);
            }
        }

        let mut current = source_node.node_id;
        while current != target {
            if let Some(elem) = distances.get(&current) {
                let next_hop = elem.next_hop.as_ref().expect("next_hop is none");
                result.push(PathEdge {
                    target: next_hop.0,
                    channel_outpoint: next_hop.1.clone(),
                });
                current = next_hop.0;
            } else {
                break;
            }
        }

        info!(
            "get_route: nodes visited: {}, edges expanded: {}, time: {:?}",
            nodes_visited,
            edges_expanded,
            started_time.elapsed()
        );
        if result.is_empty() || current != target {
            return Err(GraphError::PathFind("no path found".to_string()));
        }
        Ok(result)
    }

    fn edge_weight(&self, amount: u128, fee: u128, cltv_expiry_delta: u64) -> u128 {
        let risk_factor: u128 = 15;
        let time_lock_penalty = amount * cltv_expiry_delta as u128 * (risk_factor / 1000000000);
        fee + time_lock_penalty
    }

    fn calculate_distance_based_probability(&self, probability: f64, weight: u128) -> u128 {
        // FIXME: set this to configurable parameters
        let weight = weight as f64;
        let time_pref = 0.5_f64;
        let default_attemp_cost = 0.1_f64;
        let penalty = default_attemp_cost * (1.0 / (0.5 - time_pref / 2.0) - 1.0);
        weight as u128 + (penalty / probability) as u128
    }
}

pub trait NetworkGraphStateStore {
    fn get_channels(&self, outpoint: Option<OutPoint>) -> Vec<ChannelInfo>;
    fn get_nodes(&self, peer_id: Option<Pubkey>) -> Vec<NodeInfo>;
    fn get_nodes_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
        node_id: Option<Pubkey>,
    ) -> (Vec<NodeInfo>, JsonBytes);
    fn get_channels_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
        outpoint: Option<OutPoint>,
    ) -> (Vec<ChannelInfo>, JsonBytes);
    fn insert_channel(&self, channel: ChannelInfo);
    fn insert_node(&self, node: NodeInfo);
    fn insert_connected_peer(&self, peer_id: PeerId, multiaddr: Multiaddr);
    fn get_connected_peer(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Multiaddr)>;
    fn remove_connected_peer(&self, peer_id: &PeerId);
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession>;
    fn insert_payment_session(&self, session: PaymentSession);
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PaymentSessionStatus {
    // initial status, payment session is created, no HTLC is sent
    Created,
    // related HTLC is send and waiting for the response
    Inflight,
    // related HTLC is successfully settled
    Success,
    // related HTLC is failed
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentSession {
    pub request: SendPaymentData,
    pub retried_times: u32,
    pub last_error: Option<String>,
    pub try_limit: u32,
    pub status: PaymentSessionStatus,
    pub created_time: u128,
    pub last_updated_time: u128,
}

impl PaymentSession {
    pub fn new(request: SendPaymentData, try_limit: u32) -> Self {
        let now = std::time::UNIX_EPOCH.elapsed().unwrap().as_millis();
        Self {
            request,
            retried_times: 0,
            last_error: None,
            try_limit,
            status: PaymentSessionStatus::Created,
            created_time: now,
            last_updated_time: now,
        }
    }

    pub fn payment_hash(&self) -> Hash256 {
        self.request.payment_hash
    }

    pub fn set_status(&mut self, status: PaymentSessionStatus) {
        self.status = status;
    }

    pub fn set_failed_status(&mut self, error: String) {
        self.status = PaymentSessionStatus::Failed;
        self.last_error = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fiber::network::SendPaymentData;
    use crate::fiber::test_utils::{generate_keypair, generate_pubkey};
    use crate::store::Store;
    use ckb_types::prelude::Entity;
    use secp256k1::{PublicKey, SecretKey, XOnlyPublicKey};

    fn generate_keys(num: usize) -> Vec<PublicKey> {
        let mut keys = vec![];
        for _ in 0..num {
            keys.push(generate_pubkey());
        }
        keys
    }

    fn generate_key_pairs(num: usize) -> Vec<(SecretKey, PublicKey)> {
        let mut keys = vec![];
        for _ in 0..num {
            keys.push(generate_keypair());
        }
        keys
    }

    struct MockNetworkGraph {
        pub keys: Vec<PublicKey>,
        pub edges: Vec<(usize, usize, OutPoint)>,
        pub graph: NetworkGraph<Store>,
    }

    impl MockNetworkGraph {
        pub fn new(node_num: usize) -> Self {
            let temp_path = tempfile::tempdir().unwrap();
            let store = Store::new(temp_path.path());
            let keypairs = generate_key_pairs(node_num + 1);
            let (secret_key1, public_key1) = keypairs[0];
            let mut graph = NetworkGraph::new(store, public_key1.into());
            graph.add_node(NodeInfo {
                node_id: public_key1.into(),
                timestamp: 0,
                anouncement_msg: NodeAnnouncement::new(
                    "node0".into(),
                    vec![],
                    &secret_key1.into(),
                    0,
                ),
            });
            for i in 1..keypairs.len() {
                let (sk, pk) = keypairs[i];
                let node = NodeInfo {
                    node_id: pk.into(),
                    timestamp: 0,
                    anouncement_msg: NodeAnnouncement::new(
                        format!("node{i}").as_str().into(),
                        vec![],
                        &sk.into(),
                        0,
                    ),
                };
                graph.add_node(node);
            }
            Self {
                keys: keypairs.into_iter().map(|x| x.1).collect(),
                edges: vec![],
                graph,
            }
        }

        pub fn add_edge_with_config(
            &mut self,
            node_a: usize,
            node_b: usize,
            capacity: Option<u128>,
            fee_rate: Option<u128>,
            min_htlc_value: Option<u128>,
            max_htlc_value: Option<u128>,
            udt_type_script: Option<Script>,
        ) {
            let public_key1 = self.keys[node_a];
            let public_key2 = self.keys[node_b];
            let idx = self.edges.len() + 1;
            let channel_outpoint = OutPoint::from_slice(&[idx as u8; 36]).unwrap();
            self.edges.push((node_a, node_b, channel_outpoint.clone()));
            let channel_info = ChannelInfo {
                funding_tx_block_number: 0,
                funding_tx_index: 0,
                announcement_msg: ChannelAnnouncement {
                    chain_hash: get_chain_hash(),
                    node1_id: public_key1.into(),
                    node2_id: public_key2.into(),
                    channel_outpoint: channel_outpoint.clone(),
                    node1_signature: None,
                    node2_signature: None,
                    capacity: capacity.unwrap_or(1000),
                    ckb_key: XOnlyPublicKey::from_slice([0x01; 32].as_ref()).unwrap(),
                    ckb_signature: None,
                    udt_type_script,
                    features: 0,
                },
                timestamp: 0,
                node1_to_node2: None,
                node2_to_node1: None,
            };
            self.graph.add_channel(channel_info);
            let channel_update = ChannelUpdate {
                signature: None,
                chain_hash: get_chain_hash(),
                version: 0,
                message_flags: 1,
                channel_flags: 0,
                tlc_locktime_expiry_delta: 144,
                tlc_fee_proportional_millionths: fee_rate.unwrap_or(0),
                tlc_maximum_value: max_htlc_value.unwrap_or(10000),
                tlc_minimum_value: min_htlc_value.unwrap_or(0),
                channel_outpoint: channel_outpoint.clone(),
            };
            self.graph.process_channel_update(channel_update).unwrap();
        }

        pub fn add_edge(
            &mut self,
            node_a: usize,
            node_b: usize,
            capacity: Option<u128>,
            fee_rate: Option<u128>,
        ) {
            self.add_edge_with_config(
                node_a,
                node_b,
                capacity,
                fee_rate,
                Some(0),
                Some(10000),
                None,
            );
        }

        pub fn add_edge_udt(
            &mut self,
            node_a: usize,
            node_b: usize,
            capacity: Option<u128>,
            fee_rate: Option<u128>,
            udt_type_script: Script,
        ) {
            self.add_edge_with_config(
                node_a,
                node_b,
                capacity,
                fee_rate,
                Some(0),
                Some(10000),
                Some(udt_type_script),
            );
        }

        pub fn find_route(
            &self,
            source: usize,
            target: usize,
            amount: u128,
            max_fee: u128,
        ) -> Result<Vec<PathEdge>, GraphError> {
            let source = self.keys[source].into();
            let target = self.keys[target].into();
            self.graph
                .find_route(source, target, amount, Some(max_fee), None)
        }

        pub fn find_route_udt(
            &self,
            source: usize,
            target: usize,
            amount: u128,
            max_fee: u128,
            udt_type_script: Script,
        ) -> Result<Vec<PathEdge>, GraphError> {
            let source = self.keys[source].into();
            let target = self.keys[target].into();
            self.graph
                .find_route(source, target, amount, Some(max_fee), Some(udt_type_script))
        }
    }

    #[test]
    fn test_graph_connected_peers() {
        let temp_path = tempfile::tempdir().unwrap();
        let store = Store::new(temp_path.path());
        let keys = generate_keys(1);
        let public_key1 = keys[0];
        let mut network_graph = NetworkGraph::new(store, public_key1.into());

        let peer_id = PeerId::random();
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/10000".parse().unwrap();
        network_graph.add_connected_peer(&peer_id, address.clone());

        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 1);
        assert_eq!(connected_peers[0], (&peer_id, &address));

        network_graph.reset();
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 0);

        // load from db
        network_graph.load_from_store();
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 1);
        assert_eq!(connected_peers[0], (&peer_id, &address));

        network_graph.remove_connected_peer(&peer_id);
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 0);

        network_graph.load_from_store();
        let connected_peers = network_graph.get_connected_peers();
        assert_eq!(connected_peers.len(), 0);
    }

    #[test]
    fn test_graph_channel_info() {
        let mut mock_network = MockNetworkGraph::new(1);
        mock_network.add_edge(0, 1, Some(1000), Some(1));
        for i in 1..=mock_network.edges.len() {
            let channel_info = mock_network
                .graph
                .get_channel(&OutPoint::from_slice(&[i as u8; 36]).unwrap());
            assert!(channel_info.is_some());

            let channel_info = channel_info.unwrap();
            let channel_info_ser = serde_json::to_string(&channel_info).unwrap();
            let channel_info_de: ChannelInfo = serde_json::from_str(&channel_info_ser).unwrap();
            assert_eq!(*channel_info, channel_info_de);
        }
    }

    #[test]
    fn test_graph_graph_apis() {
        let mut mock_network = MockNetworkGraph::new(4);
        let node1 = mock_network.keys[1];
        let node2 = mock_network.keys[2];
        let node3 = mock_network.keys[3];
        assert!(mock_network.graph.get_node(node1.into()).is_some());
        assert!(mock_network.graph.get_node(node2.into()).is_some());

        let node1_channels = mock_network.graph.get_channels_by_peer(node1.into());
        assert_eq!(node1_channels.count(), 0);
        let node2_channels = mock_network.graph.get_channels_by_peer(node2.into());
        assert_eq!(node2_channels.count(), 0);

        mock_network.add_edge(1, 2, Some(1000), Some(1));
        let node1_channels = mock_network.graph.get_channels_by_peer(node1.into());
        assert_eq!(node1_channels.count(), 1);
        let node2_channels = mock_network.graph.get_channels_by_peer(node2.into());
        assert_eq!(node2_channels.count(), 1);

        mock_network.add_edge(1, 3, Some(1000), Some(1));
        let node1_channels = mock_network.graph.get_channels_by_peer(node1.into());
        assert_eq!(node1_channels.count(), 2);

        let node1_channels = mock_network.graph.get_channels_by_peer(node3.into());
        assert_eq!(node1_channels.count(), 1);
    }

    #[test]
    fn test_graph_find_path_basic() {
        let mut network = MockNetworkGraph::new(4);
        network.add_edge(1, 2, Some(1), Some(2));
        let node2 = network.keys[2];

        let route = network.find_route(1, 2, 100, 1000);
        assert!(route.is_err());

        network.add_edge(1, 2, Some(120), Some(2));
        let route = network.find_route(1, 2, 100, 1000);
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.len(), 1);
        assert_eq!(route[0].target, node2.into());
        assert_eq!(route[0].channel_outpoint, network.edges[1].2);

        let route = network.find_route(1, 3, 10, 100);
        assert!(route.is_err());
    }

    #[test]
    fn test_graph_find_path_three_nodes() {
        let mut network = MockNetworkGraph::new(3);
        network.add_edge(1, 2, Some(500), Some(2));
        network.add_edge(2, 3, Some(500), Some(2));
        let node2 = network.keys[2];
        let node3 = network.keys[3];

        // Test route from node 1 to node 3
        let route = network.find_route(1, 3, 100, 1000);
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.len(), 2);
        assert_eq!(route[0].target, node2.into());
        assert_eq!(route[1].target, node3.into());
        assert_eq!(route[0].channel_outpoint, network.edges[0].2);
        assert_eq!(route[1].channel_outpoint, network.edges[1].2);

        // Test route from node 1 to node 2
        let route = network.find_route(1, 2, 100, 1000);
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.len(), 1);
        assert_eq!(route[0].target, node2.into());
        assert_eq!(route[0].channel_outpoint, network.edges[0].2);

        // Test route from node 2 to node 3
        let route = network.find_route(2, 3, 100, 1000);
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.len(), 1);
        assert_eq!(route[0].target, node3.into());
        assert_eq!(route[0].channel_outpoint, network.edges[1].2);

        // Test route from node 3 to node 1 (should fail)
        let route = network.find_route(3, 1, 100, 1000);
        assert!(route.is_err());
    }

    #[test]
    fn test_graph_find_path_fee() {
        let mut network = MockNetworkGraph::new(5);

        network.add_edge(1, 2, Some(1000), Some(30000));
        network.add_edge(2, 4, Some(1000), Some(10000));

        network.add_edge(1, 3, Some(1000), Some(20000));
        network.add_edge(3, 4, Some(1000), Some(10000));

        let route = network.find_route(1, 4, 100, 1000);

        assert!(route.is_ok());
        let route = route.unwrap();

        // make sure we choose the path with lower fees
        assert_eq!(route.len(), 2);
        assert_eq!(route[0].channel_outpoint, network.edges[2].2);
        assert_eq!(route[1].channel_outpoint, network.edges[3].2);
    }

    #[test]
    fn test_graph_find_path_direct_linear() {
        let mut network = MockNetworkGraph::new(6);

        network.add_edge(1, 2, Some(1000), Some(4));
        network.add_edge(2, 3, Some(1000), Some(3));
        network.add_edge(3, 4, Some(1000), Some(2));
        network.add_edge(4, 5, Some(1000), Some(1));

        let route = network.find_route(1, 5, 100, 1000);

        assert!(route.is_ok());
        let route = route.unwrap();

        assert_eq!(route.len(), 4);
        assert_eq!(route[0].channel_outpoint, network.edges[0].2);
        assert_eq!(route[1].channel_outpoint, network.edges[1].2);
        assert_eq!(route[2].channel_outpoint, network.edges[2].2);
        assert_eq!(route[3].channel_outpoint, network.edges[3].2);
    }

    #[test]
    fn test_graph_find_path_cycle() {
        let mut network = MockNetworkGraph::new(6);

        network.add_edge(1, 2, Some(1000), Some(4));
        network.add_edge(2, 3, Some(1000), Some(3));
        network.add_edge(3, 1, Some(1000), Some(2));

        let route = network.find_route(1, 3, 100, 1000);

        assert!(route.is_ok());

        network.add_edge(3, 4, Some(1000), Some(2));
        network.add_edge(4, 5, Some(1000), Some(1));

        let route = network.find_route(1, 5, 100, 1000);
        assert!(route.is_ok());
    }

    #[test]
    fn test_graph_find_path_cycle_in_middle() {
        let mut network = MockNetworkGraph::new(6);

        network.add_edge(1, 2, Some(1000), Some(4));

        network.add_edge(2, 3, Some(1000), Some(3));
        network.add_edge(3, 4, Some(1000), Some(2));
        network.add_edge(4, 2, Some(1000), Some(2));

        network.add_edge(4, 5, Some(1000), Some(1));

        let route = network.find_route(1, 5, 100, 1000);
        assert!(route.is_ok());
    }

    #[test]
    fn test_graph_find_path_loop_exit() {
        let mut network = MockNetworkGraph::new(6);

        // node2 and node3 are connected with each other, node1 is disconnected
        network.add_edge(2, 3, Some(1000), Some(3));
        network.add_edge(3, 2, Some(1000), Some(2));

        let route = network.find_route(1, 3, 100, 1000);
        assert!(route.is_err());

        // now add a path from node1 to node2, so that node1 can reach node3
        network.add_edge(1, 2, Some(1000), Some(4));
        let route = network.find_route(1, 3, 100, 1000);
        assert!(route.is_ok());
    }

    #[test]
    fn test_graph_find_path_amount_failed() {
        let mut network = MockNetworkGraph::new(6);

        network.add_edge(1, 2, Some(1000), Some(4));
        network.add_edge(2, 3, Some(1000), Some(4));
        network.add_edge(3, 4, Some(1000), Some(4));
        network.add_edge(4, 5, Some(1000), Some(1));

        let route = network.find_route(1, 5, 1000, 10);
        assert!(route.is_err());
    }

    #[test]
    fn test_graph_find_optimal_path() {
        let mut network = MockNetworkGraph::new(6);

        // Direct path with high fee
        network.add_edge(1, 5, Some(2000), Some(50000));

        // Longer path with lower total fee
        network.add_edge(1, 2, Some(2000), Some(10000));
        network.add_edge(2, 3, Some(2000), Some(10000));
        network.add_edge(3, 4, Some(2000), Some(10000));
        network.add_edge(4, 5, Some(2000), Some(10000));

        // Path with insufficient capacity
        network.add_edge(1, 6, Some(500), Some(10000));
        network.add_edge(6, 5, Some(500), Some(10000));

        let route = network.find_route(1, 5, 1000, 1000);
        assert!(route.is_ok());
        let route = route.unwrap();

        // Check that the algorithm chose the longer path with lower fees
        assert_eq!(route.len(), 4);
        assert_eq!(route[0].channel_outpoint, network.edges[1].2);
        assert_eq!(route[1].channel_outpoint, network.edges[2].2);
        assert_eq!(route[2].channel_outpoint, network.edges[3].2);
        assert_eq!(route[3].channel_outpoint, network.edges[4].2);

        // Test with a smaller amount that allows using the direct path
        let small_route = network.find_route(1, 5, 100, 100);
        assert!(small_route.is_ok());
        let small_route = small_route.unwrap();

        // Check that the algorithm chose the direct path for a smaller amount
        assert_eq!(small_route.len(), 2);
        assert_eq!(small_route[0].channel_outpoint, network.edges[5].2);
        assert_eq!(small_route[1].channel_outpoint, network.edges[6].2);
    }

    #[test]
    fn test_graph_find_path_err() {
        let mut network = MockNetworkGraph::new(6);
        let (node1, _node5) = (network.keys[1], network.keys[5]);

        network.add_edge(1, 2, Some(1000), Some(4));
        let route = network.find_route(1, 1, 100, 1000);
        assert!(route.is_err());

        let no_exits_public_key = network.keys[0];
        let route = network.graph.find_route(
            node1.into(),
            no_exits_public_key.into(),
            100,
            Some(1000),
            None,
        );
        assert!(route.is_err());

        let route = network.graph.find_route(
            no_exits_public_key.into(),
            node1.into(),
            100,
            Some(1000),
            None,
        );
        assert!(route.is_err());
    }

    #[test]
    fn test_graph_build_route_three_nodes() {
        let mut network = MockNetworkGraph::new(3);
        network.add_edge(0, 2, Some(500), Some(2));
        network.add_edge(2, 3, Some(500), Some(2));
        let _node0 = network.keys[0];
        let node2 = network.keys[2];
        let node3 = network.keys[3];
        // Test build route from node1 to node3
        let route = network.graph.build_route(SendPaymentData {
            target_pubkey: node3.into(),
            amount: 100,
            payment_hash: Hash256::default(),
            invoice: None,
            final_cltv_delta: Some(100),
            timeout: Some(10),
            max_fee_amount: Some(1000),
            max_parts: None,
            keysend: false,
            udt_type_script: None,
            preimage: None,
        });
        eprintln!("return {:?}", route);
        assert!(route.is_ok());
        let route = route.unwrap();
        assert_eq!(route.len(), 3);
        assert_eq!(route[0].channel_outpoint, Some(network.edges[0].2.clone()));
        assert_eq!(route[1].channel_outpoint, Some(network.edges[1].2.clone()));

        assert_eq!(route[0].next_hop, Some(node2.into()));
        assert_eq!(route[1].next_hop, Some(node3.into()));
        assert_eq!(route[2].next_hop, None);

        assert_eq!(route[0].amount, 101);
        assert_eq!(route[1].amount, 100);
        assert_eq!(route[2].amount, 100);
    }

    #[test]
    fn test_graph_build_route_exceed_max_htlc_value() {
        let mut network = MockNetworkGraph::new(3);
        // Add edges with max_htlc_value set to 50
        network.add_edge_with_config(0, 2, Some(500), Some(2), None, Some(50), None);
        network.add_edge_with_config(2, 3, Some(500), Some(2), None, Some(50), None);
        let node3 = network.keys[3];

        // Test build route from node1 to node3 with amount exceeding max_htlc_value
        let route = network.graph.build_route(SendPaymentData {
            target_pubkey: node3.into(),
            amount: 100, // Exceeds max_htlc_value of 50
            payment_hash: Hash256::default(),
            invoice: None,
            final_cltv_delta: Some(100),
            timeout: Some(10),
            max_fee_amount: Some(1000),
            max_parts: None,
            keysend: false,
            udt_type_script: None,
            preimage: None,
        });
        assert!(route.is_err());
    }

    #[test]
    fn test_graph_build_route_below_min_htlc_value() {
        let mut network = MockNetworkGraph::new(3);
        // Add edges with min_htlc_value set to 50
        network.add_edge_with_config(0, 2, Some(500), Some(2), Some(50), None, None);
        network.add_edge_with_config(2, 3, Some(500), Some(2), Some(50), None, None);
        let node3 = network.keys[3];

        // Test build route from node1 to node3 with amount below min_htlc_value
        let route = network.graph.build_route(SendPaymentData {
            target_pubkey: node3.into(),
            amount: 10, // Below min_htlc_value of 50
            payment_hash: Hash256::default(),
            invoice: None,
            final_cltv_delta: Some(100),
            timeout: Some(10),
            max_fee_amount: Some(1000),
            max_parts: None,
            keysend: false,
            udt_type_script: None,
            preimage: None,
        });
        assert!(route.is_err());
    }

    #[test]
    fn test_graph_find_path_udt() {
        let mut network = MockNetworkGraph::new(3);
        let udt_type_script = Script::default();
        network.add_edge_udt(1, 2, Some(1000), Some(1), udt_type_script.clone());
        let node2 = network.keys[2];

        let route = network.find_route_udt(1, 2, 100, 1000, udt_type_script.clone());
        assert!(route.is_ok());

        let route = route.unwrap();
        assert_eq!(route.len(), 1);
        assert_eq!(route[0].target, node2.into());
        assert_eq!(route[0].channel_outpoint, network.edges[0].2);

        let route = network.find_route(1, 3, 10, 100);
        assert!(route.is_err());
    }
}
