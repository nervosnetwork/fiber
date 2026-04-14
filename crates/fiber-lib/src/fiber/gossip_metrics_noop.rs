use super::{BroadcastMessage, BroadcastMessageWithTimestamp};

pub(super) fn observe_gossip_received_bytes(_bytes: usize) {}

pub(super) fn observe_gossip_sent_bytes(_bytes: usize) {}

pub(super) fn observe_received_broadcast_messages(_count: usize) {}

pub(super) fn observe_applied_broadcast_message() {}

pub(super) fn observe_duplicate_broadcast_message() {}

pub(super) fn observe_rejected_broadcast_message(_reason: &'static str) {}

pub(super) fn observe_received_propagation_latencies(
    _messages: &[BroadcastMessage],
    _ingress: &'static str,
) {
}

pub(super) fn observe_applied_propagation_latency(_message: &BroadcastMessageWithTimestamp) {}

pub(super) fn observe_active_sync_completion(_latency_ms: u64) {}

pub(super) fn observe_active_sync_started() {}

pub(super) fn observe_active_sync_finished() {}

pub(super) fn observe_active_sync_timeout() {}

pub(super) fn observe_missing_dependency_message(_message_type: &'static str) {}

pub(super) fn observe_dependency_query_request(_items: usize) {}
