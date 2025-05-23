use super::channel::{ChannelActorState, ChannelActorStateStore, ChannelTlcInfo};
use super::config::{AnnouncedNodeName, CKB_SHANNONS};
use super::gossip::GossipMessageStore;
use super::history::{Direction, InternalResult, PaymentHistory, TimedResult};
use super::network::{
    get_chain_hash, BuildRouterCommand, HopHint, SendPaymentData, SendPaymentResponse,
};
use super::path::NodeHeap;
use super::types::{
    BroadcastMessageID, BroadcastMessageWithTimestamp, ChannelAnnouncement, ChannelUpdate, Hash256,
    NodeAnnouncement,
};
use super::types::{Cursor, Pubkey, TlcErr};
use crate::ckb::config::UdtCfgInfos;
use crate::fiber::config::DEFAULT_TLC_EXPIRY_DELTA;
use crate::fiber::fee::calculate_tlc_forward_fee;
use crate::fiber::path::NodeHeapElement;
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::{U128Hex, U64Hex};
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

    pub fn channel_last_update_time(&self) -> Option<u64> {
        self.update_of_node2
            .as_ref()
            .map(|n| n.timestamp)
            .max(self.update_of_node1.as_ref().map(|n| n.timestamp))
    }

    #[cfg(any(test, feature = "bench"))]
    pub fn get_channel_update_of(&self, node: Pubkey) -> Option<&ChannelUpdateInfo> {
        if self.node1() == node {
            self.update_of_node1.as_ref()
        } else if self.node2() == node {
            self.update_of_node2.as_ref()
        } else {
            None
        }
    }
}

impl TryFrom<&ChannelActorState> for ChannelInfo {
    type Error = String;

    fn try_from(state: &ChannelActorState) -> Result<Self, Self::Error> {
        if !state.is_ready() {
            return Err("Channel is not ready".to_string());
        }

        let timestamp = state.must_get_funding_transaction_timestamp();
        let channel_outpoint = state.must_get_funding_transaction_outpoint();
        let capacity = state.get_liquid_capacity();
        let udt_type_script = state.funding_udt_type_script.clone();

        let (node1, node2, update_of_node1, update_of_node2) = if state.local_is_node1() {
            (
                state.local_pubkey,
                state.remote_pubkey,
                Some(state.get_local_channel_update_info()),
                state.get_remote_channel_update_info(),
            )
        } else {
            (
                state.remote_pubkey,
                state.local_pubkey,
                state.get_remote_channel_update_info(),
                Some(state.get_local_channel_update_info()),
            )
        };
        Ok(Self {
            channel_outpoint,
            timestamp,
            features: 0,
            node1,
            node2,
            capacity,
            udt_type_script,
            update_of_node1,
            update_of_node2,
        })
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

/// The channel update info with a single direction of channel
#[serde_as]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChannelUpdateInfo {
    /// The timestamp is the time when the channel update was received by the node.
    #[serde_as(as = "U64Hex")]
    pub timestamp: u64,
    /// Whether the channel can be currently used for payments (in this one direction).
    pub enabled: bool,
    /// The exact amount of balance that we can send to the other party via the channel.
    #[serde_as(as = "Option<U128Hex>")]
    pub outbound_liquidity: Option<u128>,
    /// The difference in htlc expiry values that you must have when routing through this channel (in milliseconds).
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
    /// The minimum value, which must be relayed to the next hop via the channel
    #[serde_as(as = "U128Hex")]
    pub tlc_minimum_value: u128,
    /// The forwarding fee rate for the channel.
    #[serde_as(as = "U64Hex")]
    pub fee_rate: u64,
}

impl From<&ChannelTlcInfo> for ChannelUpdateInfo {
    fn from(info: &ChannelTlcInfo) -> Self {
        Self {
            timestamp: info.timestamp,
            enabled: info.enabled,
            outbound_liquidity: None,
            tlc_expiry_delta: info.tlc_expiry_delta,
            tlc_minimum_value: info.tlc_minimum_value,
            fee_rate: info.tlc_fee_proportional_millionths as u64,
        }
    }
}

impl From<ChannelTlcInfo> for ChannelUpdateInfo {
    fn from(info: ChannelTlcInfo) -> Self {
        Self::from(&info)
    }
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
            outbound_liquidity: None,
            tlc_expiry_delta: update.tlc_expiry_delta,
            tlc_minimum_value: update.tlc_minimum_value,
            fee_rate: update.tlc_fee_proportional_millionths as u64,
        }
    }
}

