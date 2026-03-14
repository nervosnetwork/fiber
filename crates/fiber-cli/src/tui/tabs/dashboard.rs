//! Dashboard tab: overview of node status, channel stats, and capacity utilization.

use std::collections::HashMap;

use fiber_json_types::{
    AssetFeeReport, Channel, ChannelInfo, FeeReportResult, ForwardingEventInfo,
    ForwardingHistoryResult, NodeInfo as GraphNodeInfo, NodeInfoResult,
};

/// Aggregated stats computed client-side from channel data + node info.
#[derive(Debug, Clone, Default)]
pub struct DashboardStats {
    /// Total local balance across all channels (shannons).
    pub total_local_balance: u128,
    /// Total remote balance across all channels (shannons).
    pub total_remote_balance: u128,
    /// Total capacity = local + remote for all channels.
    pub total_capacity: u128,
    /// Number of channels in Ready state.
    pub ready_count: usize,
    /// Number of channels in a negotiating/pending state.
    pub pending_count: usize,
    /// Number of channels shutting down.
    pub shutting_down_count: usize,
    /// Number of closed channels.
    pub closed_count: usize,
    /// Number of enabled channels.
    pub enabled_count: usize,
    /// Number of disabled channels (Ready but not enabled).
    pub disabled_count: usize,
    /// Total pending TLCs across all channels.
    pub total_pending_tlcs: usize,
    /// Total offered TLC balance.
    pub total_offered_tlc: u128,
    /// Total received TLC balance.
    pub total_received_tlc: u128,
    /// Total number of channels.
    pub total_channels: usize,
}

/// Aggregated network-wide stats from the graph.
#[derive(Debug, Clone, Default)]
pub struct NetworkStats {
    /// Total number of nodes in the network graph.
    pub total_nodes: usize,
    /// Total number of channels in the network graph.
    pub total_channels: usize,
    /// Total capacity across all graph channels (shannons).
    pub total_capacity: u128,
    /// Number of graph channels with at least one direction enabled.
    pub active_channels: usize,
}

/// A peer connection visible from the topology (for the adjacency list view).
#[derive(Debug, Clone)]
pub struct PeerConnection {
    /// Display label for the peer node.
    pub label: String,
    /// Number of channels between these two nodes.
    pub channel_count: usize,
    /// Total capacity across those channels (shannons).
    pub capacity: u128,
    /// Number of active channels (at least one direction enabled).
    pub active_count: usize,
}

/// A connection between two non-self nodes (for "Other connections" section).
#[derive(Debug, Clone)]
pub struct OtherConnection {
    /// Display label for node A.
    pub label_a: String,
    /// Display label for node B.
    pub label_b: String,
    /// Number of channels.
    pub channel_count: usize,
    /// Total capacity (shannons).
    pub capacity: u128,
    /// Number of active channels.
    pub active_count: usize,
}

/// Fee statistics derived from the fee_report and forwarding_history RPCs.
#[derive(Debug, Clone, Default)]
pub struct FeeStats {
    /// CKB fee report (daily/weekly/monthly sums and event counts).
    pub ckb_report: Option<AssetFeeReport>,
    /// UDT fee reports (one per UDT asset type).
    pub udt_reports: Vec<AssetFeeReport>,
    /// Recent forwarding events (most recent first, capped).
    pub recent_events: Vec<ForwardingEventInfo>,
    /// Total forwarding event count from the server.
    pub total_event_count: u64,
}

/// Dashboard tab state.
pub struct DashboardTab {
    pub stats: DashboardStats,
    /// Fee and forwarding statistics.
    pub fee_stats: FeeStats,
    /// Network-wide stats derived from graph data.
    pub network_stats: NetworkStats,
    /// Label for the self node (empty if not found in graph).
    pub self_label: String,
    /// Total degree (channel count) of the self node.
    pub self_degree: usize,
    /// Direct peers of the self node, sorted by capacity descending.
    pub self_peers: Vec<PeerConnection>,
    /// Connections between non-self nodes, sorted by capacity descending.
    pub other_connections: Vec<OtherConnection>,
}

