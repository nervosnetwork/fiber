use super::channel::ChannelActorStateStore;
use super::config::AnnouncedNodeName;
use super::gossip::GossipMessageStore;
use super::history::{Direction, InternalResult, PaymentHistory, TimedResult};
use super::network::{get_chain_hash, HopHint, SendPaymentData, SendPaymentResponse};
use super::path::NodeHeap;
use super::types::{
    BroadcastMessageID, BroadcastMessageWithTimestamp, ChannelAnnouncement, ChannelUpdate, Hash256,
    NodeAnnouncement,
};
use super::types::{Cursor, Pubkey, TlcErr};
use crate::ckb::config::UdtCfgInfos;
use crate::fiber::fee::calculate_tlc_forward_fee;
use crate::fiber::path::NodeHeapElement;
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::types::PaymentHopData;
use crate::invoice::CkbInvoice;
use crate::now_timestamp_as_millis_u64;
use ckb_types::packed::{OutPoint, Script};
use rand::seq::SliceRandom;
use rand::thread_rng;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::{HashMap, HashSet};
use tentacle::multiaddr::MultiAddr;
use tentacle::secio::PeerId;
use tentacle::utils::{is_reachable, multiaddr_to_socketaddr};
use thiserror::Error;
use tracing::log::error;
use tracing::{debug, info, trace};

const DEFAULT_MIN_PROBABILITY: f64 = 0.01;

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Details about a node in the network, known from the network announcement.
pub struct NodeInfo {
    pub node_id: Pubkey,
    // The timestamp set by the owner for the node announcement.
    pub timestamp: u64,
    // Tentatively using 64 bits for features. May change the type later while developing.
    // rust-lightning uses a Vec<u8> here.
    pub features: u64,
    // The name of the node. This is a human-readable string that is meant to be used for labelling nodes in the UI.
    pub node_name: AnnouncedNodeName,
    // All the reachable addresses.
    pub addresses: Vec<MultiAddr>,
    // If the other party funding more than this amount, we will automatically accept the channel.
    pub auto_accept_min_ckb_funding_amount: u64,
    // UDT config info
    pub udt_cfg_infos: UdtCfgInfos,
}

impl NodeInfo {
    pub fn cursor(&self) -> Cursor {
        Cursor::new(
            self.timestamp,
            BroadcastMessageID::NodeAnnouncement(self.node_id),
        )
    }
}

