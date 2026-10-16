//! Coalesced tenant wakeups from business actors after signing state is persisted.

use std::{collections::HashMap, sync::Arc};

use fiber_types::NodeId;
use parking_lot::Mutex;
use tokio::sync::watch;

#[derive(Debug, Default)]
pub(crate) struct TenantEvents(Mutex<HashMap<NodeId, watch::Sender<()>>>);

impl TenantEvents {
    pub(crate) fn subscribe(self: &Arc<Self>, node: &NodeId) -> TenantEventSubscription {
        let mut senders = self.0.lock();
        let receiver = senders
            .entry(node.clone())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe();
        TenantEventSubscription {
            events: self.clone(),
            node: node.clone(),
            receiver: Some(receiver),
        }
    }

    pub(crate) fn notify(&self, node: &NodeId) {
        if let Some(sender) = self.0.lock().get(node) {
            sender.send_replace(());
        }
    }
}

/// The last subscriber removes its registration rather than retaining idle tenants.
pub(crate) struct TenantEventSubscription {
    events: Arc<TenantEvents>,
    node: NodeId,
    receiver: Option<watch::Receiver<()>>,
}

impl TenantEventSubscription {
    pub(crate) async fn changed(&mut self) {
        if let Some(receiver) = &mut self.receiver {
            let _ = receiver.changed().await;
        }
    }
}

impl Drop for TenantEventSubscription {
    fn drop(&mut self) {
        // Remove the receiver first so concurrent final drops cannot both
        // observe the other's receiver and leave an unused sender behind.
        drop(self.receiver.take());
        let mut senders = self.events.0.lock();
        if senders
            .get(&self.node)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            senders.remove(&self.node);
        }
    }
}