impl DashboardTab {
    pub fn new() -> Self {
        Self {
            stats: DashboardStats::default(),
            fee_stats: FeeStats::default(),
            network_stats: NetworkStats::default(),
            self_label: String::new(),
            self_degree: 0,
            self_peers: Vec::new(),
            other_connections: Vec::new(),
        }
    }

    /// Recompute aggregate stats from the given channels and node info.
    pub fn update_stats(&mut self, channels: &[Channel], _node_info: Option<&NodeInfoResult>) {
        let mut stats = DashboardStats {
            total_channels: channels.len(),
            ..DashboardStats::default()
        };

        for ch in channels {
            stats.total_local_balance += ch.local_balance;
            stats.total_remote_balance += ch.remote_balance;
            stats.total_offered_tlc += ch.offered_tlc_balance;
            stats.total_received_tlc += ch.received_tlc_balance;
            stats.total_pending_tlcs += ch.pending_tlcs.len();

            let state_name = channel_state_category(ch);
            match state_name {
                StateCategory::Ready => {
                    stats.ready_count += 1;
                    if ch.enabled {
                        stats.enabled_count += 1;
                    } else {
                        stats.disabled_count += 1;
                    }
                }
                StateCategory::Pending => stats.pending_count += 1,
                StateCategory::ShuttingDown => stats.shutting_down_count += 1,
                StateCategory::Closed => stats.closed_count += 1,
            }
        }

        stats.total_capacity = stats.total_local_balance + stats.total_remote_balance;
        self.stats = stats;
    }

    /// Update fee statistics from fee_report and forwarding_history RPC results.
    pub fn update_fee_stats(
        &mut self,
        fee_report: Option<&FeeReportResult>,
        forwarding_history: Option<&ForwardingHistoryResult>,
    ) {
        let mut fee_stats = FeeStats::default();

        if let Some(report) = fee_report {
            for asset_report in &report.asset_reports {
                if asset_report.udt_type_script.is_none() {
                    fee_stats.ckb_report = Some(asset_report.clone());
                } else {
                    fee_stats.udt_reports.push(asset_report.clone());
                }
            }
        }

        if let Some(history) = forwarding_history {
            fee_stats.recent_events = history.events.clone();
            fee_stats.total_event_count = history.total_count;
        }

        self.fee_stats = fee_stats;
    }

