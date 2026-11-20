//! Forward a typed stream using jsonrpsee's bounded output queue and cancellation.

use std::time::Duration;

use anyhow::{anyhow, Result};
use fiber_types::TenantId;
use futures::TryStreamExt;
use jsonrpsee::SubscriptionSink;

use crate::rpc::biscuit::TenantTokenEpochs;

use super::service::EventStream;

/// Live revocation watcher for a tenant-scoped WebSocket subscription.
pub(super) struct RevocationWatch {
    tenant_id: TenantId,
    token_epoch: u64,
    epochs: TenantTokenEpochs,
    rx: tokio::sync::watch::Receiver<u64>,
}

impl RevocationWatch {
    pub(super) fn new(tenant_id: TenantId, token_epoch: u64, epochs: TenantTokenEpochs) -> Self {
        let rx = epochs.subscribe_revocations();
        Self {
            tenant_id,
            token_epoch,
            epochs,
            rx,
        }
    }

    async fn wait_revoked(mut self) {
        loop {
            if self.epochs.get(&self.tenant_id) > self.token_epoch {
                return;
            }
            if self.rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

pub(super) async fn forward(
    sink: SubscriptionSink,
    stream: EventStream,
    revocation: Option<RevocationWatch>,
) -> Result<()> {
    let revoked = async {
        match revocation {
            Some(watch) => watch.wait_revoked().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = sink.closed() => Ok(()),
        _ = revoked => Err(anyhow!("tenant token revoked")),
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
