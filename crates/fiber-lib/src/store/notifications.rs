//! Coalesced, namespace-bound wakeups after signing state is committed.

use std::{collections::HashMap, sync::Arc};

use fiber_types::NodeId;
use parking_lot::Mutex;
use tokio::sync::watch;

use super::NodeNamespace;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SigningScope {
    Channel(Option<NodeNamespace>),
    Watchtower(NodeId),
}

#[derive(Default)]
pub(crate) struct SigningNotifications(Mutex<HashMap<SigningScope, watch::Sender<()>>>);

impl SigningNotifications {
    pub(crate) fn subscribe(self: &Arc<Self>, scope: SigningScope) -> SigningChanges {
        let receiver = self
            .0
            .lock()
            .entry(scope.clone())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe();
        SigningChanges {
            notifications: self.clone(),
            scope,
            receiver: Some(receiver),
        }
    }

    pub(crate) fn notify(&self, scope: SigningScope) {
        if let Some(sender) = self.0.lock().get(&scope) {
            sender.send_replace(());
        }
    }
}

/// A scoped listener that coalesces changes until its reader reloads current state.
pub struct SigningChanges {
    notifications: Arc<SigningNotifications>,
    scope: SigningScope,
    receiver: Option<watch::Receiver<()>>,
}

impl SigningChanges {
    pub(crate) async fn changed(&mut self) {
        if let Some(receiver) = &mut self.receiver {
            let _ = receiver.changed().await;
        }
    }
}

impl Drop for SigningChanges {
    fn drop(&mut self) {
        drop(self.receiver.take());
        let mut senders = self.notifications.0.lock();
        if senders
            .get(&self.scope)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            senders.remove(&self.scope);
        }
    }
}

/// Store notification boundary used by RPC subscriptions; no persisted schema changes.
pub trait SigningSubscriptionStore {
    /// Listen for channel signing changes in this store's logical namespace.
    fn subscribe_channel_signing(&self) -> SigningChanges;
    /// Listen for watchtower signing changes owned by the authorized node.
    fn subscribe_watchtower_signing(&self, node: &NodeId) -> SigningChanges;
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use fiber_types::NodeId;

    use crate::store::NodeNamespace;

    use super::{SigningNotifications, SigningScope};

    #[tokio::test]
    async fn signing_wakeups_are_scoped_coalesced_and_cleaned_up() {
        let hub = Arc::new(SigningNotifications::default());
        let alice = SigningScope::Channel(Some(NodeNamespace::hosted_tenant("alice")));
        let bob = SigningScope::Channel(Some(NodeNamespace::hosted_tenant("bob")));
        let host = SigningScope::Channel(None);
        let watchtower = SigningScope::Watchtower(NodeId::local());
        let mut first = hub.subscribe(alice.clone());
        let mut second = hub.subscribe(alice.clone());
        let mut others = [
            hub.subscribe(bob),
            hub.subscribe(host),
            hub.subscribe(watchtower),
        ];
        for _ in 0..100 {
            hub.notify(alice.clone());
        }
        for subscription in [&mut first, &mut second] {
            tokio::time::timeout(Duration::from_secs(1), subscription.changed())
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(20), subscription.changed())
                    .await
                    .is_err()
            );
        }
        for subscription in &mut others {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), subscription.changed())
                    .await
                    .is_err()
            );
        }
        drop(first);
        assert_eq!(hub.0.lock().len(), 4);
        drop(second);
        drop(others);
        assert!(hub.0.lock().is_empty());
    }
}
