//! Forward a typed stream using jsonrpsee's bounded output queue and cancellation.

use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use jsonrpsee::SubscriptionSink;

use super::service::EventStream;

pub(super) async fn forward(sink: SubscriptionSink, stream: EventStream) -> Result<()> {
    tokio::select! {
        _ = sink.closed() => Ok(()),
        result = stream.try_for_each(|event| {
            let sink = &sink;
            async move {
                let message = serde_json::value::to_raw_value(&event)?;
                sink.send_timeout(message, Duration::from_secs(5)).await.map_err(|_| {
                    anyhow!("subscription queue full or closed; reconnect to recover pending requests")
                })?;
                Ok(())
            }
        }) => result,
    }
}
