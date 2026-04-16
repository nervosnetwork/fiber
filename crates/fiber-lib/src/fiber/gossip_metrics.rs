use crate::now_timestamp_as_millis_u64;

use super::{broadcast_message_type, BroadcastMessage, BroadcastMessageWithTimestamp};

pub(super) fn observe_gossip_received_bytes(bytes: usize) {
    metrics::counter!(crate::metrics::GOSSIP_RECEIVED_BYTES_TOTAL).increment(bytes as u64);
}

pub(super) fn observe_gossip_sent_bytes(bytes: usize) {
    metrics::counter!(crate::metrics::GOSSIP_SENT_BYTES_TOTAL).increment(bytes as u64);
}

pub(super) fn observe_received_broadcast_messages(count: usize) {
    metrics::counter!(crate::metrics::GOSSIP_RECEIVED_BROADCAST_MESSAGES_TOTAL)
        .increment(count as u64);
}

pub(super) fn observe_applied_broadcast_message() {
    metrics::counter!(crate::metrics::GOSSIP_APPLIED_BROADCAST_MESSAGES_TOTAL).increment(1);
}

pub(super) fn observe_duplicate_broadcast_message() {
    metrics::counter!(crate::metrics::GOSSIP_DUPLICATE_BROADCAST_MESSAGES_TOTAL).increment(1);
}

pub(super) fn observe_rejected_broadcast_message(reason: &'static str) {
    metrics::counter!(
        crate::metrics::GOSSIP_REJECTED_BROADCAST_MESSAGES_TOTAL,
        "reason" => reason
    )
    .increment(1);
}

fn observe_gossip_propagation_received_latency(
    message_type: &'static str,
    ingress: &'static str,
    latency_ms: u64,
) {
    metrics::histogram!(
        crate::metrics::GOSSIP_PROPAGATION_RECEIVED_LATENCY_MS,
        "message_type" => message_type,
        "ingress" => ingress
    )
    .record(latency_ms as f64);
}

fn observe_gossip_propagation_applied_latency(message_type: &'static str, latency_ms: u64) {
    metrics::histogram!(
        crate::metrics::GOSSIP_PROPAGATION_APPLIED_LATENCY_MS,
        "message_type" => message_type
    )
    .record(latency_ms as f64);
}

fn observe_gossip_propagation_sample_skipped(
    stage: &'static str,
    message_type: &'static str,
    reason: &'static str,
) {
    metrics::counter!(
        crate::metrics::GOSSIP_PROPAGATION_SAMPLES_SKIPPED_TOTAL,
        "stage" => stage,
        "message_type" => message_type,
        "reason" => reason
    )
    .increment(1);
}

pub(super) fn observe_received_propagation_latencies(
    messages: &[BroadcastMessage],
    ingress: &'static str,
) {
    let now = now_timestamp_as_millis_u64();
    for message in messages {
        let message_type = broadcast_message_type(message);
        match message.timestamp() {
            Some(message_timestamp) if message_timestamp <= now => {
                observe_gossip_propagation_received_latency(
                    message_type,
                    ingress,
                    now.saturating_sub(message_timestamp),
                );
            }
            Some(_) => {
                observe_gossip_propagation_sample_skipped(
                    "received",
                    message_type,
                    "future_timestamp",
                );
            }
            None => {
                observe_gossip_propagation_sample_skipped("received", message_type, "no_timestamp");
            }
        }
    }
}

pub(super) fn observe_applied_propagation_latency(message: &BroadcastMessageWithTimestamp) {
    let message_type = broadcast_message_with_timestamp_type(message);
    let message_timestamp = match message {
        BroadcastMessageWithTimestamp::ChannelUpdate(channel_update) => channel_update.timestamp,
        BroadcastMessageWithTimestamp::NodeAnnouncement(node_announcement) => {
            node_announcement.timestamp
        }
        BroadcastMessageWithTimestamp::ChannelAnnouncement(_, _) => {
            observe_gossip_propagation_sample_skipped(
                "applied",
                message_type,
                "unsupported_message_type",
            );
            return;
        }
    };

    let now = now_timestamp_as_millis_u64();
    if message_timestamp > now {
        observe_gossip_propagation_sample_skipped("applied", message_type, "future_timestamp");
        return;
    }
    observe_gossip_propagation_applied_latency(message_type, now.saturating_sub(message_timestamp));
}

pub(super) fn observe_active_sync_completion(latency_ms: u64) {
    metrics::histogram!(crate::metrics::GOSSIP_ACTIVE_SYNC_COMPLETION_MS).record(latency_ms as f64);
}

pub(super) fn observe_active_sync_started() {
    metrics::counter!(crate::metrics::GOSSIP_ACTIVE_SYNC_STARTED_TOTAL).increment(1);
}

pub(super) fn observe_active_sync_finished() {
    metrics::counter!(crate::metrics::GOSSIP_ACTIVE_SYNC_FINISHED_TOTAL).increment(1);
}

pub(super) fn observe_active_sync_timeout() {
    metrics::counter!(crate::metrics::GOSSIP_ACTIVE_SYNC_TIMEOUT_TOTAL).increment(1);
}

pub(super) fn observe_missing_dependency_message(message_type: &'static str) {
    metrics::counter!(
        crate::metrics::GOSSIP_MISSING_DEPENDENCY_MESSAGES_TOTAL,
        "message_type" => message_type
    )
    .increment(1);
}

pub(super) fn observe_dependency_query_request(items: usize) {
    metrics::counter!(crate::metrics::GOSSIP_DEPENDENCY_QUERY_REQUESTS_TOTAL).increment(1);
    metrics::counter!(crate::metrics::GOSSIP_DEPENDENCY_QUERY_ITEMS_TOTAL).increment(items as u64);
}

fn broadcast_message_with_timestamp_type(message: &BroadcastMessageWithTimestamp) -> &'static str {
    match message {
        BroadcastMessageWithTimestamp::ChannelAnnouncement(_, _) => "channel_announcement",
        BroadcastMessageWithTimestamp::ChannelUpdate(_) => "channel_update",
        BroadcastMessageWithTimestamp::NodeAnnouncement(_) => "node_announcement",
    }
}
