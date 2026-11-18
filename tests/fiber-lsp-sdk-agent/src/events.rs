//! WebSocket subscriptions with pending-state recovery after reconnect.

use std::{future::pending, time::Duration};

use anyhow::{Context, Result};
use fiber_json_types::{SubscriptionEvent, SubscriptionTopic};
use jsonrpsee::{
    core::client::{Subscription, SubscriptionClientT},
    rpc_params,
    ws_client::{HeaderMap, HeaderValue, WsClientBuilder},
};
use tokio::sync::{mpsc, oneshot};

/// An update acknowledged after the signer processes it; failure triggers pending-state recovery.
pub struct EventDelivery {
    /// The typed update received from the node.
    pub event: SubscriptionEvent,
    /// Report processing success, or request recovery by sending false.
    pub processed: oneshot::Sender<bool>,
}

/// The agent's channel and watchtower subscriptions on a single authenticated connection.
pub struct EventStream {
    receiver: mpsc::Receiver<EventDelivery>,
    task: tokio::task::JoinHandle<()>,
}

impl EventStream {
    /// Subscribe to both signing topics; the node automatically resends pending requests.
    pub fn connect(rpc_url: &str, token: String) -> Result<Self> {
        let url = reqwest::Url::parse(rpc_url)?;
        if !matches!(url.scheme(), "ws" | "wss") {
            anyhow::bail!("expected dedicated ws:// or wss:// RPC URL");
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        let (sender, receiver) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_millis(200);
            loop {
                let started = tokio::time::Instant::now();
                let result = tokio::select! {
                    _ = sender.closed() => return,
                    result = receive(url.as_str(), headers.clone(), &sender) => result,
                };
                if let Err(error) = &result {
                    if error.downcast_ref::<jsonrpsee::core::ClientError>().is_some_and(|error| matches!(error, jsonrpsee::core::ClientError::Call(error) if error.message() == "Unauthorized")) {
                        tracing::warn!("subscription credentials rejected; stopping signer reader");
                        return;
                    }
                    tracing::debug!("signer subscription reconnecting: {error}");
                }
                if started.elapsed() >= Duration::from_secs(30) {
                    backoff = Duration::from_millis(200);
                }
                let jitter =
                    Duration::from_millis(rand::random_range(0..=backoff.as_millis() as u64));
                tokio::select! {
                    _ = sender.closed() => return,
                    _ = tokio::time::sleep(backoff + jitter) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        });
        Ok(Self { receiver, task })
    }

    /// Receive the next update, or None after the reader stops.
    pub async fn next(&mut self) -> Option<EventDelivery> {
        self.receiver.recv().await
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn deliver(sender: &mpsc::Sender<EventDelivery>, event: SubscriptionEvent) -> Result<()> {
    let (processed, reply) = oneshot::channel();
    sender.send(EventDelivery { event, processed }).await?;
    if !reply.await.unwrap_or(false) {
        anyhow::bail!("signer could not process update; reload pending requests");
    }
    Ok(())
}

async fn receive(
    url: &str,
    headers: HeaderMap,
    sender: &mpsc::Sender<EventDelivery>,
) -> Result<()> {
    let client = WsClientBuilder::default()
        .set_headers(headers)
        .connection_timeout(Duration::from_secs(10))
        .request_timeout(Duration::from_secs(10))
        .build(url)
        .await?;
    let mut channel: Subscription<SubscriptionEvent> = client
        .subscribe(
            "subscribe",
            rpc_params![SubscriptionTopic::ChannelSigning],
            "unsubscribe",
        )
        .await?;
    let mut watchtower: Option<Subscription<SubscriptionEvent>> = match client
        .subscribe(
            "subscribe",
            rpc_params![SubscriptionTopic::WatchtowerSigning],
            "unsubscribe",
        )
        .await
    {
        Ok(subscription) => Some(subscription),
        Err(jsonrpsee::core::ClientError::Call(error))
            if error.message() == "subscription topic is disabled" =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    loop {
        let event = tokio::select! {
            event = channel.next() => event,
            event = async { match watchtower.as_mut() { Some(subscription) => subscription.next().await, None => pending().await } } => event,
        };
        deliver(sender, event.context("signer subscription closed")??).await?;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use jsonrpsee::{
        core::SubscriptionError,
        server::{ServerBuilder, ServerHandle},
        RpcModule,
    };

    use super::{EventStream, SubscriptionEvent, SubscriptionTopic};

    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn server(
        watchtower: bool,
        deny: bool,
    ) -> (String, ServerHandle, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", server.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();
        let observed_active = active.clone();
        let mut module = RpcModule::new(());
        module
            .register_subscription(
                "subscribe",
                "subscribe",
                "unsubscribe",
                move |params, pending, _, _| {
                    let calls = observed_calls.clone();
                    let active = observed_active.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let (topic,): (SubscriptionTopic,) = params.parse().unwrap();
                        if deny || (!watchtower && topic == SubscriptionTopic::WatchtowerSigning) {
                            let reason = if deny {
                                "Unauthorized"
                            } else {
                                "subscription topic is disabled"
                            };
                            pending
                                .reject(jsonrpsee::types::ErrorObjectOwned::owned(
                                    -32001, reason, None::<()>,
                                ))
                                .await;
                            return Ok::<(), SubscriptionError>(());
                        }
                        active.fetch_add(1, Ordering::SeqCst);
                        let _active = Active(active);
                        let sink = pending.accept().await?;
                        sink.send(
                            serde_json::value::to_raw_value(&SubscriptionEvent::Removed {
                                channel_id: fiber_json_types::Hash256([42; 32]),
                            })
                            .unwrap(),
                        )
                        .await?;
                        sink.closed().await;
                        Ok(())
                    }
                },
            )
            .unwrap();
        (url, server.start(module), calls, active)
    }

    #[tokio::test]
    async fn subscription_reader_reconnects_after_failed_processing_and_releases_both_topics() {
        use std::time::Duration;
        let (url, handle, calls, active) = server(true, false).await;
        let mut reader = EventStream::connect(&url, "tenant-token".into()).unwrap();
        for success in [false, true] {
            let event = tokio::time::timeout(Duration::from_secs(5), reader.next())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(event.event, SubscriptionEvent::Removed { .. }));
            event.processed.send(success).unwrap();
        }
        assert!(calls.load(Ordering::SeqCst) >= 4);
        drop(reader);
        tokio::time::timeout(Duration::from_secs(5), async {
            while active.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        handle.stop().unwrap();
    }

    #[tokio::test]
    async fn subscription_reader_handles_disabled_watchtower_and_stops_on_rejected_credentials() {
        use std::time::Duration;
        let (url, handle, _, _) = server(false, false).await;
        let mut reader = EventStream::connect(&url, "tenant-token".into()).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), reader.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event.event, SubscriptionEvent::Removed { .. }));
        event.processed.send(true).unwrap();
        drop(reader);
        handle.stop().unwrap();

        let (url, handle, calls, _) = server(true, true).await;
        let mut reader = EventStream::connect(&url, "expired-token".into()).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), reader.next())
            .await
            .unwrap()
            .is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        handle.stop().unwrap();
    }
}
