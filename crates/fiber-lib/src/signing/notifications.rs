//! Business-layer signing events. Persistence itself has no subscription side effects.

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use fiber_json_types::ChannelSigningStatus;
#[cfg(feature = "watchtower")]
use fiber_json_types::WatchtowerSigningStatus;
use fiber_types::{Hash256, NodeId};
use parking_lot::{Mutex, MutexGuard};
use tokio::sync::broadcast;

pub(crate) const SIGNING_EVENT_BUFFER: usize = 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SigningScope {
    Channel(Option<String>),
    Watchtower(NodeId),
}

/// Public signing projections only; no channel secrets or database handles.
#[derive(Clone, Debug)]
pub(crate) enum SigningChange {
    Channel {
        channel_id: Hash256,
        status: Result<ChannelSigningStatus, String>,
    },
    #[cfg(feature = "watchtower")]
    WatchtowerCreated {
        channel_id: Hash256,
        status: WatchtowerSigningStatus,
    },
    #[cfg(feature = "watchtower")]
    WatchtowerUpdated {
        channel_id: Hash256,
        status: WatchtowerSigningStatus,
    },
    Removed(Hash256),
}

impl SigningChange {
    pub(crate) fn channel_id(&self) -> Hash256 {
        match self {
            Self::Channel { channel_id, .. } | Self::Removed(channel_id) => *channel_id,
            #[cfg(feature = "watchtower")]
            Self::WatchtowerCreated { channel_id, .. }
            | Self::WatchtowerUpdated { channel_id, .. } => *channel_id,
        }
    }
}

#[derive(Default)]
pub(crate) struct SigningNotifications(Mutex<HashMap<SigningScope, Weak<SigningTopic>>>);

impl SigningNotifications {
    pub(crate) fn topic(self: &Arc<Self>, scope: SigningScope) -> Arc<SigningTopic> {
        let mut topics = self.0.lock();
        if let Some(topic) = topics.get(&scope).and_then(Weak::upgrade) {
            return topic;
        }
        let topic = Arc::new(SigningTopic {
            notifications: Arc::downgrade(self),
            scope: scope.clone(),
            sender: Mutex::new(None),
        });
        topics.insert(scope, Arc::downgrade(&topic));
        topic
    }

    pub(crate) fn subscribe(self: &Arc<Self>, scope: SigningScope) -> SigningChanges {
        let topic = self.topic(scope);
        let receiver = topic
            .sender
            .lock()
            .get_or_insert_with(|| broadcast::channel(SIGNING_EVENT_BUFFER).0)
            .subscribe();
        SigningChanges { topic, receiver }
    }
}

/// One topic/scope lock orders initial reads with commits and their publications.
/// Scopes allocate no broadcast buffer until subscribed, and disappear when the
/// last publisher or subscriber releases them.
pub(crate) struct SigningTopic {
    notifications: Weak<SigningNotifications>,
    scope: SigningScope,
    sender: Mutex<Option<broadcast::Sender<Arc<SigningChange>>>>,
}

impl SigningTopic {
    pub(crate) fn write(&self) -> SigningWrite<'_> {
        SigningWrite(self.sender.lock())
    }
}

impl Drop for SigningTopic {
    fn drop(&mut self) {
        if let Some(notifications) = self.notifications.upgrade() {
            let mut topics = notifications.0.lock();
            if topics
                .get(&self.scope)
                .is_some_and(|topic| std::ptr::eq(topic.as_ptr(), self))
            {
                topics.remove(&self.scope);
            }
        }
    }
}

pub(crate) struct SigningWrite<'a>(MutexGuard<'a, Option<broadcast::Sender<Arc<SigningChange>>>>);

impl SigningWrite<'_> {
    pub(crate) fn observed(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|sender| sender.receiver_count() != 0)
    }

    /// Build the projection once for all receivers, only when someone is listening.
    pub(crate) fn publish(&self, change: impl FnOnce() -> SigningChange) {
        if let Some(sender) = self
            .0
            .as_ref()
            .filter(|sender| sender.receiver_count() != 0)
        {
            let _ = sender.send(Arc::new(change()));
        }
    }
}

/// A scoped event receiver. Lag terminates the subscription instead of querying DB.
pub(crate) struct SigningChanges {
    topic: Arc<SigningTopic>,
    receiver: broadcast::Receiver<Arc<SigningChange>>,
}

impl SigningChanges {
    /// Start at a consistent boundary: discard pre-snapshot events while writes
    /// to this scope are excluded. No store or read closure is retained afterwards.
    pub(crate) fn snapshot<R>(&mut self, read: impl FnOnce() -> R) -> R {
        let _guard = self.topic.write();
        self.receiver = self.receiver.resubscribe();
        read()
    }

    pub(crate) async fn recv(&mut self) -> Result<Arc<SigningChange>, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

#[cfg(test)]
mod tests;
