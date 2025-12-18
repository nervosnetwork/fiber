use super::channel::{ChannelActorState, ChannelActorStateStore, ChannelTlcInfo};
use super::config::AnnouncedNodeName;
use super::features::FeatureVector;
use super::gossip::GossipMessageStore;
use super::history::{Direction, InternalResult, PaymentHistory, TimedResult};
use super::network::{get_chain_hash, BuildRouterCommand, HopHint, SendPaymentData};
use super::path::NodeHeap;
use super::types::{
    BroadcastMessageID, BroadcastMessageWithTimestamp, ChannelAnnouncement, ChannelUpdate, Hash256,
    NodeAnnouncement,
};
use super::types::{Cursor, Pubkey, TlcErr};
use crate::ckb::config::UdtCfgInfos;
use crate::fiber::config::{
    DEFAULT_FINAL_TLC_EXPIRY_DELTA, DEFAULT_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT,
};
use crate::fiber::fee::calculate_tlc_forward_fee;
use crate::fiber::history::SentNode;
use crate::fiber::path::NodeHeapElement;
use crate::fiber::payment::{Attempt, AttemptStatus, PaymentSession, PaymentStatus};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::serde_utils::{U128Hex, U64Hex};
use crate::fiber::types::PaymentHopData;
use crate::fiber::types::TrampolineOnionData;
use crate::invoice::CkbInvoice;
use crate::now_timestamp_as_millis_u64;
use ckb_types::packed::{OutPoint, Script};
use ckb_types::prelude::Entity;
use parking_lot::Mutex;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tentacle::multiaddr::MultiAddr;
use tentacle::secio::PeerId;
use tentacle::utils::{is_reachable, multiaddr_to_socketaddr};
use thiserror::Error;
use tracing::log::error;
use tracing::{debug, info, trace, warn};
const DEFAULT_MIN_PROBABILITY: f64 = 0.01;

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Details about a node in the network, known from the network announcement.
pub struct NodeInfo {
    // The node's public key, which is used to identify the node in the network.
    pub node_id: Pubkey,
    // The node version
    pub version: String,
    // The timestamp set by the owner for the node announcement.
    pub timestamp: u64,
    // Tentatively using 64 bits for features. May change the type later while developing.
    // rust-lightning uses a Vec<u8> here.
    pub features: FeatureVector,
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
            version: value.version,
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