/// Update for our own channel has been made. We can use those events to update our graph.
/// The events only contain the information that is relevant for our own channels.
/// Other channel update events should be processed by gossip messages.
#[derive(Debug)]
pub enum OwnedChannelUpdateEvent {
    /// The channel is back online and can be used for routing payments.
    /// This normally means the peer is now reachable.
    Up(ChannelInfo),
    /// The channel is down and should not be used for routing payments.
    /// This normally means the peer is not reachable.
    Down(OutPoint),
    /// One direction of the channel is updated (e.g. new balance, new fee rate).
    Updated(OutPoint, Pubkey, ChannelUpdateInfo),
}

#[derive(Clone, Debug)]
pub struct NetworkGraph<S> {
    // Whether to always process gossip messages for our own channels.
    // See comments in should_process_gossip_message_for_channel for why we need this.
    // TLDR: Most of the tests do not need this. Only tests in src/fiber/tests/graph.rs need this.
    // We will only set this to true for tests in src/fiber/tests/graph.rs.
    #[cfg(any(test, feature = "bench"))]
    pub always_process_gossip_message: bool,
    // The pubkey of the node that is running this instance of the network graph.
    source: Pubkey,
    // All the channels in the network.
    pub(crate) channels: HashMap<OutPoint, ChannelInfo>,
    // All the nodes in the network.
    nodes: HashMap<Pubkey, NodeInfo>,

    // Channel pending stats map
    channel_pending_stats: HashMap<OutPoint, usize>,

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

/// A router hop information for a payment, a paymenter router is an array of RouterHop,
/// a router hop generally implies hop `target` will receive `amount_received` with `channel_outpoint` of channel.
/// Improper hop hint may make payment fail, for example the specified channel do not have enough capacity.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouterHop {
    /// The node that is sending the TLC to the next node.
    pub(crate) target: Pubkey,
    /// The channel of this hop used to receive TLC
    #[serde_as(as = "EntityHex")]
    pub(crate) channel_outpoint: OutPoint,
    /// The amount that the source node will transfer to the target node.
    /// We have already added up all the fees along the path, so this amount can be used directly for the TLC.
    #[serde_as(as = "U128Hex")]
    pub(crate) amount_received: u128,
    /// The expiry for the TLC that the source node sends to the target node.
    /// We have already added up all the expiry deltas along the path,
    /// the only thing missing is current time. So the expiry is the current time plus the expiry delta.
    #[serde_as(as = "U64Hex")]
    pub(crate) incoming_tlc_expiry: u64,
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
            #[cfg(any(test, feature = "bench"))]
            always_process_gossip_message: false,
            source,
            channels: HashMap::new(),
            channel_pending_stats: HashMap::new(),
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