impl From<NodeAnnouncement> for NodeInfo {
    fn from(value: NodeAnnouncement) -> Self {
        Self {
            node_id: value.node_id,
            timestamp: value.timestamp,
            features: value.features,
            node_name: value.node_name,
            addresses: value.addresses,
            auto_accept_min_ckb_funding_amount: value.auto_accept_min_ckb_funding_amount,
            udt_cfg_infos: value.udt_cfg_infos,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelInfo {
    pub channel_outpoint: OutPoint,
    // The timestamp in the block header of the block that includes the funding transaction of the channel.
    pub timestamp: u64,

    pub features: u64,
    pub node1: Pubkey,
    pub node2: Pubkey,
    // The total capacity of the channel.
    pub capacity: u128,
    // UDT script
    pub udt_type_script: Option<Script>,
    pub update_of_node1: Option<ChannelUpdateInfo>,
    pub update_of_node2: Option<ChannelUpdateInfo>,
}

impl ChannelInfo {
    pub fn cursor(&self) -> Cursor {
        Cursor::new(
            self.timestamp,
            BroadcastMessageID::ChannelAnnouncement(self.channel_outpoint.clone()),
        )
    }

    pub fn out_point(&self) -> &OutPoint {
        &self.channel_outpoint
    }

    pub fn capacity(&self) -> u128 {
        self.capacity
    }

    pub fn node1(&self) -> Pubkey {
        self.node1
    }

    pub fn node2(&self) -> Pubkey {
        self.node2
    }

    pub fn node1_peerid(&self) -> PeerId {
        self.node1.tentacle_peer_id()
    }

    pub fn node2_peerid(&self) -> PeerId {
        self.node2.tentacle_peer_id()
    }

    pub fn udt_type_script(&self) -> &Option<Script> {
        &self.udt_type_script
    }

    // Whether this channel is explicitly disabled in either direction.
    // TODO: we currently deem a channel as disabled if one direction is disabled.
    // Is it possible that one direction is disabled while the other is not?
    pub fn is_explicitly_disabled(&self) -> bool {
        match (&self.update_of_node2, &self.update_of_node1) {
            (Some(update1), _) if !update1.enabled => true,
            (_, Some(update2)) if !update2.enabled => true,
            _ => false,
        }
    }

    fn get_update_info_with(&self, node: Pubkey) -> Option<&ChannelUpdateInfo> {
        if self.node2() == node {
            self.update_of_node2.as_ref()
        } else if self.node1() == node {
            self.update_of_node1.as_ref()
        } else {
            None
        }
    }

    pub fn channel_last_update_time(&self) -> Option<u64> {
        self.update_of_node2
            .as_ref()
            .map(|n| n.timestamp)
            .max(self.update_of_node1.as_ref().map(|n| n.timestamp))
    }
}

impl From<(u64, ChannelAnnouncement)> for ChannelInfo {
    fn from((timestamp, channel_announcement): (u64, ChannelAnnouncement)) -> Self {
        Self {
            channel_outpoint: channel_announcement.channel_outpoint,
            timestamp,
            features: channel_announcement.features,
            node1: channel_announcement.node1_id,
            node2: channel_announcement.node2_id,
            capacity: channel_announcement.capacity,
            udt_type_script: channel_announcement.udt_type_script,
            update_of_node2: None,
            update_of_node1: None,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelUpdateInfo {
    // The timestamp is the time when the channel update was received by the node.
    pub timestamp: u64,
    /// Whether the channel can be currently used for payments (in this one direction).
    pub enabled: bool,
    /// The difference in htlc expiry values that you must have when routing through this channel (in milliseconds).
    pub tlc_expiry_delta: u64,
    /// The minimum value, which must be relayed to the next hop via the channel
    pub tlc_minimum_value: u128,
    pub fee_rate: u64,
}

impl From<ChannelUpdate> for ChannelUpdateInfo {
    fn from(update: ChannelUpdate) -> Self {
        Self::from(&update)
    }
}

impl From<&ChannelUpdate> for ChannelUpdateInfo {
    fn from(update: &ChannelUpdate) -> Self {
        Self {
            timestamp: update.timestamp,
            enabled: !update.is_disabled(),
            tlc_expiry_delta: update.tlc_expiry_delta,
            tlc_minimum_value: update.tlc_minimum_value,
            fee_rate: update.tlc_fee_proportional_millionths as u64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetworkGraph<S> {
    // The pubkey of the node that is running this instance of the network graph.
    source: Pubkey,
    // All the channels in the network.
    channels: HashMap<OutPoint, ChannelInfo>,
    // All the nodes in the network.
    nodes: HashMap<Pubkey, NodeInfo>,
    // The latest cursor we read from the GossipMessageStore. When we need to refresh our view of the
    // the network, we need to load all the messages starting from this cursor.
    latest_cursor: Cursor,
    // A store is both a persistent storage from which we can fetch all the network messages.
    // and a state store where we can store our local state (e.g. when a node has been unresponsive
    // for a few rounds, we need to mark it as failed, this information needs to be persisted).
    // The formal use of the store is defined as a GossipMessageStore, while the latter is defined
    // as a NetworkGraphStateStore.
    store: S,
    history: PaymentHistory<S>,
    // Whether to process announcement of private address
    announce_private_addr: bool,
}

#[derive(Error, Debug)]
pub enum PathFindError {
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
    S: NetworkGraphStateStore
        + ChannelActorStateStore
        + GossipMessageStore
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn new(store: S, source: Pubkey, announce_private_addr: bool) -> Self {
        let mut network_graph = Self {
            source,
            channels: HashMap::new(),
            nodes: HashMap::new(),
            latest_cursor: Cursor::default(),
            store: store.clone(),
            history: PaymentHistory::new(source, None, store),
            announce_private_addr,
        };
        network_graph.load_from_store();
        network_graph
    }

    pub fn get_latest_cursor(&self) -> &Cursor {
        &self.latest_cursor
    }

    fn update_lastest_cursor(&mut self, cursor: Cursor) {
        if cursor > self.latest_cursor {
            self.latest_cursor = cursor;
        }
    }

    // Update the network graph with the messages received from the network.
    // Returns true if the network graph has been updated.
    pub(crate) fn update_for_messages(
        &mut self,
        messages: Vec<BroadcastMessageWithTimestamp>,
    ) -> bool {
        if messages.is_empty() {
            return false;
        }
        debug!("Updating network graph with {} messages", messages.len());
        for message in messages {
            self.update_lastest_cursor(message.cursor());
            if message.chain_hash() != get_chain_hash() {
                tracing::warn!(
                    "Chain hash mismatch: having {:?}, expecting {:?}, full message {:?}",
                    message.chain_hash(),
                    get_chain_hash(),
                    &message
                );
                continue;
            }
            match message {
                BroadcastMessageWithTimestamp::ChannelAnnouncement(
                    timestamp,
                    channel_announcement,
                ) => {
                    self.process_channel_announcement(timestamp, channel_announcement);
                }
                BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => {
                    self.process_channel_update(channel_update);
                }
                BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                    self.process_node_announcement(node_announcement);
                }
            }
        }
        return true;
    }

    // Load all the broadcast messages starting from latest_cursor from the store.
    // Process them and set nodes and channels accordingly.
    pub(crate) fn load_from_store(&mut self) {
        loop {
            let messages = self.store.get_broadcast_messages(&self.latest_cursor, None);
            if messages.is_empty() {
                break;
            }
            self.update_for_messages(messages);
        }
    }

    // Completely reload from store. Because messages with larger timestamp
    // can be added to the store earlier than messages with smaller timestamp,
    // It is possible in regular load_from_store may skip some messages.
    // We use this method to reset the cursor and load all messages from start.
    #[cfg(test)]
    pub(crate) fn reload_from_store(&mut self) {
        self.reset();
        self.load_from_store();
    }

    fn load_channel_updates_from_store(&self, channel_info: &mut ChannelInfo) {
        let channel_update_of_node1 = self
            .store
            .get_latest_channel_update(&channel_info.channel_outpoint, true)
            .map(Into::into);
        let channel_update_of_node2 = self
            .store
            .get_latest_channel_update(&channel_info.channel_outpoint, false)
            .map(Into::into);
        channel_info.update_of_node1 = channel_update_of_node1;
        channel_info.update_of_node2 = channel_update_of_node2;
    }

    fn load_channel_info_mut(&mut self, channel_outpoint: &OutPoint) -> Option<&mut ChannelInfo> {
        if !self.channels.contains_key(channel_outpoint) {
            if let Some((timestamp, channel_announcement)) =
                self.store.get_latest_channel_announcement(channel_outpoint)
            {
                debug!(
                    "Loading channel announcement: timestamp {}, channel announcement {:?}",
                    timestamp, &channel_announcement
                );
                self.process_channel_announcement(timestamp, channel_announcement);
            }
        }
        self.channels.get_mut(channel_outpoint)
    }

    fn process_channel_announcement(
        &mut self,
        timestamp: u64,
        channel_announcement: ChannelAnnouncement,
    ) -> Option<Cursor> {
        match self.channels.get(&channel_announcement.channel_outpoint) {
            Some(_channel) => {
                trace!(
                    "Channel already exists, ignoring: {:?}",
                    &channel_announcement
                );
                return None;
            }
            None => {
                let cursor = Cursor::new(
                    timestamp,
                    BroadcastMessageID::ChannelAnnouncement(
                        channel_announcement.channel_outpoint.clone(),
                    ),
                );
                trace!(
                    "Inserting new channel announcement: {:?}",
                    &channel_announcement
                );
                let channel_info = ChannelInfo::from((timestamp, channel_announcement));
                // The history needs to know the mapping between nodes and channels.
                // So that when a node is marked as failed, the history can mark all the channels
                // associated with the node as failed. Here we tell the history about
                // the mapping between nodes and channels.
                self.history.add_node_channel_map(
                    channel_info.node1.clone(),
                    channel_info.out_point().clone(),
                );
                self.history.add_node_channel_map(
                    channel_info.node2.clone(),
                    channel_info.out_point().clone(),
                );
                self.channels
                    .insert(channel_info.channel_outpoint.clone(), channel_info);
                return Some(cursor);
            }
        }
    }

    fn process_channel_update(&mut self, channel_update: ChannelUpdate) -> Option<Cursor> {
        let channel_outpoint = &channel_update.channel_outpoint;
        // The channel update message may have smaller timestamp than channel announcement.
        // So it is possible that the channel announcement is not loaded into the graph yet,
        // when we receive the channel update message.
        let channel = self.load_channel_info_mut(channel_outpoint)?;
        let update_info = if channel_update.is_update_of_node_1() {
            &mut channel.update_of_node1
        } else {
            &mut channel.update_of_node2
        };

        match update_info {
            Some(old_update) if old_update.timestamp > channel_update.timestamp => {
                trace!(
                    "Ignoring outdated channel update {:?} for channel {:?}",
                    &channel_update,
                    &channel
                );
                return None;
            }
            _ => {
                let cursor = Cursor::new(
                    channel_update.timestamp,
                    BroadcastMessageID::ChannelUpdate(channel_update.channel_outpoint.clone()),
                );
                trace!(
                    "Saving new channel update to the graph: {:?}",
                    &channel_update
                );
                *update_info = Some(ChannelUpdateInfo::from(channel_update));
                return Some(cursor);
            }
        }
    }

    fn process_node_announcement(
        &mut self,
        mut node_announcement: NodeAnnouncement,
    ) -> Option<Cursor> {
        if !self.announce_private_addr {
            node_announcement.addresses.retain(|addr| {
                multiaddr_to_socketaddr(addr)
                    .map(|socket_addr| is_reachable(socket_addr.ip()))
                    .unwrap_or_default()
            });

            if node_announcement.addresses.is_empty() {
                return None;
            }
        }
        let node_info = NodeInfo::from(node_announcement);
        match self.nodes.get(&node_info.node_id) {
            Some(old_node) if old_node.timestamp > node_info.timestamp => {
                trace!(
                    "Ignoring outdated node announcement {:?} for node {:?}",
                    &node_info,
                    &old_node
                );
                return None;
            }
            _ => {
                let cursor = Cursor::new(
                    node_info.timestamp,
                    BroadcastMessageID::NodeAnnouncement(node_info.node_id),
                );
                trace!("Saving new node info to the graph: {:?}", &node_info);
                self.nodes.insert(node_info.node_id, node_info);
                return Some(cursor);
            }
        }
    }

    pub(crate) fn num_of_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn sample_n_peers_to_connect(&self, n: usize) -> HashMap<PeerId, Vec<MultiAddr>> {
        // TODO: we may need to shuffle the nodes before selecting the first n nodes,
        // to avoid some malicious nodes from being always selected.
        self.nodes
            .iter()
            .filter(|(k, _)| **k != self.source)
            .take(n)
            .map(|(k, v)| (k.tentacle_peer_id(), v.addresses.clone()))
            .collect()
    }

    pub fn nodes(&self) -> impl Iterator<Item = &NodeInfo> {
        self.nodes.values()
    }

    pub fn get_nodes_with_params(&self, limit: usize, after: Option<Cursor>) -> Vec<NodeInfo> {
        let cursor = after.unwrap_or_default();
        self.store
            .get_broadcast_messages_iter(&cursor)
            .into_iter()
            .filter_map(|message| match message {
                BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
                    Some(NodeInfo::from(node_announcement))
                }
                _ => None,
            })
            .take(limit)
            .collect()
    }

    pub fn get_node(&self, node_id: &Pubkey) -> Option<&NodeInfo> {
        self.nodes.get(node_id)
    }

    pub fn channels(&self) -> impl Iterator<Item = &ChannelInfo> {
        self.channels.values()
    }

    pub fn get_channel(&self, outpoint: &OutPoint) -> Option<&ChannelInfo> {
        self.channels.get(outpoint)
    }

    pub fn get_channels_with_params(
        &self,
        limit: usize,
        after: Option<Cursor>,
    ) -> Vec<ChannelInfo> {
        let cursor = after.unwrap_or_default();
        self.store
            .get_broadcast_messages_iter(&cursor)
            .into_iter()
            .filter_map(|message| match message {
                BroadcastMessageWithTimestamp::ChannelAnnouncement(
                    timestamp,
                    channel_announcement,
                ) => {
                    let mut channel_info = ChannelInfo::from((timestamp, channel_announcement));
                    self.load_channel_updates_from_store(&mut channel_info);
                    if channel_info.is_explicitly_disabled() {
                        None
                    } else {
                        Some(channel_info)
                    }
                }
                _ => None,
            })
            .take(limit)
            .collect()
    }

    pub fn get_channels_by_peer(&self, node_id: Pubkey) -> impl Iterator<Item = &ChannelInfo> {
        self.channels
            .values()
            .filter(move |channel| channel.node1() == node_id || channel.node2() == node_id)
    }

    pub fn get_mut_channels_by_peer(
        &mut self,
        node_id: Pubkey,
    ) -> impl Iterator<Item = &mut ChannelInfo> {
        self.channels
            .values_mut()
            .filter(move |channel| channel.node1() == node_id || channel.node2() == node_id)
    }

    pub fn get_node_inbounds(
        &self,
        node_id: Pubkey,
    ) -> impl Iterator<Item = (Pubkey, Pubkey, &ChannelInfo, &ChannelUpdateInfo)> {
        let mut channels: Vec<_> = self
            .channels
            .values()
            .filter_map(move |channel| {
                if let Some(info) = channel.update_of_node2.as_ref() {
                    if info.enabled && channel.node2() == node_id {
                        return Some((channel.node1(), channel.node2(), channel, info));
                    }
                }

                if let Some(info) = channel.update_of_node1.as_ref() {
                    if info.enabled && channel.node1() == node_id {
                        return Some((channel.node2(), channel.node1(), channel, info));
                    }
                }
                None
            })
            .collect();

        // Iterating over HashMap's values is not guaranteed to be in order,
        // which may introduce randomness in the path finding.
        // the weight algorithm in find_path does not considering capacity,
        // so the channel with larger capacity maybe have the same weight with the channel with smaller capacity
        // so we sort by capacity reverse order to make sure we try channel with larger capacity firstly
        channels.sort_by(|(_, _, a, _), (_, _, b, _)| {
            b.capacity().cmp(&a.capacity()).then(
                b.channel_last_update_time()
                    .cmp(&a.channel_last_update_time()),
            )
        });
        channels.into_iter()
    }

    pub fn get_source_pubkey(&self) -> Pubkey {
        self.source
    }

    pub(crate) fn mark_channel_failed(&mut self, channel_outpoint: &OutPoint) {
        if let Some(channel) = self.channels.get_mut(channel_outpoint) {
            if let Some(info) = channel.update_of_node2.as_mut() {
                info.enabled = false;
            }
            if let Some(info) = channel.update_of_node1.as_mut() {
                info.enabled = false;
            }
        }
    }

    pub(crate) fn mark_node_failed(&mut self, node_id: Pubkey) {
        for channel in self.get_mut_channels_by_peer(node_id) {
            if channel.node1() == node_id {
                if let Some(info) = channel.update_of_node2.as_mut() {
                    info.enabled = false;
                }
            } else {
                if let Some(info) = channel.update_of_node1.as_mut() {
                    info.enabled = false;
                }
            }
        }
    }

    pub(crate) fn record_payment_success(&mut self, mut payment_session: PaymentSession) {
        let session_route = &payment_session.route.nodes;
        let mut result = InternalResult::default();
        result.succeed_range_pairs(session_route, 0, session_route.len() - 1);
        self.history.apply_internal_result(result);
        payment_session.set_success_status();
        self.store.insert_payment_session(payment_session);
    }

    pub(crate) fn record_payment_fail(
        &mut self,
        payment_session: &PaymentSession,
        tlc_err: TlcErr,
    ) -> bool {
        let mut internal_result = InternalResult::default();
        let nodes = &payment_session.route.nodes;
        let need_to_retry = internal_result.record_payment_fail(nodes, tlc_err);
        self.history.apply_internal_result(internal_result);
        return need_to_retry && payment_session.can_retry();
    }

    #[cfg(test)]
    pub fn reset(&mut self) {
        self.latest_cursor = Cursor::default();
        self.channels.clear();
        self.nodes.clear();
        self.history.reset();
    }

    #[cfg(test)]
    pub fn set_source(&mut self, source: Pubkey) {
        self.source = source;
    }

    /// Returns a list of `PaymentHopData` for all nodes in the route,
    /// including the origin and the target node.
    pub fn build_route(
        &self,
        payment_data: SendPaymentData,
    ) -> Result<Vec<PaymentHopData>, PathFindError> {
        let source = self.get_source_pubkey();
        let target = payment_data.target_pubkey;
        let amount = payment_data.amount;
        let preimage = payment_data.preimage;
        let payment_hash = payment_data.payment_hash;
        let udt_type_script = payment_data.udt_type_script;
        let final_tlc_expiry_delta = payment_data.final_tlc_expiry_delta;
        let invoice = payment_data
            .invoice
            .map(|x| x.parse::<CkbInvoice>().expect("parse CKB invoice"));
        let hash_algorithm = invoice
            .as_ref()
            .and_then(|x| x.hash_algorithm().copied())
            .unwrap_or_default();

        info!(
            "build_route source: {:?} target: {:?} amount: {:?}, payment_hash: {:?}",
            source, target, amount, payment_hash
        );

        let allow_self_payment = payment_data.allow_self_payment;
        if source == target && !allow_self_payment {
            return Err(PathFindError::PathFind(
                "allow_self_payment is not enable, can not pay to self".to_string(),
            ));
        }

        let route = self.find_path(
            source,
            target,
            amount,
            payment_data.max_fee_amount,
            udt_type_script,
            final_tlc_expiry_delta,
            payment_data.tlc_expiry_limit,
            allow_self_payment,
            payment_data.hop_hints,
        )?;
        assert!(!route.is_empty());

        let mut current_amount = amount;
        let current_time = now_timestamp_as_millis_u64();
        let mut current_expiry = current_time + final_tlc_expiry_delta;
        let mut hops_data = vec![];

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
            let (fee, expiry_delta) = if is_last {
                (0, 0)
            } else {
                let channel_info = self
                    .get_channel(&route[i].channel_outpoint)
                    .expect("channel not found");
                let channel_update = channel_info
                    .get_update_info_with(route[i].target)
                    .expect("channel_update not found");
                let fee_rate = channel_update.fee_rate;
                let fee =
                    calculate_tlc_forward_fee(current_amount, fee_rate as u128).expect("fee is ok");
                let expiry = channel_update.tlc_expiry_delta;
                (fee, expiry)
            };

            let funding_tx_hash = if let Some(next_channel_outpoint) = next_channel_outpoint {
                next_channel_outpoint.tx_hash().into()
            } else {
                Hash256::default()
            };
            // make sure the final hop's amount is the same as the payment amount
            // the last hop will check the amount from TLC and the amount from the onion packet

            hops_data.push(PaymentHopData {
                amount: current_amount,
                next_hop,
                hash_algorithm: hash_algorithm,
                expiry: current_expiry,
                funding_tx_hash,
                payment_preimage: if is_last { preimage } else { None },
            });
            current_expiry += expiry_delta;
            current_amount += fee;
        }
        // Add the first hop as the instruction for the current node, so the logic for send HTLC can be reused.
        hops_data.push(PaymentHopData {
            amount: current_amount,
            next_hop: Some(route[0].target),
            hash_algorithm: hash_algorithm,
            expiry: current_expiry,
            funding_tx_hash: route[0].channel_outpoint.tx_hash().into(),
            payment_preimage: None,
        });
        hops_data.reverse();
        assert_eq!(hops_data.len(), route.len() + 1);
        assert_eq!(hops_data[route.len()].amount, amount);
        // assert there is no duplicate node in the route
        assert_eq!(
            hops_data
                .iter()
                .filter_map(|x| x.next_hop)
                .collect::<HashSet<_>>()
                .len(),
            route.len()
        );

        Ok(hops_data)
    }

    // the algorithm works from target-to-source to find the shortest path
    pub fn find_path(
        &self,
        source: Pubkey,
        target: Pubkey,
        amount: u128,
        max_fee_amount: Option<u128>,
        udt_type_script: Option<Script>,
        final_tlc_expiry_delta: u64,
        tlc_expiry_limit: u64,
        allow_self: bool,
        hop_hints: Vec<HopHint>,
    ) -> Result<Vec<PathEdge>, PathFindError> {
        let started_time = std::time::Instant::now();
        let nodes_len = self.nodes.len();
        let route_to_self = source == target;

        let mut result = vec![];
        let mut nodes_visited = 0;
        let mut edges_expanded = 0;
        let mut nodes_heap = NodeHeap::new(nodes_len);
        let mut distances = HashMap::<Pubkey, NodeHeapElement>::new();

        if amount == 0 {
            return Err(PathFindError::Amount(
                "amount must be greater than 0".to_string(),
            ));
        }

        if source == target && !allow_self {
            return Err(PathFindError::PathFind(
                "allow_self_payment is not enable, can not pay self".to_string(),
            ));
        }

        let hop_hint_map: HashMap<(Pubkey, bool), OutPoint> = hop_hints
            .into_iter()
            .map(|hint| {
                (
                    (hint.pubkey, hint.inbound),
                    OutPoint::new(hint.channel_funding_tx.into(), 0),
                )
            })
            .collect::<HashMap<_, _>>();

        let mut target = target;
        let mut current_expiry = final_tlc_expiry_delta;
        let mut last_edge = None;

        if route_to_self {
            let (new_target, expiry, edge) =
                self.adjust_target_for_route_self(&hop_hint_map, amount, source, target)?;
            target = new_target;
            last_edge = edge;
            current_expiry += expiry;
        }
        assert_ne!(source, target);
        // initialize the target node
        nodes_heap.push(NodeHeapElement {
            node_id: target,
            weight: 0,
            distance: 0,
            amount_received: amount,
            fee_charged: 0,
            probability: 1.0,
            next_hop: None,
            incoming_tlc_expiry: current_expiry,
        });

        while let Some(cur_hop) = nodes_heap.pop() {
            nodes_visited += 1;

            for (from, to, channel_info, channel_update) in self.get_node_inbounds(cur_hop.node_id)
            {
                assert_eq!(to, cur_hop.node_id);
                if &udt_type_script != channel_info.udt_type_script() {
                    continue;
                }

                if let Some(channel) = hop_hint_map.get(&(from, false)) {
                    if channel != channel_info.out_point() {
                        continue;
                    }
                }
                if let Some(channel) = hop_hint_map.get(&(to, true)) {
                    if channel != channel_info.out_point() {
                        continue;
                    }
                }

                if let Some((_node, channel)) = &last_edge {
                    if channel == channel_info.out_point() {
                        continue;
                    }
                }

                edges_expanded += 1;

                let next_hop_received_amount = cur_hop.amount_received;
                if next_hop_received_amount > channel_info.capacity() {
                    debug!(
                        "next_hop_received_amount: {} > channel_info.capacity {}",
                        next_hop_received_amount,
                        channel_info.capacity()
                    );
                    continue;
                }

                let fee = if from == source {
                    0
                } else {
                    calculate_tlc_forward_fee(
                        next_hop_received_amount,
                        channel_update.fee_rate as u128,
                    )
                    .map_err(|err| {
                        PathFindError::PathFind(format!(
                            "calculate_tlc_forward_fee error: {:?}",
                            err
                        ))
                    })?
                };
                let amount_to_send = next_hop_received_amount + fee;

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
                // if `tlc_maximum_value` equals 0, it means there is no limit
                if amount_to_send > channel_info.capacity() {
                    continue;
                }
                if amount_to_send < channel_update.tlc_minimum_value {
                    continue;
                }

                // if this is a direct channel, try to load the channel actor state for balance
                if from == self.source || to == self.source {
                    if let Some(state) = self
                        .store
                        .get_channel_state_by_outpoint(&channel_info.out_point())
                    {
                        let balance = if from == self.source {
                            state.to_local_amount
                        } else {
                            state.to_remote_amount
                        };
                        if amount_to_send > balance {
                            continue;
                        }
                    }
                }

                let expiry_delta = if from == source {
                    0
                } else {
                    channel_update.tlc_expiry_delta
                };

                let incoming_htlc_expiry = cur_hop.incoming_tlc_expiry + expiry_delta;
                if incoming_htlc_expiry > tlc_expiry_limit {
                    continue;
                }

                let probability = cur_hop.probability
                    * self.history.eval_probability(
                        from,
                        to,
                        &channel_info.out_point(),
                        amount_to_send,
                        channel_info.capacity(),
                    );

                debug!(
                    "probability: {} for channel_outpoint: {:?} from: {:?} => to: {:?}",
                    probability,
                    channel_info.out_point(),
                    from,
                    to
                );
                if probability < DEFAULT_MIN_PROBABILITY {
                    debug!("probability is too low: {:?}", probability);
                    continue;
                }
                let agg_weight =
                    self.edge_weight(amount_to_send, fee, channel_update.tlc_expiry_delta);
                let weight = cur_hop.weight + agg_weight;
                let distance = self.calculate_distance_based_probability(probability, weight);

                if let Some(node) = distances.get(&from) {
                    if distance >= node.distance {
                        continue;
                    }
                }
                let node = NodeHeapElement {
                    node_id: from,
                    weight,
                    distance,
                    amount_received: amount_to_send,
                    incoming_tlc_expiry: incoming_htlc_expiry,
                    fee_charged: fee,
                    probability,
                    next_hop: Some((cur_hop.node_id, channel_info.out_point().clone())),
                };
                distances.insert(node.node_id, node.clone());
                nodes_heap.push_or_fix(node);
            }
        }

        let mut current = source;
        while let Some(elem) = distances.remove(&current) {
            let (next_pubkey, next_out_point) = elem.next_hop.expect("next_hop is none");
            result.push(PathEdge {
                target: next_pubkey,
                channel_outpoint: next_out_point,
            });
            current = next_pubkey;
            if current == target {
                break;
            }
        }

        info!(
            "get_route: nodes visited: {}, edges expanded: {}, time: {:?} \nresult: {:?}",
            nodes_visited,
            edges_expanded,
            started_time.elapsed(),
            result
        );
        if result.is_empty() || current != target {
            return Err(PathFindError::PathFind("no path found".to_string()));
        }
        if let Some((node, channel)) = last_edge {
            result.push(PathEdge {
                target: node,
                channel_outpoint: channel.clone(),
            })
        }

        Ok(result)
    }

    fn adjust_target_for_route_self(
        &self,
        hop_hint_map: &HashMap<(Pubkey, bool), OutPoint>,
        amount: u128,
        source: Pubkey,
        target: Pubkey,
    ) -> Result<(Pubkey, u64, Option<(Pubkey, OutPoint)>), PathFindError> {
        let direct_channels: Vec<(Pubkey, Pubkey, &ChannelInfo, &ChannelUpdateInfo)> = self
            .get_node_inbounds(source)
            .filter(|(_, _, channel_info, _)| {
                if let Some(channel) = hop_hint_map.get(&(source, true)) {
                    // if there is a hop hint for node -> source,
                    // try to use this channel as the last candidate hop for route self
                    // and we event don't check the direct balance of channel,
                    // hop hint's priority is higher than direct balance
                    return channel == channel_info.out_point();
                }
                if let Some(channel) = hop_hint_map.get(&(source, false)) {
                    // if there is a hop hint for source -> node,
                    // then we can not set this node as the last candidate hop for route self
                    // so skip this channel
                    if channel == channel_info.out_point() {
                        return false;
                    }
                }
                if let Some(state) = self
                    .store
                    .get_channel_state_by_outpoint(&channel_info.out_point())
                {
                    let balance = state.to_remote_amount;
                    return balance >= amount;
                }
                // normal code path will not reach here, we must can get balance for direct channels
                // anyway, check the capacity here for safety
                return channel_info.capacity() >= amount;
            })
            .collect();

        // a proper hop hint for route self will limit the direct_channels to only one
        // if there are multiple channels, we will randomly select a channel from the source node for route to self
        // so that the following part of algorithm will always trying to find a path without cycle
        if let Some(&(from, _, channel_info, channel_update)) =
            direct_channels.choose(&mut thread_rng())
        {
            let last_edge = Some((source, channel_info.out_point().clone()));
            let current_expiry = channel_update.tlc_expiry_delta;
            assert_ne!(target, from);
            let target = from;
            Ok((target, current_expiry, last_edge))
        } else {
            return Err(PathFindError::PathFind(
                "no direct channel found for source node".to_string(),
            ));
        }
    }

    fn edge_weight(&self, amount: u128, fee: u128, htlc_expiry_delta: u64) -> u128 {
        let risk_factor: u128 = 15;
        let time_lock_penalty = amount * htlc_expiry_delta as u128 * (risk_factor / 1000000000);
        fee + time_lock_penalty
    }

    fn calculate_distance_based_probability(&self, probability: f64, weight: u128) -> u128 {
        assert!(probability > 0.0);
        // FIXME: set this to configurable parameters
        let weight = weight as f64;
        let time_pref = 0.5_f64;
        let default_attemp_cost = 0.1_f64;
        let penalty = default_attemp_cost * (1.0 / (0.5 - time_pref / 2.0) - 1.0);
        weight as u128 + (penalty / probability) as u128
    }
}

pub trait NetworkGraphStateStore {
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession>;
    fn insert_payment_session(&self, session: PaymentSession);
    fn insert_payment_history_result(
        &mut self,
        channel_outpoint: OutPoint,
        direction: Direction,
        result: TimedResult,
    );
    fn get_payment_history_results(&self) -> Vec<(OutPoint, Direction, TimedResult)>;
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

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    pub pubkey: Pubkey,
    pub amount: u128,
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

// The router is a list of nodes that the payment will go through.
// We store in the payment session and then will use it to track the payment history.
// The router is a list of nodes that the payment will go through.
// For example:
//    A(amount, channel) -> B -> C -> D means A will send `amount` with `channel` to B.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    pub nodes: Vec<SessionRouteNode>,
}

impl SessionRoute {
    // Create a new route from the source to the target with the given payment hops.
    // The payment hops are the hops that the payment will go through.
    // for a payment route A -> B -> C -> D
    // the `payment_hops` is [B, C, D], which is a convinent way for onion routing.
    // here we need to create a session route with source, which is A -> B -> C -> D
    pub fn new(source: Pubkey, target: Pubkey, payment_hops: &[PaymentHopData]) -> Self {
        let nodes = std::iter::once(source)
            .chain(
                payment_hops
                    .iter()
                    .map(|hop| hop.next_hop.clone().unwrap_or(target)),
            )
            .zip(payment_hops)
            .map(|(pubkey, hop)| SessionRouteNode {
                pubkey,
                channel_outpoint: OutPoint::new(
                    if hop.funding_tx_hash != Hash256::default() {
                        hop.funding_tx_hash.into()
                    } else {
                        Hash256::default().into()
                    },
                    0,
                ),
                amount: hop.amount,
            })
            .collect();
        Self { nodes }
    }

    pub fn fee(&self) -> u128 {
        let first_amount = self.nodes.first().map_or(0, |s| s.amount);
        let last_amount = self.nodes.last().map_or(0, |s| s.amount);
        assert!(first_amount >= last_amount);
        first_amount - last_amount
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentSession {
    pub request: SendPaymentData,
    pub retried_times: u32,
    pub last_error: Option<String>,
    pub try_limit: u32,
    pub status: PaymentSessionStatus,
    pub created_at: u64,
    pub last_updated_at: u64,
    pub route: SessionRoute,
    // Session key for onion packet. Save it for decoding the error packet.
    pub session_key: [u8; 32],
}

impl PaymentSession {
    pub fn new(request: SendPaymentData, try_limit: u32) -> Self {
        let now = now_timestamp_as_millis_u64();
        Self {
            request,
            retried_times: 0,
            last_error: None,
            try_limit,
            status: PaymentSessionStatus::Created,
            created_at: now,
            last_updated_at: now,
            route: SessionRoute::default(),
            session_key: Default::default(),
        }
    }

    pub fn payment_hash(&self) -> Hash256 {
        self.request.payment_hash
    }

    fn set_status(&mut self, status: PaymentSessionStatus) {
        self.status = status;
        self.last_updated_at = now_timestamp_as_millis_u64();
    }

    pub fn set_inflight_status(&mut self) {
        self.set_status(PaymentSessionStatus::Inflight);
    }

    pub fn set_success_status(&mut self) {
        self.set_status(PaymentSessionStatus::Success);
        self.last_error = None;
    }

    pub fn set_failed_status(&mut self, error: &str) {
        self.set_status(PaymentSessionStatus::Failed);
        self.last_error = Some(error.to_string());
    }

    pub fn can_retry(&self) -> bool {
        self.retried_times < self.try_limit
    }

    pub fn fee(&self) -> u128 {
        self.route.fee()
    }

    pub fn hops_public_keys(&self) -> Vec<Pubkey> {
        // Skip the first node, which is the sender.
        self.route.nodes.iter().skip(1).map(|x| x.pubkey).collect()
    }
}

impl From<PaymentSession> for SendPaymentResponse {
    fn from(session: PaymentSession) -> Self {
        let fee = session.fee();
        Self {
            payment_hash: session.request.payment_hash,
            status: session.status,
            failed_error: session.last_error,
            created_at: session.created_at,
            last_updated_at: session.last_updated_at,
            fee,
        }
    }
}