    pub fn get_send_node(&self, from: Pubkey) -> Option<SentNode> {
        if self.node1() == from {
            Some(SentNode::Node1)
        } else if self.node2() == from {
            Some(SentNode::Node2)
        } else {
            None
        }
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

        let Some(channel_outpoint) = state.get_funding_transaction_outpoint() else {
            return Err("Channel outpoint is not set".to_string());
        };

        let timestamp = state.must_get_funding_transaction_timestamp();
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

/// TODO: store this into DB in future?
#[derive(Clone, Debug, Default)]
pub struct ChannelStatElem {
    // The pending payment attempt for a channel,
    // increase when a payment is sent, decreasing when a payment is succeed or failed.
    pub pending_count: usize,
    // The total amount sent through the channel,
    // increase when a payment is sent, decreasing when a payment is only failed.
    pub node1_sent_amount: u128,
    pub node2_sent_amount: u128,
}

impl ChannelStatElem {
    pub fn new(pending_count: usize, sent_node: SentNode, amount: u128) -> Self {
        if sent_node == SentNode::Node1 {
            Self {
                pending_count,
                node1_sent_amount: amount,
                node2_sent_amount: 0,
            }
        } else {
            Self {
                pending_count,
                node1_sent_amount: 0,
                node2_sent_amount: amount,
            }
        }
    }

    pub fn new_attempt(&mut self, send_node: SentNode, amount: u128) {
        self.pending_count += 1;
        if send_node == SentNode::Node1 {
            self.node1_sent_amount += amount;
        } else {
            self.node2_sent_amount += amount;
        }
    }

    pub fn untrack_attempt(&mut self, send_node: SentNode, amount: u128) {
        self.pending_count = self.pending_count.saturating_sub(1);
        if send_node == SentNode::Node1 {
            self.node1_sent_amount = self.node1_sent_amount.saturating_sub(amount);
        } else {
            self.node2_sent_amount = self.node2_sent_amount.saturating_sub(amount);
        }
    }

    // when we cat channel update event from direct channel, the actual channel balance is more accurate
    // than the amount here, so we need to reset the sent amount to 0.
    pub fn reset_attempt(&mut self, sent_node: SentNode) {
        if sent_node == SentNode::Node1 {
            self.node1_sent_amount = 0;
        } else {
            self.node2_sent_amount = 0;
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GraphChannelStat {
    base: Option<Arc<Mutex<GraphChannelStat>>>,
    inner: HashMap<OutPoint, ChannelStatElem>,
}

impl GraphChannelStat {
    pub fn new(base: Option<Arc<Mutex<GraphChannelStat>>>) -> Self {
        Self {
            inner: HashMap::new(),
            base,
        }
    }

    pub fn get_channel_count(&self, channel_outpoint: &OutPoint) -> usize {
        self.base
            .as_ref()
            .map_or(0, |base| base.lock().get_channel_count(channel_outpoint))
            + self
                .inner
                .get(channel_outpoint)
                .map_or(0, |stat| stat.pending_count)
    }

    pub fn get_channel_sent_amount(
        &self,
        channel_outpoint: &OutPoint,
        sent_node: SentNode,
    ) -> u128 {
        self.base.as_ref().map_or(0, |base| {
            base.lock()
                .get_channel_sent_amount(channel_outpoint, sent_node)
        }) + self
            .inner
            .get(channel_outpoint)
            .map(|stat| match sent_node {
                SentNode::Node1 => stat.node1_sent_amount,
                SentNode::Node2 => stat.node2_sent_amount,
            })
            .unwrap_or(0)
    }

    pub fn add_channel(&mut self, channel_outpoint: &OutPoint, sent_node: SentNode, amount: u128) {
        self.inner
            .entry(channel_outpoint.clone())
            .and_modify(|e| e.new_attempt(sent_node, amount))
            .or_insert(ChannelStatElem::new(1, sent_node, amount));
    }

    pub fn remove_channel(&mut self, channel_outpoint: &OutPoint) {
        self.inner.remove(channel_outpoint);
    }

    pub fn reset_channel(&mut self, channel_outpoint: &OutPoint, sent_node: SentNode) {
        self.inner
            .entry(channel_outpoint.clone())
            .and_modify(|e| e.reset_attempt(sent_node));
    }

    pub fn untrack_channel(
        &mut self,
        channel_outpoint: &OutPoint,
        sent_node: SentNode,
        amount: u128,
    ) {
        self.inner
            .entry(channel_outpoint.clone())
            .and_modify(|e| e.untrack_attempt(sent_node, amount));
    }
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

    // Channel stats map, used to track the attempts for each channel,
    // this information is used to HELP the path finding algorithm for better routing in two ways:
    // 1. If a channel has more pending payment attempts, it may be overloaded and should not be used for routing.
    // 2. For middle hops, network graph can only get the channel capacity, track the balance of channel will be helpful
    channel_stats: Arc<Mutex<GraphChannelStat>>,

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

    #[cfg(test)]
    pub(crate) add_rand_expiry_delta: bool,
}

#[derive(Error, Debug)]
pub enum PathFindError {
    #[error("Graph error: {0}")]
    Amount(String),
    #[error("PathFind error: no path found")]
    NoPathFound,
    #[error("Overflow error: {0}")]
    Overflow(String),
    #[error("Feature not enabled: {0}")]
    FeatureNotEnabled(String),
    #[error("Node not found in graph: {0}")]
    UnknownNode(String),
    #[error("Insufficient balance: {0}")]
    InsufficientBalance(String),
    #[error("Path find failed for min_tlc_value error: {0}")]
    TlcMinValue(u128),
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
            channel_stats: Default::default(),
            nodes: HashMap::new(),
            latest_cursor: Cursor::default(),
            store: store.clone(),
            history: PaymentHistory::new(source, None, store),
            announce_private_addr,
            #[cfg(test)]
            add_rand_expiry_delta: true,
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

    pub(crate) fn channel_stats(&self) -> Arc<Mutex<GraphChannelStat>> {
        self.channel_stats.clone()
    }

    #[cfg(test)]
    pub(crate) fn set_add_rand_expiry_delta(&mut self, add: bool) {
        self.add_rand_expiry_delta = add;
    }

    pub(crate) fn reset_channel_stats_for_direct_channel(
        &mut self,
        channel_outpoint: &OutPoint,
        node: Pubkey,
    ) {
        if let Some(sent_node) = self.get_channel_sent_node(channel_outpoint, node) {
            self.channel_stats
                .lock()
                .reset_channel(channel_outpoint, sent_node);
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
        for message in messages {
            self.update_latest_cursor(message.cursor());
            if message.chain_hash() != get_chain_hash() {
                warn!(
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
                    debug!("process channel update: {:?}", &channel_update);
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
                self.history
                    .remove_channel_history(&channel_info.channel_outpoint);
                self.channels
                    .insert(channel_info.channel_outpoint.clone(), channel_info);
            }
            OwnedChannelUpdateEvent::Down(channel_outpoint) => {
                self.channels.remove(&channel_outpoint);
                self.channel_stats.lock().remove_channel(&channel_outpoint);
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
                self.reset_channel_stats_for_direct_channel(&channel_outpoint, node);
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

    #[cfg(any(test, feature = "bench"))]
    pub(crate) fn clear_history(&mut self) {
        self.history.reset();
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
        // the channel_update RPC has already checked the tlc_expiry_delta is in the range
        // but a malicious node may send a channel update with a too large expiry delta
        // which makes the network graph contains a channel update with a too large expiry delta.
        // We need to check it again here to avoid any malicious channel update
        // Note: we don't check the tlc_expiry_delta is too small here, because it does not effect
        // the path finding, and a too small tlc_expiry_delta only makes the hop itself more risky.
        if channel_update.tlc_expiry_delta > MAX_PAYMENT_TLC_EXPIRY_LIMIT {
            error!(
                "Channel update has too large expiry delta: {} > {}, channel update: {:?}",
                channel_update.tlc_expiry_delta, MAX_PAYMENT_TLC_EXPIRY_LIMIT, &channel_update
            );
            return None;
        }
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
            Some(old_update) if old_update.timestamp >= channel_update.timestamp => {
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
        debug!("Processing node announcement: {:?}", &node_announcement);
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
            Some(old_node) if old_node.timestamp >= node_info.timestamp => {
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

    pub fn get_channel_sent_node(&self, outpoint: &OutPoint, from: Pubkey) -> Option<SentNode> {
        self.get_channel(outpoint).and_then(|channel_info| {
            if channel_info.node1() == from {
                Some(SentNode::Node1)
            } else if channel_info.node2() == from {
                Some(SentNode::Node2)
            } else {
                None
            }
        })
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

                    // assuming channel is closed if disabled from the both side
                    let is_closed = channel_info.update_of_node1.is_some_and(|u| !u.enabled)
                        && channel_info.update_of_node2.is_some_and(|u| !u.enabled);
                    if !is_closed {
                        Some(channel_info)
                    } else {
                        None
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

    pub fn get_node_outbounds(
        &self,
        node_id: Pubkey,
    ) -> impl Iterator<Item = (Pubkey, &ChannelInfo, &ChannelUpdateInfo)> {
        let channels: Vec<_> = self
            .channels
            .values()
            .filter_map(move |channel| {
                match channel.update_of_node1.as_ref() {
                    Some(info) if node_id == channel.node1() && info.enabled => {
                        return Some((channel.node2(), channel, info));
                    }
                    _ => {}
                }
                match channel.update_of_node2.as_ref() {
                    Some(info) if node_id == channel.node2() && info.enabled => {
                        return Some((channel.node1(), channel, info));
                    }
                    _ => {}
                }
                None
            })
            .collect();

        channels.into_iter()
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

    pub(crate) fn track_attempt_router(&mut self, attempt: &Attempt) {
        for (from, channel_outpoint, amount) in attempt.channel_outpoints() {
            if let Some(sent_node) = self.get_channel_sent_node(channel_outpoint, from) {
                self.channel_stats
                    .lock()
                    .add_channel(channel_outpoint, sent_node, amount);
            }
        }
    }

    pub(crate) fn untrack_attempt_router(&mut self, attempt: &Attempt) {
        for (from, channel_outpoint, amount) in attempt.channel_outpoints() {
            if let Some(sent_node) = self.get_channel_sent_node(channel_outpoint, from) {
                self.channel_stats
                    .lock()
                    .untrack_channel(channel_outpoint, sent_node, amount);
            }
        }
    }

    pub(crate) fn record_attempt_success(&mut self, attempt: &Attempt) {
        self.untrack_attempt_router(attempt);
        let session_route = &attempt.route.nodes;
        let mut result = InternalResult::default();
        result.succeed_range_pairs(session_route, 0, session_route.len() - 1);
        self.history.apply_internal_result(result);
    }

    pub(crate) fn record_attempt_fail(
        &mut self,
        attempt: &Attempt,
        tlc_err: TlcErr,
        first_hop_error: bool,
    ) -> bool {
        if !first_hop_error {
            self.untrack_attempt_router(attempt);
        }
        let mut internal_result = InternalResult::default();
        let nodes = &attempt.route.nodes;
        let need_to_retry = internal_result.record_payment_fail(nodes, tlc_err);
        self.history.apply_internal_result(internal_result);
        let is_send_payment_with_router = self
            .store
            .get_payment_session(attempt.payment_hash)
            .is_some_and(|p| p.is_payment_with_router());
        return need_to_retry && !is_send_payment_with_router;
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
        amount: u128,
        amount_low_bound: Option<u128>,
        max_fee_amount: Option<u128>,
        payment_data: &SendPaymentData,
    ) -> Result<Vec<PaymentHopData>, PathFindError> {
        info!(
            "entered build_route: amount: {}, amount_low_bound: {:?}",
            amount, amount_low_bound
        );
        let source = self.get_source_pubkey();
        let target = payment_data.target_pubkey;

        if source == target && !payment_data.allow_self_payment {
            return Err(PathFindError::FeatureNotEnabled(
                "allow_self_payment is not enabled, can not pay to self".to_string(),
            ));
        }

        let (route_hops, route_amount, trampoline_payload) = if !payment_data.router.is_empty() {
            // If a router is explicitly provided, use it.
            // Assume it's valid for the requested `amount`.
            (payment_data.router.clone(), amount, None)
        } else {
            let amount_low_bound = amount_low_bound.unwrap_or(u128::MAX);
            error!(
                "debug now allow_trampoline_routing: {:?}",
                payment_data.allow_trampoline_routing()
            );
            match self.find_path_with_payment_data(source, amount, max_fee_amount, payment_data) {
                Ok(route) => (route, amount, None),
                Err(PathFindError::NoPathFound) | Err(PathFindError::TlcMinValue(_))
                    if payment_data.allow_mpp() && amount_low_bound < amount =>
                {
                    let Ok(res) = self.binary_find_path_in_range(
                        source,
                        amount.saturating_sub(1),
                        amount_low_bound,
                        max_fee_amount,
                        payment_data,
                    ) else {
                        return Err(PathFindError::NoPathFound);
                    };
                    (res.0, res.1, None)
                }
                Err(PathFindError::NoPathFound) if payment_data.allow_trampoline_routing() => {
                    let (route, amount_to_trampoline, trampoline_payload) =
                        self.find_trampoline_route(source, amount, max_fee_amount, payment_data)?;
                    error!(
                        "debug now found trampoline route: {:?}, amount_to_trampoline: {}",
                        route, amount_to_trampoline
                    );
                    (route, amount_to_trampoline, Some(trampoline_payload))
                }
                Err(err) => return Err(err),
            }
        };

        debug_assert!(
            !route_hops.is_empty(),
            "Route hops should not be empty if Ok"
        );

        Ok(
            self.build_router_from_path(
                &route_hops,
                route_amount,
                payment_data,
                trampoline_payload,
            ),
        )
    }

    fn is_node_support_trampoline_routing(&self, node: &Pubkey) -> bool {
        self.nodes
            .get(node)
            .map(|node_info| node_info.features.supports_trampoline_routing())
            .unwrap_or(true)
    }

    fn find_trampoline_route(
        &self,
        source: Pubkey,
        final_amount: u128,
        max_fee_amount: Option<u128>,
        payment_data: &SendPaymentData,
    ) -> Result<(Vec<RouterHop>, u128, TrampolineOnionData), PathFindError> {
        // Limit candidate count to avoid pathological full-graph scans.
        const MAX_TRAMPOLINE_CANDIDATES: usize = 32;

        let target = payment_data.target_pubkey;
        let total_fee_budget = max_fee_amount.unwrap_or(0);

        let invoice = payment_data
            .invoice
            .clone()
            .map(|x| x.parse::<CkbInvoice>().expect("parse CKB invoice"));
        let hash_algorithm = invoice
            .as_ref()
            .and_then(|x| x.hash_algorithm().copied())
            .unwrap_or_default();

        let mut trampoline_candidates = self
            .nodes
            .keys()
            .filter(|n| **n != source && **n != target)
            .filter(|n| self.is_node_support_trampoline_routing(n))
            .take(MAX_TRAMPOLINE_CANDIDATES)
            .copied()
            .collect::<Vec<_>>();

        // Prefer determinism across runs.
        trampoline_candidates.sort();

        for candidate in trampoline_candidates {
            let fee_budget_forward = total_fee_budget.saturating_mul(50) / 100;
            let fee_budget_routing = total_fee_budget.saturating_sub(fee_budget_forward);
            let amount_to_trampoline = final_amount.saturating_add(fee_budget_forward);
            error!(
                "haha fee_budget_forward: {:?} final_amount: {:?} amount_to_trampoline: {:?}",
                fee_budget_forward, final_amount, amount_to_trampoline
            );

            let Ok(route_to_trampoline) = self.find_path(
                source,
                candidate,
                amount_to_trampoline,
                Some(fee_budget_routing),
                payment_data.udt_type_script.clone(),
                payment_data.final_tlc_expiry_delta,
                payment_data.tlc_expiry_limit,
                payment_data.allow_self_payment,
                &payment_data.hop_hints,
                &payment_data.channel_stats,
                payment_data.allow_mpp(),
            ) else {
                continue;
            };

            let trampoline_payload = TrampolineOnionData {
                final_recipient: payment_data.target_pubkey,
                final_amount,
                udt_type_script: payment_data
                    .udt_type_script
                    .as_ref()
                    .map(|s| s.as_bytes().to_vec()),
                final_tlc_expiry_delta: payment_data.final_tlc_expiry_delta,
                payment_preimage: payment_data.preimage,
                hash_algorithm,
                custom_records: payment_data.custom_records.clone(),
            };

            return Ok((
                route_to_trampoline,
                amount_to_trampoline,
                trampoline_payload,
            ));
        }

        Err(PathFindError::NoPathFound)
    }

    fn find_path_with_payment_data(
        &self,
        source: Pubkey,
        amount: u128,
        max_fee_amount: Option<u128>,
        payment_data: &SendPaymentData,
    ) -> Result<Vec<RouterHop>, PathFindError> {
        self.find_path(
            source,
            payment_data.target_pubkey,
            amount,
            max_fee_amount,
            payment_data.udt_type_script.clone(),
            payment_data.final_tlc_expiry_delta,
            payment_data.tlc_expiry_limit,
            payment_data.allow_self_payment,
            &payment_data.hop_hints,
            &payment_data.channel_stats,
            payment_data.allow_mpp(),
        )
    }

    // This function attempts to find a path for the given `amount` using binary search.
    // It will try to find the largest feasible sub-amount that can be sent.
    // not a efficient algorithm, but it is simple and works for most cases.
    // maybe we can use a more efficient algorithm in the future.
    fn binary_find_path_in_range(
        &self,
        source: Pubkey,
        amount: u128,
        amount_low_bound: u128,
        max_fee_amount: Option<u128>,
        payment_data: &SendPaymentData,
    ) -> Result<(Vec<RouterHop>, u128), PathFindError> {
        debug!(
            "binary_find_path_in_range for amount: {} low_bound: {} max_fee_amount: {:?}",
            amount, amount_low_bound, max_fee_amount
        );

        let mut low = amount_low_bound;
        let mut high = amount;

        let direct_channel_amounts: Vec<_> = self
            .get_node_outbounds(source)
            .filter_map(|(_, channel_info, update_info)| {
                if channel_info.udt_type_script == payment_data.udt_type_script {
                    Some(update_info.outbound_liquidity.unwrap_or(0))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let max_liquidity = direct_channel_amounts.iter().max().copied().unwrap_or(0);
        high = high.min(max_liquidity);

        const MAX_BINARY_SEARCH_ITERATIONS: usize = 50;
        let mut best_route_found: Option<Vec<RouterHop>> = None;
        let mut amount_for_best_route: u128 = 0;
        let mut iterations = 0;

        while low <= high && iterations < MAX_BINARY_SEARCH_ITERATIONS {
            iterations += 1;

            let mid = low + (high - low) / 2;
            debug!("iterations: {} mid: {}", iterations, mid);

            match self.find_path_with_payment_data(source, mid, max_fee_amount, payment_data) {
                Ok(route) => {
                    // Found a path for `mid`. Store it and try for a larger amount.
                    if mid > amount_for_best_route {
                        best_route_found = Some(route);
                        amount_for_best_route = mid;
                    }
                    low = mid.saturating_add(1);
                }
                Err(PathFindError::NoPathFound) => {
                    // `mid` is too high, try smaller.
                    high = mid.saturating_sub(1);
                }
                Err(PathFindError::TlcMinValue(tlc_min_value)) => {
                    low = tlc_min_value;
                }
                Err(other_error) => return Err(other_error),
            }
        }

        if let Some(route) = best_route_found {
            Ok((route, amount_for_best_route))
        } else {
            return Err(PathFindError::NoPathFound);
        }
    }

    fn build_router_from_path(
        &self,
        route: &Vec<RouterHop>,
        max_amount: u128,
        payment_data: &SendPaymentData,
        trampoline_payload: Option<TrampolineOnionData>,
    ) -> Vec<PaymentHopData> {
        let invoice = payment_data
            .invoice
            .clone()
            .map(|x| x.parse::<CkbInvoice>().expect("parse CKB invoice"));
        let hash_algorithm = invoice
            .as_ref()
            .and_then(|x| x.hash_algorithm().copied())
            .unwrap_or_default();

        let route_len = route.len();
        let now = now_timestamp_as_millis_u64();
        let mut hops_data = Vec::with_capacity(route.len() + 1);
        let rand_tlc_expiry_delta = self.rand_tlc_expiry_delta(route);

        for r in route {
            hops_data.push(PaymentHopData {
                amount: r.amount_received,
                next_hop: Some(r.target),
                hash_algorithm,
                expiry: now + r.incoming_tlc_expiry + rand_tlc_expiry_delta,
                funding_tx_hash: r.channel_outpoint.tx_hash().into(),
                ..Default::default()
            });
        }

        let (last_amount, payment_preimage, custom_records, trampoline_onion) =
            match trampoline_payload {
                Some(payload) => (payload.final_amount, None, None, Some(payload.serialize())),
                None => (
                    max_amount,
                    payment_data.preimage,
                    payment_data.custom_records.clone(),
                    None,
                ),
            };

        hops_data.push(PaymentHopData {
            amount: last_amount,
            hash_algorithm,
            expiry: now + payment_data.final_tlc_expiry_delta + rand_tlc_expiry_delta,
            payment_preimage,
            custom_records,
            trampoline_onion,
            ..Default::default()
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

    fn is_node_support_mpp(&self, node: &Pubkey) -> bool {
        self.nodes
            .get(node)
            .map(|node_info| node_info.features.supports_basic_mpp())
            .unwrap_or(true)
    }

    fn rand_tlc_expiry_delta(&self, route: &[RouterHop]) -> u64 {
        #[cfg(test)]
        if !self.add_rand_expiry_delta {
            return 0;
        }
        // https://github.com/lightning/bolts/blob/master/07-routing-gossip.md#recommendations-for-routing
        // Add a small random tlc_expiry_delta to each hop for privacy reason.
        // This makes it harder for the intermediate nodes to infer their position in the route.
        let max_rand_expiry_num = MAX_PAYMENT_TLC_EXPIRY_LIMIT
            .saturating_sub(route.first().map(|r| r.incoming_tlc_expiry).unwrap_or(0))
            / DEFAULT_TLC_EXPIRY_DELTA;

        thread_rng().gen_range(0..max_rand_expiry_num.max(1)) * DEFAULT_TLC_EXPIRY_DELTA
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
        // The tlc min value of the channel.
        tlc_min_value: u128,
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
        channel_stats: &GraphChannelStat,
    ) {
        let mut probability = cur_probability
            * self.history.eval_probability(
                from,
                target,
                channel_outpoint,
                next_hop_received_amount,
                channel_capacity,
            );

        let pending_count = channel_stats.get_channel_count(channel_outpoint) + cur_pending_count;

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

        let node = NodeHeapElement {
            node_id: from,
            weight,
            distance,
            amount_to_send: next_hop_received_amount + fee,
            tlc_min_value,
            incoming_tlc_expiry: incoming_tlc_expiry + tlc_expiry_delta,
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
                // cur_hop.incoming_tlc_expiry is the expiry time for the TLC that is going to be
                // received by the target, while incoming_tlc_expiry is the expiry time for the TLC
                // that is going to be received by the source.
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
        final_tlc_expiry: u64,
        tlc_expiry_limit: u64,
        allow_self: bool,
        hop_hints: &[HopHint],
        channel_stats: &GraphChannelStat,
        allow_mpp: bool,
    ) -> Result<Vec<RouterHop>, PathFindError> {
        debug!(
            "begin find_path from {:?} to {:?} amount: {:?}",
            source, target, amount
        );

        let route_to_self = source == target;

        if amount == 0 {
            return Err(PathFindError::Amount(
                "amount must be greater than 0".to_string(),
            ));
        }

        if amount.checked_add(max_fee_amount.unwrap_or(0)).is_none() {
            return Err(PathFindError::Amount(format!(
                "amount {} + max_fee_amount {} overflow",
                amount,
                max_fee_amount.unwrap_or(0)
            )));
        }

        if source == target && !allow_self {
            return Err(PathFindError::FeatureNotEnabled(
                "allow_self_payment is not enable, can not pay self".to_string(),
            ));
        }

        if allow_mpp && !self.is_node_support_mpp(&target) {
            return Err(PathFindError::FeatureNotEnabled(
                "MPP is not supported by the target node".to_string(),
            ));
        }

        if route_to_self {
            let direct_channels = self.adjust_target_for_route_self(
                source,
                amount,
                final_tlc_expiry,
                &udt_type_script,
                tlc_expiry_limit,
                channel_stats,
                allow_mpp,
            );
            for (edge, new_target, expiry_delta, fee) in direct_channels {
                let final_tlc_expiry = final_tlc_expiry.saturating_add(expiry_delta);
                let amount = amount.saturating_add(fee);
                let max_fee_amount = max_fee_amount.map(|x| x.saturating_sub(fee));
                if let Ok(res) = self.inner_find_path(
                    source,
                    new_target,
                    amount,
                    max_fee_amount,
                    udt_type_script.clone(),
                    final_tlc_expiry,
                    tlc_expiry_limit,
                    &[],
                    channel_stats,
                    allow_mpp,
                    Some(edge),
                ) {
                    return Ok(res);
                }
            }
            return Err(PathFindError::NoPathFound);
        } else {
            self.inner_find_path(
                source,
                target,
                amount,
                max_fee_amount,
                udt_type_script.clone(),
                final_tlc_expiry,
                tlc_expiry_limit,
                hop_hints,
                channel_stats,
                allow_mpp,
                None,
            )
        }
    }

    // the algorithm works from target-to-source to find the shortest path
    // assume thers is no cycle from source node to target node,
    // since we already use `adjust_target_for_route_self` to adjust target
    #[allow(clippy::too_many_arguments)]
    fn inner_find_path(
        &self,
        source: Pubkey,
        target: Pubkey,
        amount: u128,
        max_fee_amount: Option<u128>,
        udt_type_script: Option<Script>,
        final_tlc_expiry_delta: u64,
        tlc_expiry_limit: u64,
        hop_hints: &[HopHint],
        channel_stats: &GraphChannelStat,
        allow_mpp: bool,
        last_edge: Option<RouterHop>,
    ) -> Result<Vec<RouterHop>, PathFindError> {
        debug!(
            "begin inner_find_path from {:?} to {:?} amount: {:?}",
            source, target, amount
        );
        let started_time = crate::time::Instant::now();
        let nodes_len = self.nodes.len();
        let route_to_self = source == target;

        let mut result = vec![];
        let mut nodes_visited = 0;
        let mut edges_expanded = 0;
        let mut nodes_heap = NodeHeap::new(nodes_len);
        let mut distances = HashMap::<Pubkey, NodeHeapElement>::new();

        let expiry = final_tlc_expiry_delta;
        if !route_to_self {
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
                        PathFindError::Overflow(format!(
                            "calculate_tlc_forward_fee error: {:?}",
                            err
                        ))
                    })?;
                // hop hint is only used for private channels, we assume there is no tlc_min_value limit
                let tlc_min_val = 0;
                self.eval_and_update(
                    &hint.channel_outpoint,
                    tlc_min_val,
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
                    channel_stats,
                );
            }
        }
        assert_ne!(source, target);

        // initialize the target node
        nodes_heap.push(NodeHeapElement {
            node_id: target,
            tlc_min_value: 0,
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
                let is_source = from == source;
                if allow_mpp && !self.is_node_support_mpp(&from) {
                    continue;
                }

                debug_assert_eq!(to, cur_hop.node_id);
                if &udt_type_script != channel_info.udt_type_script() {
                    if tracing::enabled!(tracing::Level::TRACE)
                        && udt_type_script.is_some()
                        && channel_info.udt_type_script().is_some()
                    {
                        // Common errors that channel and payment uses different UDT type scripts
                        trace!("skip channel because of different UDT type scripts: payment: {:?}, channel: {:?}", udt_type_script, channel_info.udt_type_script());
                    }
                    continue;
                }

                if let Some(last_edge) = &last_edge {
                    if &last_edge.channel_outpoint == channel_info.out_point() {
                        continue;
                    }
                }

                edges_expanded += 1;

                let next_hop_received_amount = cur_hop.amount_to_send;
                let fee = if is_source {
                    0
                } else {
                    match calculate_tlc_forward_fee(
                        next_hop_received_amount,
                        channel_update.fee_rate as u128,
                    ) {
                        Ok(fee) => fee,
                        // skip this edge if the fee calculation fails
                        Err(_) => {
                            continue;
                        }
                    }
                };
                let amount_to_send = next_hop_received_amount.saturating_add(fee);
                let expiry_delta = if is_source {
                    0
                } else {
                    channel_update.tlc_expiry_delta
                };

                let incoming_tlc_expiry = cur_hop.incoming_tlc_expiry.saturating_add(expiry_delta);

                let send_node = channel_info
                    .get_send_node(from)
                    .expect("send_node should exist");

                let tlc_min_value = channel_update.tlc_minimum_value;
                if !self.check_channel_amount_and_expiry(
                    amount_to_send,
                    channel_info,
                    channel_update,
                    incoming_tlc_expiry,
                    tlc_expiry_limit,
                    send_node,
                    channel_stats,
                    allow_mpp,
                ) {
                    continue;
                }

                // if the amount to send is greater than the amount we have, skip this edge
                if let Some(max_fee_amount) = max_fee_amount {
                    if amount_to_send > amount.saturating_add(max_fee_amount) {
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
                    tlc_min_value,
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
                    channel_stats,
                );
            }
        }

        let mut current = source;
        let mut max_min_tlc_value = 0;
        while let Some(elem) = distances.remove(&current) {
            let edge = elem.next_hop.expect("next_hop is none");
            if elem.amount_to_send < elem.tlc_min_value {
                max_min_tlc_value = max_min_tlc_value.max(elem.tlc_min_value);
            }
            current = edge.target;
            result.push(edge);
            if current == target {
                break;
            }
        }

        if result.is_empty() || current != target {
            // TODO check total outbound balance and return error if it's not enough
            // this can help us early return if the payment is not possible to be sent
            // otherwise when PathFind error is returned, we need to retry with half amount
            return Err(PathFindError::NoPathFound);
        }
        if let Some(edge) = last_edge {
            result.push(edge)
        }

        // check the router meets tlc_min_val requirements, otherwise return proper error
        // only do this for mpp payments, because for single payment we already checked the amount
        if allow_mpp && max_min_tlc_value > 0 {
            return Err(PathFindError::TlcMinValue(max_min_tlc_value));
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
    // This may makes the result of find_path is not a global optimized path.
    #[allow(clippy::too_many_arguments)]
    fn adjust_target_for_route_self(
        &self,
        source: Pubkey,
        amount: u128,
        expiry: u64,
        udt_type_script: &Option<Script>,
        tlc_expiry_limit: u64,
        channel_stats: &GraphChannelStat,
        allow_mpp: bool,
    ) -> Vec<(RouterHop, Pubkey, u64, u128)> {
        self.get_node_inbounds(source)
            .filter(|(from, _, channel_info, channel_update)| {
                udt_type_script == channel_info.udt_type_script()
                    && self.check_channel_amount_and_expiry(
                        amount,
                        channel_info,
                        channel_update,
                        channel_update.tlc_expiry_delta,
                        tlc_expiry_limit,
                        channel_info
                            .get_send_node(*from)
                            .expect("send_node should exist"),
                        channel_stats,
                        allow_mpp,
                    )
                    && (!allow_mpp || self.is_node_support_mpp(from))
            })
            .map(|(from, _, channel_info, channel_update)| {
                let last_edge = RouterHop {
                    target: source,
                    channel_outpoint: channel_info.channel_outpoint.clone(),
                    amount_received: amount,
                    incoming_tlc_expiry: expiry,
                };
                (
                    last_edge,
                    from,
                    channel_update.tlc_expiry_delta,
                    // we have already checked the fee rate and amount in check_channel_amount_and_expiry
                    calculate_tlc_forward_fee(amount, channel_update.fee_rate as u128)
                        .expect("already checked"),
                )
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn check_channel_amount_and_expiry(
        &self,
        amount: u128,
        channel_info: &ChannelInfo,
        channel_update: &ChannelUpdateInfo,
        incoming_tlc_expiry: u64,
        tlc_expiry_limit: u64,
        sent_node: SentNode,
        channel_stats: &GraphChannelStat,
        allow_mpp: bool,
    ) -> bool {
        if calculate_tlc_forward_fee(amount, channel_update.fee_rate as u128).is_err() {
            return false;
        }

        if channel_update.tlc_expiry_delta > DEFAULT_TLC_EXPIRY_DELTA {
            return false;
        }

        if amount > channel_info.capacity() {
            return false;
        }
        // We are finding path for a amount range for mpp, we don't check the tlc_minimum_value here,
        // but return proper error after the path is found
        if amount < channel_update.tlc_minimum_value && !allow_mpp {
            return false;
        }

        let sent_amount =
            channel_stats.get_channel_sent_amount(channel_info.out_point(), sent_node);

        if amount > channel_info.capacity().saturating_sub(sent_amount) {
            return false;
        }

        // If we already know the balance of the channel, check if we can send the amount.
        if let Some(balance) = channel_update.outbound_liquidity {
            debug_assert!(
                balance >= sent_amount,
                "balance {} is less than sent amount {}",
                balance,
                sent_amount
            );
            if amount > balance.saturating_sub(sent_amount) {
                return false;
            }
        }

        if incoming_tlc_expiry > tlc_expiry_limit {
            return false;
        }
        true
    }

    // Larger fee and htlc_expiry_delta makes edge_weight large,
    // which reduce the probability of choosing this edge,
    fn edge_weight(&self, amount: u128, fee: u128, htlc_expiry_delta: u64) -> u128 {
        // The factor is currently a fixed value, but might be configurable in the future,
        // lock 1% of amount with default tlc expiry delta.
        let risk_factor: f64 = 0.01;
        let time_lock_penalty = (amount as f64
            * (risk_factor * (htlc_expiry_delta as f64 / DEFAULT_TLC_EXPIRY_DELTA as f64)))
            as u128;
        fee + time_lock_penalty
    }

    fn calculate_distance_based_probability(&self, probability: f64, weight: u128) -> u128 {
        debug_assert!(probability > 0.0);
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

        let mut agg_tlc_expiry = final_tlc_expiry_delta.unwrap_or(DEFAULT_FINAL_TLC_EXPIRY_DELTA);
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
                            PathFindError::Overflow(format!(
                                "calculate_tlc_forward_fee error: {:?}",
                                err
                            ))
                        })?
                };
                amount_to_send += fee;
                if amount_to_send > channel_info.capacity() {
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
                let probability = if cur_hop.channel_outpoint.is_some() {
                    // If the channel outpoint is specified, we will assume that the channel is routable
                    // it's user's responsibility to ensure that the channel is routable.
                    1.0
                } else {
                    self.history.eval_probability(
                        from,
                        to,
                        &channel_outpoint,
                        amount_to_send,
                        channel_info.capacity(),
                    )
                };

                // we don't skip the edge if the probability is too low for build with router
                // but we can still find a optimised path if there are multiple channels
                let probability = probability.max(DEFAULT_MIN_PROBABILITY);

                debug!(
                    "probability: {} for channel_outpoint: {:?} from: {:?} => to: {:?}",
                    probability, channel_outpoint, from, to
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
                return Err(PathFindError::NoPathFound);
            }
        }
        path.reverse();
        Ok(path)
    }
}

pub trait NetworkGraphStateStore {
    fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession>;
    fn get_payment_sessions_with_status(&self, status: PaymentStatus) -> Vec<PaymentSession>;
    fn insert_payment_session(&self, session: PaymentSession);
    fn insert_payment_history_result(
        &mut self,
        channel_outpoint: OutPoint,
        direction: Direction,
        result: TimedResult,
    );
    fn remove_channel_history(&mut self, channel_outpoint: &OutPoint);
    fn get_payment_history_results(&self) -> Vec<(OutPoint, Direction, TimedResult)>;
    fn get_attempt(&self, payment_hash: Hash256, attempt_id: u64) -> Option<Attempt>;
    fn insert_attempt(&self, attempt: Attempt);
    fn get_attempts(&self, payment_hash: Hash256) -> Vec<Attempt>;
    fn delete_attempts(&self, payment_hash: Hash256);
    fn get_attempts_with_statuses(&self, status: &[AttemptStatus]) -> Vec<Attempt>;
}