    /// Update network-wide stats and adjacency data from graph data.
    pub fn update_network_stats(
        &mut self,
        graph_nodes: &[GraphNodeInfo],
        graph_channels: &[ChannelInfo],
        own_pubkey: Option<&str>,
    ) {
        // ── Stats ───────────────────────────────────────────────────
        let mut net = NetworkStats {
            total_nodes: graph_nodes.len(),
            total_channels: graph_channels.len(),
            ..NetworkStats::default()
        };
        for ch in graph_channels {
            net.total_capacity += ch.capacity;
            let node1_enabled = ch.update_info_of_node1.as_ref().is_some_and(|u| u.enabled);
            let node2_enabled = ch.update_info_of_node2.as_ref().is_some_and(|u| u.enabled);
            if node1_enabled || node2_enabled {
                net.active_channels += 1;
            }
        }
        self.network_stats = net;

        // ── Build node labels map ───────────────────────────────────
        let mut pubkey_to_label: HashMap<String, String> = HashMap::new();
        let mut self_pk: Option<String> = None;

        for node in graph_nodes {
            let pk = format!("{}", node.pubkey);
            let is_self = own_pubkey.is_some_and(|own| own == pk);
            let label = if is_self {
                if node.node_name.is_empty() {
                    "me".to_string()
                } else {
                    node.node_name.chars().take(12).collect::<String>()
                }
            } else if !node.node_name.is_empty() {
                node.node_name.chars().take(10).collect()
            } else {
                truncate_pubkey(&pk)
            };
            if is_self {
                self_pk = Some(pk.clone());
                self.self_label = label.clone();
            }
            pubkey_to_label.insert(pk, label);
        }

        // ── Build per-pair aggregation ──────────────────────────────
        // Key: (min_pk, max_pk) to deduplicate direction.
        struct PairAgg {
            channel_count: usize,
            capacity: u128,
            active_count: usize,
        }

        let mut pairs: HashMap<(String, String), PairAgg> = HashMap::new();
        for ch in graph_channels {
            let pk1 = format!("{}", ch.node1);
            let pk2 = format!("{}", ch.node2);
            let active = ch.update_info_of_node1.as_ref().is_some_and(|u| u.enabled)
                || ch.update_info_of_node2.as_ref().is_some_and(|u| u.enabled);
            let key = if pk1 <= pk2 { (pk1, pk2) } else { (pk2, pk1) };
            let entry = pairs.entry(key).or_insert(PairAgg {
                channel_count: 0,
                capacity: 0,
                active_count: 0,
            });
            entry.channel_count += 1;
            entry.capacity += ch.capacity;
            if active {
                entry.active_count += 1;
            }
        }

        // ── Partition into self-peers vs other connections ───────────
        let mut self_peers: Vec<PeerConnection> = Vec::new();
        let mut other_connections: Vec<OtherConnection> = Vec::new();
        let mut self_total_channels = 0usize;

        for ((pk_a, pk_b), agg) in &pairs {
            let is_self_a = self_pk.as_ref().is_some_and(|s| s == pk_a);
            let is_self_b = self_pk.as_ref().is_some_and(|s| s == pk_b);

            if is_self_a || is_self_b {
                let peer_pk = if is_self_a { pk_b } else { pk_a };
                let label = pubkey_to_label
                    .get(peer_pk)
                    .cloned()
                    .unwrap_or_else(|| truncate_pubkey(peer_pk));
                self_peers.push(PeerConnection {
                    label,
                    channel_count: agg.channel_count,
                    capacity: agg.capacity,
                    active_count: agg.active_count,
                });
                self_total_channels += agg.channel_count;
            } else {
                let label_a = pubkey_to_label
                    .get(pk_a)
                    .cloned()
                    .unwrap_or_else(|| truncate_pubkey(pk_a));
                let label_b = pubkey_to_label
                    .get(pk_b)
                    .cloned()
                    .unwrap_or_else(|| truncate_pubkey(pk_b));
                other_connections.push(OtherConnection {
                    label_a,
                    label_b,
                    channel_count: agg.channel_count,
                    capacity: agg.capacity,
                    active_count: agg.active_count,
                });
            }
        }

        // Sort by capacity descending.
        self_peers.sort_by(|a, b| b.capacity.cmp(&a.capacity));
        other_connections.sort_by(|a, b| b.capacity.cmp(&a.capacity));

        self.self_degree = self_total_channels;
        self.self_peers = self_peers;
        self.other_connections = other_connections;
    }
}

/// Broad state categories for aggregation.
enum StateCategory {
    Ready,
    Pending,
    ShuttingDown,
    Closed,
}

fn channel_state_category(ch: &Channel) -> StateCategory {
    match &ch.state {
        fiber_json_types::ChannelState::ChannelReady => StateCategory::Ready,
        fiber_json_types::ChannelState::ShuttingDown(_) => StateCategory::ShuttingDown,
        fiber_json_types::ChannelState::Closed(_) => StateCategory::Closed,
        _ => StateCategory::Pending,
    }
}

/// Shorten a pubkey hex string to first 4 + last 4 chars.
fn truncate_pubkey(pk: &str) -> String {
    if pk.len() <= 10 {
        pk.to_string()
    } else {
        format!("{}..{}", &pk[..4], &pk[pk.len() - 4..])
    }
}