    fn update_latest_cursor(&mut self, cursor: Cursor) {
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
        debug!("Latest cursor: {:?}", self.latest_cursor);
        for message in messages {
            debug!("Processing message: {:?}", &message);
            self.update_latest_cursor(message.cursor());
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

    // Process the events that are relevant for our own channels, and update the graph accordingly.
    pub(crate) fn process_owned_channel_update_event(&mut self, event: OwnedChannelUpdateEvent) {
        match event {
            OwnedChannelUpdateEvent::Up(channel_info) => {
                // Normally the channel_info passed here is the latest channel info,
                // so we can just overwrite the old channel info.
                self.channels
                    .insert(channel_info.channel_outpoint.clone(), channel_info);
            }
            OwnedChannelUpdateEvent::Down(channel_outpoint) => {
                self.channels.remove(&channel_outpoint);
            }
            OwnedChannelUpdateEvent::Updated(channel_outpoint, node, channel_update) => {
                if let Some(channel) = self.channels.get_mut(&channel_outpoint) {
                    if node == channel.node2() {
                        channel.update_of_node2 = Some(channel_update);
                    }
                    if node == channel.node1() {
                        channel.update_of_node1 = Some(channel_update);
                    }
                }
            }
        }
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

    // We don't need to process our own channel announcement with gossip messages.
    // They are processed by passing OwnedChannelUpdateEvents to the graph.
    // These are real-time events with more detailed information (e.g. balance).
    // We don't want to overwrite their detailed information here.
    // But tests in src/fiber/tests/graph.rs need to process gossip messages
    // to update the network graph. Many of the tests are messages from the graph.source.
    // If we ignore these messages, the graph won't be updated. And many tests will fail.
    fn should_process_gossip_message_for_nodes(&self, node1: &Pubkey, node2: &Pubkey) -> bool {
        #[cfg(any(test, feature = "bench"))]
        if self.always_process_gossip_message {
            return true;
        }
        !(&self.source == node1 || &self.source == node2)
    }

    fn process_channel_announcement(
        &mut self,
        timestamp: u64,
        channel_announcement: ChannelAnnouncement,
    ) -> Option<Cursor> {
        if !self.should_process_gossip_message_for_nodes(
            &channel_announcement.node1_id,
            &channel_announcement.node2_id,
        ) {
            return None;
        }

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
                self.history
                    .add_node_channel_map(channel_info.node1, channel_info.out_point().clone());
                self.history
                    .add_node_channel_map(channel_info.node2, channel_info.out_point().clone());
                self.channels
                    .insert(channel_info.channel_outpoint.clone(), channel_info);
                return Some(cursor);
            }
        }
    }

    fn process_channel_update(&mut self, channel_update: ChannelUpdate) -> Option<Cursor> {
        match self.get_channel(&channel_update.channel_outpoint) {
            Some(channel)
                if !self
                    .should_process_gossip_message_for_nodes(&channel.node1, &channel.node2) =>
            {
                return None;
            }
            _ => {}
        }
        let channel = self.load_channel_info_mut(&channel_update.channel_outpoint)?;
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
                *update_info = Some(ChannelUpdateInfo::from(&channel_update));
                self.history.process_channel_update(&channel_update);
                return Some(cursor);
            }
        }
    }

    pub(crate) fn process_node_announcement(
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

    fn get_outbound_channel_info_and_update(
        &self,
        outpoint: &OutPoint,
        from: Pubkey,
    ) -> (Option<&ChannelInfo>, Option<&ChannelUpdateInfo>) {
        let channel_info = self.get_channel(outpoint);
        if let Some(channel_info) = channel_info {
            if channel_info.node1() == from {
                (Some(channel_info), channel_info.update_of_node1.as_ref())
            } else if channel_info.node2() == from {
                (Some(channel_info), channel_info.update_of_node2.as_ref())
            } else {
                (None, None)
            }
        } else {
            (None, None)
        }
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
                    Some(channel_info)
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
                match channel.update_of_node1.as_ref() {
                    Some(info) if node_id == channel.node2() && info.enabled => {
                        return Some((channel.node1(), channel.node2(), channel, info));
                    }
                    _ => {}
                }
                match channel.update_of_node2.as_ref() {
                    Some(info) if node_id == channel.node1() && info.enabled => {
                        return Some((channel.node2(), channel.node1(), channel, info));
                    }
                    _ => {}
                }
                None
            })
            .collect();

        // Iterating over HashMap's values is not guaranteed to be in order,
        // which may introduce randomness in the path finding.
        // We will first sort the channels by outbound_liquidity, then capacity, and at last update time.
        // This is because the weight algorithm in find_path does not considering outbound_liquidity and capacity,
        // so the channel with larger outbound_liquidity/capacity maybe have the same weight with the channel
        // with smaller outbound_liquidity/capacity, even though the former have better chance to success.
        channels.sort_by(
            |(_, _, a_channel_info, a_channel_update_info),
             (_, _, b_channel_info, b_channel_update_info)| {
                b_channel_update_info
                    .outbound_liquidity
                    .cmp(&a_channel_update_info.outbound_liquidity)
                    .then(
                        b_channel_info
                            .capacity()
                            .cmp(&a_channel_info.capacity())
                            .then(
                                b_channel_info
                                    .channel_last_update_time()
                                    .cmp(&a_channel_info.channel_last_update_time()),
                            ),
                    )
            },
        );
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
            } else if let Some(info) = channel.update_of_node1.as_mut() {
                info.enabled = false;
            }
        }
    }

    pub(crate) fn track_payment_router(&mut self, payment_session: &PaymentSession) {
        for channel_outpoint in payment_session.channel_outpoints() {
            self.channel_pending_stats
                .entry(channel_outpoint.clone())
                .and_modify(|e| *e += 1)
                .or_insert(1);
        }
    }

    pub(crate) fn untrack_payment_router(&mut self, payment_session: &PaymentSession) {
        for channel_outpoint in payment_session.channel_outpoints() {
            self.channel_pending_stats
                .entry(channel_outpoint.clone())
                .and_modify(|e| *e -= 1)
                .or_insert(0);
        }
    }

    pub(crate) fn record_payment_success(&mut self, mut payment_session: PaymentSession) {
        self.untrack_payment_router(&payment_session);
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
        first_hop_error: bool,
    ) -> bool {
        if !first_hop_error {
            self.untrack_payment_router(payment_session);
        }
        let mut internal_result = InternalResult::default();
        let nodes = &payment_session.route.nodes;
        let need_to_retry = internal_result.record_payment_fail(nodes, tlc_err);
        self.history.apply_internal_result(internal_result);
        return need_to_retry
            && !payment_session.is_send_payment_with_router()
            && payment_session.can_retry();
    }

    #[cfg(any(test, feature = "bench"))]
    pub fn reset(&mut self) {
        self.latest_cursor = Cursor::default();
        self.channels.clear();
        self.nodes.clear();
        self.history.reset();
    }

    #[cfg(any(test, feature = "bench"))]
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
        let final_tlc_expiry_delta = payment_data.final_tlc_expiry_delta;
        let allow_self_payment = payment_data.allow_self_payment;

        if source == target && !allow_self_payment {
            return Err(PathFindError::PathFind(
                "allow_self_payment is not enable, can not pay to self".to_string(),
            ));
        }

        let route = if !payment_data.router.is_empty() {
            payment_data.router.clone()
        } else {
            self.find_path(
                source,
                target,
                amount,
                payment_data.max_fee_amount,
                payment_data.udt_type_script.clone(),
                final_tlc_expiry_delta,
                payment_data.tlc_expiry_limit,
                allow_self_payment,
                &payment_data.hop_hints,
            )?
        };

        assert!(!route.is_empty());

        Ok(self.build_router_from_path(&route, payment_data))
    }

    fn build_router_from_path(
        &self,
        route: &Vec<RouterHop>,
        payment_data: SendPaymentData,
    ) -> Vec<PaymentHopData> {
        let invoice = payment_data
            .invoice
            .map(|x| x.parse::<CkbInvoice>().expect("parse CKB invoice"));
        let hash_algorithm = invoice
            .as_ref()
            .and_then(|x| x.hash_algorithm().copied())
            .unwrap_or_default();

        let route_len = route.len();
        let now = now_timestamp_as_millis_u64();
        let mut hops_data = Vec::with_capacity(route.len() + 1);

        for r in route {
            hops_data.push(PaymentHopData {
                amount: r.amount_received,
                next_hop: Some(r.target),
                hash_algorithm,
                expiry: now + r.incoming_tlc_expiry,
                funding_tx_hash: r.channel_outpoint.tx_hash().into(),
                payment_preimage: None,
                custom_records: None,
            });
        }
        hops_data.push(PaymentHopData {
            amount: payment_data.amount,
            next_hop: None,
            hash_algorithm,
            expiry: now + payment_data.final_tlc_expiry_delta,
            funding_tx_hash: Default::default(),
            payment_preimage: payment_data.preimage,
            custom_records: payment_data.custom_records.clone(),
        });
        // assert there is no duplicate node in the route
        assert_eq!(
            hops_data
                .iter()
                .filter_map(|x| x.next_hop)
                .collect::<HashSet<_>>()
                .len(),
            route_len
        );
        hops_data
    }

    // A helper function to evaluate whether an edge should be added to the heap of nodes to visit.
    // We will check the accumulated probability of this edge to be a successful payment, and evaluate
    // the distance from the source node to the final payee. If the distance is shorter than the current
    // distance, we will update the distance and add the node to the heap.
    // This should be called after some sanity checks on the edge, e.g. the amount to send
    // is less than the capacity and the expiry is less than the limit.
    #[allow(clippy::too_many_arguments)]
    fn eval_and_update(
        &self,
        // The channel outpoint of the edge.
        channel_outpoint: &OutPoint,
        // The capacity of the channel.
        channel_capacity: u128,
        // The source node of the edge.
        from: Pubkey,
        // The target node of the edge.
        target: Pubkey,
        // The amount that the source node will send to the target node.
        next_hop_received_amount: u128,
        // The amount that the source node will receive from forwarding `amount_to_send` to the target.
        fee: u128,
        // The TLC expiry for the TLC that the target node will receive.
        incoming_tlc_expiry: u64,
        // The difference in TLC expiry between the TLC that the source node will receive
        // and the TLC that the target node will receive.
        tlc_expiry_delta: u64,
        // The probability of a successful payment from current target to the final payee.
        cur_probability: f64,
        // The weight accumulated from the payment path from current target to the final payee.
        cur_weight: u128,
        // The pending channel count
        cur_pending_count: usize,
        // The distances from nodes to the final payee.
        distances: &mut HashMap<Pubkey, NodeHeapElement>,
        // The priority queue of nodes to be visited (sorted by distance and probability).
        nodes_heap: &mut NodeHeap,
    ) {
        let mut probability = cur_probability
            * self.history.eval_probability(
                from,
                target,
                channel_outpoint,
                next_hop_received_amount,
                channel_capacity,
            );

        let pending_count = self
            .channel_pending_stats
            .get(channel_outpoint)
            .copied()
            .unwrap_or(0)
            + cur_pending_count;
        if pending_count > 0 {
            probability *= (0.95f64).powi(pending_count as i32);
        }

        debug!(
            "probability: {} for channel_outpoint: {:?} from: {:?} => to: {:?}",
            probability, channel_outpoint, from, target
        );
        if probability < DEFAULT_MIN_PROBABILITY {
            debug!("probability is too low: {:?}", probability);
            return;
        }

        let agg_weight = self.edge_weight(next_hop_received_amount, fee, tlc_expiry_delta);
        let weight = cur_weight + agg_weight;

        let distance = self.calculate_distance_based_probability(probability, weight);

        if let Some(node) = distances.get(&from) {
            if distance >= node.distance {
                return;
            }
        }
        let total_amount = next_hop_received_amount + fee;
        let total_tlc_expiry = incoming_tlc_expiry + tlc_expiry_delta;

        let node = NodeHeapElement {
            node_id: from,
            weight,
            distance,
            amount_to_send: total_amount,
            incoming_tlc_expiry: total_tlc_expiry,
            fee_charged: fee,
            probability,
            pending_count,
            next_hop: Some(RouterHop {
                target,
                channel_outpoint: channel_outpoint.clone(),
                // Here we need to use the amount accumulated so far (i.e. with the fees in current hop)
                // because the fee here is for the receiving node to forward the amount to the next node.
                // So the total amount in AddTlc packet should include the fee.
                amount_received: next_hop_received_amount,
                // We need to use cur_hop.incoming_tlc_expiry instead of incoming_tlc_expiry here
                // because we need the expiry for the AddTlc packet sent from source to target.
                // cur_hop.incoming_tlc_expiry is the expiry time for the TLC that is going to be received by the target,
                // while incoming_tlc_expiry is the expiry time for the TLC that is going to be received by the source.
                incoming_tlc_expiry,
            }),
        };
        distances.insert(node.node_id, node.clone());
        nodes_heap.push_or_fix(node);
    }

    // the algorithm works from target-to-source to find the shortest path
    #[allow(clippy::too_many_arguments)]
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
        hop_hints: &[HopHint],
    ) -> Result<Vec<RouterHop>, PathFindError> {
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

        let mut target = target;
        let mut expiry = final_tlc_expiry_delta;
        let mut amount = amount;
        let mut last_edge = None;

        if route_to_self {
            let (edge, new_target, expiry_delta, fee) = self.adjust_target_for_route_self(
                source,
                amount,
                final_tlc_expiry_delta,
                &udt_type_script,
                tlc_expiry_limit,
            )?;
            target = new_target;
            expiry += expiry_delta;
            amount += fee;
            last_edge = Some(edge);
        } else {
            // The calculation of probability and distance requires a capacity of the channel.
            // We don't know the capacity of the channels in the hop hints. We just assume that the capacity
            // of these channels is sufficiently large.
            let sufficiently_large_capacity = u128::MAX;

            // hop hint only referring to private channels for sender node,
            // if we get public channel information for this hophint, we just ignore this hophint
            let filtered_hints = hop_hints.iter().filter(|hint| {
                match self.get_outbound_channel_info_and_update(&hint.channel_outpoint, hint.pubkey)
                {
                    (Some(channel_info), Some(channel_update)) => {
                        channel_info.udt_type_script == udt_type_script && !channel_update.enabled
                    }
                    _ => true,
                }
            });

            for hint in filtered_hints {
                // Say we have a payment path A -- channel 1 --> B -- channel 2 --> C.
                // For now, all the fees that B will receive are calculated based on the fee rate B sets in channel 1.
                // We didn't use the outbound fees for B in channel 2 at all. This is different from lnd,
                // which calculates both the inbound fees in channel 1 and the outbound fees in channel 2.
                // For now, we set the fees to be 0. We may need to change this in the future.
                let fee =
                    calculate_tlc_forward_fee(amount, hint.fee_rate as u128).map_err(|err| {
                        PathFindError::PathFind(format!(
                            "calculate_tlc_forward_fee error: {:?}",
                            err
                        ))
                    })?;
                self.eval_and_update(
                    &hint.channel_outpoint,
                    sufficiently_large_capacity,
                    hint.pubkey,
                    target,
                    amount,
                    fee,
                    expiry,
                    hint.tlc_expiry_delta,
                    1.0,
                    0,
                    0,
                    &mut distances,
                    &mut nodes_heap,
                );
            }
        }
        assert_ne!(source, target);

        // initialize the target node
        nodes_heap.push(NodeHeapElement {
            node_id: target,
            weight: 0,
            distance: 0,
            amount_to_send: amount,
            fee_charged: 0,
            probability: 1.0,
            next_hop: None,
            incoming_tlc_expiry: expiry,
            pending_count: 0,
        });

        while let Some(cur_hop) = nodes_heap.pop() {
            nodes_visited += 1;

            if cur_hop.node_id == source {
                break;
            }

            for (from, to, channel_info, channel_update) in self.get_node_inbounds(cur_hop.node_id)
            {
                let is_initial = from == source;

                assert_eq!(to, cur_hop.node_id);
                if &udt_type_script != channel_info.udt_type_script() {
                    continue;
                }

                if let Some(last_edge) = &last_edge {
                    if &last_edge.channel_outpoint == channel_info.out_point() {
                        continue;
                    }
                }

                edges_expanded += 1;

                let next_hop_received_amount = cur_hop.amount_to_send;
                let fee = if is_initial {
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
                let expiry_delta = if is_initial {
                    0
                } else {
                    channel_update.tlc_expiry_delta
                };

                let incoming_tlc_expiry = cur_hop.incoming_tlc_expiry + expiry_delta;
                if !self.check_channel_amount_and_expiry(
                    amount_to_send,
                    channel_info,
                    channel_update,
                    incoming_tlc_expiry,
                    tlc_expiry_limit,
                ) {
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

                self.eval_and_update(
                    channel_info.out_point(),
                    channel_info.capacity(),
                    from,
                    to,
                    next_hop_received_amount,
                    fee,
                    cur_hop.incoming_tlc_expiry,
                    expiry_delta,
                    cur_hop.probability,
                    cur_hop.weight,
                    cur_hop.pending_count,
                    &mut distances,
                    &mut nodes_heap,
                );
            }
        }

        let mut current = source;
        while let Some(elem) = distances.remove(&current) {
            let edge = elem.next_hop.expect("next_hop is none");
            current = edge.target;
            result.push(edge);
            if current == target {
                break;
            }
        }

        if result.is_empty() || current != target {
            return Err(PathFindError::PathFind("no path found".to_string()));
        }
        if let Some(edge) = last_edge {
            result.push(edge)
        }

        info!(
            "get_route: nodes visited: {}, edges expanded: {}, time: {:?} \nresult: {:?}",
            nodes_visited,
            edges_expanded,
            started_time.elapsed(),
            result
        );
        Ok(result)
    }

    // to resolve the scenario of network with cycle, we use the original `source` node as target,
    // trying to find a new target which has direct channel to `source` node, then the find_path
    // algorithm can work well to assume the network don't contains a cycle.
    // This may makes the result of find_path is not a global optimimized path.
    fn adjust_target_for_route_self(
        &self,
        source: Pubkey,
        amount: u128,
        expiry: u64,
        udt_type_script: &Option<Script>,
        tlc_expiry_limit: u64,
    ) -> Result<(RouterHop, Pubkey, u64, u128), PathFindError> {
        let channels: Vec<_> = self
            .get_node_inbounds(source)
            .filter(|(_, _, channel_info, channel_update)| {
                udt_type_script == channel_info.udt_type_script()
                    && self.check_channel_amount_and_expiry(
                        amount,
                        channel_info,
                        channel_update,
                        channel_update.tlc_expiry_delta,
                        tlc_expiry_limit,
                    )
            })
            .map(|(from, _, channel_info, channel_update)| {
                (
                    from,
                    channel_info.channel_outpoint.clone(),
                    channel_update.tlc_expiry_delta,
                    channel_update.fee_rate,
                    channel_info,
                    channel_update,
                )
            })
            .collect();

        // if there are multiple channels, we will randomly select a channel from the source node for route to self
        // so that the following part of algorithm will always trying to find a path without a cycle in network graph
        if let Some((from, outpoint, tlc_expiry_delta, fee_rate, ..)) =
            channels.choose(&mut thread_rng())
        {
            assert_ne!(source, *from);
            let fee = calculate_tlc_forward_fee(amount, *fee_rate as u128).map_err(|err| {
                PathFindError::PathFind(format!("calculate_tlc_forward_fee error: {:?}", err))
            })?;
            let last_edge = RouterHop {
                target: source,
                channel_outpoint: outpoint.clone(),
                amount_received: amount,
                incoming_tlc_expiry: expiry,
            };
            Ok((last_edge, *from, *tlc_expiry_delta, fee))
        } else {
            return Err(PathFindError::PathFind(
                "no direct channel found for source node".to_string(),
            ));
        }
    }

    fn check_channel_amount_and_expiry(
        &self,
        amount: u128,
        channel_info: &ChannelInfo,
        channel_update: &ChannelUpdateInfo,
        incoming_tlc_expiry: u64,
        tlc_expiry_limit: u64,
    ) -> bool {
        if amount > channel_info.capacity() {
            return false;
        }
        // We should use amount_to_send because that is the amount to be sent over the channel.
        if amount < channel_update.tlc_minimum_value {
            return false;
        }

        // If we already know the balance of the channel, check if we can send the amount.
        if let Some(balance) = channel_update.outbound_liquidity {
            if amount > balance {
                return false;
            }
        }

        if incoming_tlc_expiry > tlc_expiry_limit {
            return false;
        }
        true
    }

    fn edge_weight(&self, amount: u128, fee: u128, htlc_expiry_delta: u64) -> u128 {
        let risk_factor: u128 = 15;
        let time_lock_penalty =
            amount * htlc_expiry_delta as u128 * (risk_factor / CKB_SHANNONS as u128);
        fee + time_lock_penalty
    }

    fn calculate_distance_based_probability(&self, probability: f64, weight: u128) -> u128 {
        assert!(probability > 0.0);
        // FIXME: set this to configurable parameters
        let weight = weight as f64;
        let time_pref = 0.9_f64;
        let default_attempt_cost = 100_f64;
        let penalty = default_attempt_cost * (1.0 / (0.5 - time_pref / 2.0) - 1.0);

        weight as u128 + (penalty / probability) as u128
    }

    // This function is used to build the path from the specified path
    // if there are multiple channels between the same two nodes and channel is not specified,
    // we still try to select the best channel based on the channel's capacity and fee rate,
    // there difference is that we don't need to use a aggregated distance which is used
    // in the path finding algorithm, because we assume all the nodes in the path are
    // already selected, and we only need to find the best channel for each hop.
    pub(crate) fn build_path(
        &self,
        source: Pubkey,
        command: BuildRouterCommand,
    ) -> Result<Vec<RouterHop>, PathFindError> {
        let BuildRouterCommand {
            hops_info: mut router_hops,
            amount,
            final_tlc_expiry_delta,
            udt_type_script,
        } = command;
        router_hops.reverse();

        let mut path = vec![];
        // If not set, the minimum routable amount `1` is used
        let mut agg_amount = amount.unwrap_or(1);
        if agg_amount == 0 {
            return Err(PathFindError::Amount(
                "amount must be greater than 0".to_string(),
            ));
        }

        let mut agg_tlc_expiry = final_tlc_expiry_delta.unwrap_or(DEFAULT_TLC_EXPIRY_DELTA);
        for (idx, cur_hop) in router_hops.iter().enumerate() {
            let prev_hop_pubkey = router_hops.get(idx + 1).map(|h| h.pubkey).unwrap_or(source);

            let mut found = None;
            for (from, to, channel_info, channel_update) in self.get_node_inbounds(cur_hop.pubkey) {
                if from != prev_hop_pubkey {
                    continue;
                }
                if &udt_type_script != channel_info.udt_type_script() {
                    continue;
                }

                // if specified channel outpoint is not empty, we will only use the specified channel
                let channel_outpoint = channel_info.out_point().clone();
                if cur_hop
                    .channel_outpoint
                    .clone()
                    .unwrap_or(channel_outpoint.clone())
                    != channel_outpoint
                {
                    continue;
                }

                let mut amount_to_send = agg_amount;
                let is_initial = from == source;
                let fee = if is_initial {
                    0
                } else {
                    calculate_tlc_forward_fee(amount_to_send, channel_update.fee_rate as u128)
                        .map_err(|err| {
                            PathFindError::PathFind(format!(
                                "calculate_tlc_forward_fee error: {:?}",
                                err
                            ))
                        })?
                };
                amount_to_send += fee;
                if amount_to_send > channel_info.capacity() {
                    debug!(
                        "current_amount: {} > channel_info.capacity {}",
                        amount_to_send,
                        channel_info.capacity()
                    );
                    continue;
                }
                if let Some(balance) = channel_update.outbound_liquidity {
                    if amount_to_send > balance {
                        continue;
                    }
                }

                let expiry_delta = if is_initial {
                    0
                } else {
                    channel_update.tlc_expiry_delta
                };

                let current_incoming_tlc_expiry = agg_tlc_expiry + expiry_delta;
                let probability = self.history.eval_probability(
                    from,
                    to,
                    &channel_outpoint,
                    amount_to_send,
                    channel_info.capacity(),
                );

                let weight = self.edge_weight(amount_to_send, fee, current_incoming_tlc_expiry);
                let distance = self.calculate_distance_based_probability(probability, weight);

                if let Some((old_distance, _fee, _edge)) = &found {
                    if distance >= *old_distance {
                        continue;
                    }
                }
                found = Some((
                    distance,
                    fee,
                    RouterHop {
                        target: to,
                        channel_outpoint: channel_outpoint.clone(),
                        amount_received: agg_amount,
                        incoming_tlc_expiry: agg_tlc_expiry,
                    },
                ));
            }
            if let Some((_, fee, edge)) = found {
                agg_tlc_expiry += edge.incoming_tlc_expiry;
                agg_amount = edge.amount_received + fee;
                path.push(edge.clone());
            } else {
                return Err(PathFindError::PathFind("no path found".to_string()));
            }
        }
        path.reverse();
        Ok(path)
    }
}

pub trait NetworkGraphStateStore {
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession>;
    fn get_payment_sessions_with_status(&self, status: PaymentSessionStatus)
        -> Vec<PaymentSession>;
    fn insert_payment_session(&self, session: PaymentSession);
    fn insert_payment_history_result(
        &mut self,
        channel_outpoint: OutPoint,
        direction: Direction,
        result: TimedResult,
    );
    fn get_payment_history_results(&self) -> Vec<(OutPoint, Direction, TimedResult)>;
}

/// The status of a payment, will update as the payment progresses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PaymentSessionStatus {
    /// initial status, payment session is created, no HTLC is sent
    Created,
    /// the first hop AddTlc is sent successfully and waiting for the response
    Inflight,
    /// related HTLC is successfully settled
    Success,
    /// related HTLC is failed
    Failed,
}

