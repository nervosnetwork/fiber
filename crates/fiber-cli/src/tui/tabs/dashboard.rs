//! Dashboard tab: overview of node status, channel stats, and capacity utilization.

use fiber_json_types::{Channel, NodeInfoResult};

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

/// Maximum number of sparkline data points to keep.
const SPARKLINE_MAX_POINTS: usize = 60;

/// Dashboard tab state.
pub struct DashboardTab {
    pub stats: DashboardStats,
    /// Historical channel counts for sparkline (most recent at end).
    pub channel_history: Vec<u64>,
    /// Historical capacity data for sparkline (most recent at end, in CKB units).
    pub capacity_history: Vec<u64>,
}

impl DashboardTab {
    pub fn new() -> Self {
        Self {
            stats: DashboardStats::default(),
            channel_history: Vec::new(),
            capacity_history: Vec::new(),
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

        // Update sparkline history
        self.channel_history.push(self.stats.total_channels as u64);
        if self.channel_history.len() > SPARKLINE_MAX_POINTS {
            self.channel_history
                .drain(..self.channel_history.len() - SPARKLINE_MAX_POINTS);
        }
        // Capacity in CKB (whole units) for sparkline
        let capacity_ckb = (self.stats.total_capacity / 100_000_000) as u64;
        self.capacity_history.push(capacity_ckb);
        if self.capacity_history.len() > SPARKLINE_MAX_POINTS {
            self.capacity_history
                .drain(..self.capacity_history.len() - SPARKLINE_MAX_POINTS);
        }
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
