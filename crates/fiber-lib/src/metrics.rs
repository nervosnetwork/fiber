use std::net::ToSocketAddrs;
use tracing::error;

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

    metrics_exporter_prometheus::PrometheusBuilder::new()
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