/// The node and channel information in a payment route hop
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRouteNode {
    /// the public key of the node
    pub pubkey: Pubkey,
    /// the amount for this hop
    pub amount: u128,
    /// the channel outpoint for this hop
    #[serde_as(as = "EntityHex")]
    pub channel_outpoint: OutPoint,
}

/// The router is a list of nodes that the payment will go through.
/// We store in the payment session and then will use it to track the payment history.
/// The router is a list of nodes that the payment will go through.
/// For example:
///    `A(amount, channel) -> B -> C -> D`
/// means A will send `amount` with `channel` to B.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionRoute {
    /// the nodes in the route
    pub nodes: Vec<SessionRouteNode>,
}

impl SessionRoute {
    // Create a new route from the source to the target with the given payment hops.
    // The payment hops are the hops that the payment will go through.
    // for a payment route A -> B -> C -> D
    // the `payment_hops` is [B, C, D], which is a convenient way for onion routing.
    // here we need to create a session route with source, which is A -> B -> C -> D
    pub fn new(source: Pubkey, target: Pubkey, payment_hops: &[PaymentHopData]) -> Self {
        let nodes = std::iter::once(source)
            .chain(
                payment_hops
                    .iter()
                    .map(|hop| hop.next_hop.unwrap_or(target)),
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

    pub fn is_send_payment_with_router(&self) -> bool {
        !self.request.router.is_empty()
    }

    pub fn first_hop_channel_outpoint_eq(&self, out_point: &OutPoint) -> bool {
        self.route
            .nodes
            .first()
            .map(|x| x.channel_outpoint.eq(out_point))
            .unwrap_or_default()
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

    fn channel_outpoints(&self) -> impl Iterator<Item = &OutPoint> {
        self.route.nodes.iter().map(|x| &x.channel_outpoint)
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
            custom_records: session.request.custom_records,
            fee,
            #[cfg(any(debug_assertions, feature = "bench"))]
            router: session.route,
        }
    }
}
