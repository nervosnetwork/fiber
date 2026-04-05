use std::net::ToSocketAddrs;
use tracing::error;

/// READY_CHANNEL_COUNT + SHUTTING_DOWN_CHANNEL_COUNT
pub const TOTAL_CHANNEL_COUNT: &str = "fiber.total_channel_count";
pub const READY_CHANNEL_COUNT: &str = "fiber.ready_channel_count";
pub const SHUTTING_DOWN_CHANNEL_COUNT: &str = "fiber.shutting_down_channel_count";
pub const TOTAL_PEER_COUNT: &str = "fiber.total_peer_count";
pub const INBOUND_PEER_COUNT: &str = "fiber.inbound_peer_count";
pub const OUTBOUND_PEER_COUNT: &str = "fiber.outbound_peer_count";
pub const DOWN_WITH_CHANNEL_PEER_COUNT: &str = "fiber.down_with_channel_peer_count";
pub const SEND_PAYMENT_FIND_PATH_COUNT: &str = "fiber.send_payment_find_path_count";
pub const INFLIGHT_PAYMENTS_COUNT: &str = "fiber.inflight_payments_count";
pub const GOSSIP_RECEIVED_BROADCAST_MESSAGES_TOTAL: &str =
    "fiber.gossip.received_broadcast_messages_total";
pub const GOSSIP_APPLIED_BROADCAST_MESSAGES_TOTAL: &str =
    "fiber.gossip.applied_broadcast_messages_total";
pub const GOSSIP_DUPLICATE_BROADCAST_MESSAGES_TOTAL: &str =
    "fiber.gossip.duplicate_broadcast_messages_total";
pub const GOSSIP_REJECTED_BROADCAST_MESSAGES_TOTAL: &str =
    "fiber.gossip.rejected_broadcast_messages_total";
pub const GOSSIP_RECEIVED_BYTES_TOTAL: &str = "fiber.gossip.received_bytes_total";
pub const GOSSIP_SENT_BYTES_TOTAL: &str = "fiber.gossip.sent_bytes_total";
pub const GOSSIP_PROPAGATION_RECEIVED_LATENCY_MS: &str =
    "fiber.gossip.propagation_received_latency_ms";
pub const GOSSIP_PROPAGATION_APPLIED_LATENCY_MS: &str =
    "fiber.gossip.propagation_applied_latency_ms";
pub const GOSSIP_PROPAGATION_SAMPLES_SKIPPED_TOTAL: &str =
    "fiber.gossip.propagation_samples_skipped_total";
pub const GOSSIP_ACTIVE_SYNC_COMPLETION_MS: &str = "fiber.gossip.active_sync_completion_ms";
pub const GOSSIP_ACTIVE_SYNC_STARTED_TOTAL: &str = "fiber.gossip.active_sync_started_total";
pub const GOSSIP_ACTIVE_SYNC_FINISHED_TOTAL: &str = "fiber.gossip.active_sync_finished_total";
pub const GOSSIP_ACTIVE_SYNC_TIMEOUT_TOTAL: &str = "fiber.gossip.active_sync_timeout_total";
pub const GOSSIP_MISSING_DEPENDENCY_MESSAGES_TOTAL: &str =
    "fiber.gossip.missing_dependency_messages_total";
pub const GOSSIP_DEPENDENCY_QUERY_REQUESTS_TOTAL: &str =
    "fiber.gossip.dependency_query_requests_total";
pub const GOSSIP_DEPENDENCY_QUERY_ITEMS_TOTAL: &str = "fiber.gossip.dependency_query_items_total";

// Buckets tuned for gossip propagation/sync latency in milliseconds.
const GOSSIP_LATENCY_HISTOGRAM_BUCKETS_MS: &[f64] = &[
    1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0,
    20_000.0, 60_000.0,
];

pub const CCH_LND_TRACKER_INVOICE_QUEUE_LEN: &str = "fiber.cch.lnd_tracker.invoice_queue_len";
pub const CCH_LND_TRACKER_ACTIVE_INVOICE_TRACKERS: &str =
    "fiber.cch.lnd_tracker.active_invoice_trackers";

pub fn start_metrics(metrics_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let socket_addr = metrics_addr
        .to_socket_addrs()
        .map_err(|e| {
            error!("Invalid metrics address '{}': {}", metrics_addr, e);
            e
        })?
        .next()
        .ok_or_else(|| {
            let err = std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("No valid socket address found for '{}'", metrics_addr),
            );
            error!("{}", err);
            err
        })?;

    let prometheus_builder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(
                GOSSIP_PROPAGATION_RECEIVED_LATENCY_MS.to_owned(),
            ),
            GOSSIP_LATENCY_HISTOGRAM_BUCKETS_MS,
        )
        .map_err(|e| {
            error!(
                "Failed to configure Prometheus histogram buckets for '{}': {}",
                GOSSIP_PROPAGATION_RECEIVED_LATENCY_MS, e
            );
            Box::new(e) as Box<dyn std::error::Error>
        })?
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(
                GOSSIP_PROPAGATION_APPLIED_LATENCY_MS.to_owned(),
            ),
            GOSSIP_LATENCY_HISTOGRAM_BUCKETS_MS,
        )
        .map_err(|e| {
            error!(
                "Failed to configure Prometheus histogram buckets for '{}': {}",
                GOSSIP_PROPAGATION_APPLIED_LATENCY_MS, e
            );
            Box::new(e) as Box<dyn std::error::Error>
        })?
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(GOSSIP_ACTIVE_SYNC_COMPLETION_MS.to_owned()),
            GOSSIP_LATENCY_HISTOGRAM_BUCKETS_MS,
        )
        .map_err(|e| {
            error!(
                "Failed to configure Prometheus histogram buckets for '{}': {}",
                GOSSIP_ACTIVE_SYNC_COMPLETION_MS, e
            );
            Box::new(e) as Box<dyn std::error::Error>
        })?;

    prometheus_builder
        .with_http_listener(socket_addr)
        .install()
        .map_err(|e| {
            error!("Failed to install Prometheus metrics exporter: {}", e);
            Box::new(e) as Box<dyn std::error::Error>
        })?;

    tokio::task::spawn(
        tokio_metrics::RuntimeMetricsReporterBuilder::default()
            // the default metric sampling interval is 30 seconds, which is
            // too long for quick tests, so have it be 1 second.
            .with_interval(std::time::Duration::from_secs(1))
            .describe_and_run(),
    );

    Ok(())
}
