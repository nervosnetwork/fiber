//! Application-owned signing event channels, shared by business actors and RPC.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod notifications;

use fiber_types::Hash256;

use crate::fiber::channel::ChannelActorState;

/// One event hub per running application. It is independent of database handles.
#[derive(Clone, Default)]
pub struct SigningEvents {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) hub: std::sync::Arc<notifications::SigningNotifications>,
}

/// Trusted host or tenant channel scope assigned by the runtime, never by clients.
#[derive(Clone)]
pub struct ChannelSigningEvents {
    #[cfg(not(target_arch = "wasm32"))]
    topic: std::sync::Arc<notifications::SigningTopic>,
}

#[cfg(feature = "watchtower")]
pub(crate) struct WatchtowerSigningEvents {
    #[cfg(not(target_arch = "wasm32"))]
    topic: std::sync::Arc<notifications::SigningTopic>,
}

impl Default for ChannelSigningEvents {
    fn default() -> Self {
        SigningEvents::default().channels(None)
    }
}

impl std::fmt::Debug for SigningEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningEvents").finish_non_exhaustive()
    }
}

impl SigningEvents {
    /// Obtain the publisher for the host (`None`) or a registered hosted tenant.
    pub fn channels(&self, tenant: Option<String>) -> ChannelSigningEvents {
        #[cfg(target_arch = "wasm32")]
        let _ = tenant;
        ChannelSigningEvents {
            #[cfg(not(target_arch = "wasm32"))]
            topic: self.hub.topic(notifications::SigningScope::Channel(tenant)),
        }
    }

    #[cfg(feature = "watchtower")]
    pub(crate) fn watchtower(&self, node: &fiber_types::NodeId) -> WatchtowerSigningEvents {
        #[cfg(target_arch = "wasm32")]
        let _ = node;
        WatchtowerSigningEvents {
            #[cfg(not(target_arch = "wasm32"))]
            topic: self
                .hub
                .topic(notifications::SigningScope::Watchtower(node.clone())),
        }
    }
}

impl ChannelSigningEvents {
    /// Capture the public state before persistence consumes the channel snapshot.
    pub(crate) fn prepare(&self, state: &ChannelActorState) -> SigningUpdate<'_> {
        let update = SigningUpdate::new(
            #[cfg(not(target_arch = "wasm32"))]
            &self.topic,
        );
        #[cfg(not(target_arch = "wasm32"))]
        let update = {
            let mut update = update;
            if update.write.observed() {
                update.change = Some(notifications::SigningChange::Channel {
                    channel_id: state.get_id(),
                    status: crate::rpc::channel::to_rpc_channel_signing_status(state),
                });
            }
            update
        };
        #[cfg(target_arch = "wasm32")]
        let _ = state;
        update
    }

    pub(crate) fn prepare_removed(&self, id: Hash256) -> SigningUpdate<'_> {
        SigningUpdate::new(
            #[cfg(not(target_arch = "wasm32"))]
            &self.topic,
        )
        .removed(id)
    }
}

#[cfg(feature = "watchtower")]
impl WatchtowerSigningEvents {
    pub(crate) fn prepare_update(&self) -> SigningUpdate<'_> {
        SigningUpdate::new(
            #[cfg(not(target_arch = "wasm32"))]
            &self.topic,
        )
    }

    pub(crate) fn prepare_created(&self, id: Hash256) -> SigningUpdate<'_> {
        let update = self.prepare_update();
        #[cfg(not(target_arch = "wasm32"))]
        let update = {
            let mut update = update;
            if update.write.observed() {
                update.change = Some(notifications::SigningChange::WatchtowerCreated {
                    channel_id: id,
                    // Registration resets the signer to Internal or an empty External.
                    status: fiber_json_types::WatchtowerSigningStatus::NoSignatureRequired,
                });
            }
            update
        };
        #[cfg(target_arch = "wasm32")]
        let _ = id;
        update
    }

    pub(crate) fn prepare_removed(&self, id: Hash256) -> SigningUpdate<'_> {
        self.prepare_update().removed(id)
    }
}

/// Orders business writes and their notifications with subscription initialization.
/// Dropping an unpublished update releases the lock without emitting an event.
#[must_use = "publish the update only after persistence succeeds"]
pub(crate) struct SigningUpdate<'a> {
    #[cfg(not(target_arch = "wasm32"))]
    write: notifications::SigningWrite<'a>,
    #[cfg(not(target_arch = "wasm32"))]
    change: Option<notifications::SigningChange>,
    #[cfg(target_arch = "wasm32")]
    _scope: std::marker::PhantomData<&'a ()>,
}

impl<'a> SigningUpdate<'a> {
    fn new(#[cfg(not(target_arch = "wasm32"))] topic: &'a notifications::SigningTopic) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            write: topic.write(),
            #[cfg(not(target_arch = "wasm32"))]
            change: None,
            #[cfg(target_arch = "wasm32")]
            _scope: std::marker::PhantomData,
        }
    }

    fn removed(self, id: Hash256) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut update = self;
            update.change = Some(notifications::SigningChange::Removed(id));
            update
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = id;
            self
        }
    }

    /// Capture the final signer projection inside a successful read/modify/write.
    #[cfg(feature = "watchtower")]
    pub(crate) fn watchtower_state(
        &mut self,
        id: Hash256,
        state: &fiber_types::WatchtowerSignerState,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.write.observed() {
            self.change = Some(notifications::SigningChange::WatchtowerUpdated {
                channel_id: id,
                status: crate::rpc::watchtower::to_rpc_watchtower_signing_status(state.clone()),
            });
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (id, state);
    }

    /// Notify the old ID during a committed channel-ID migration, under the same lock.
    pub(crate) fn publish_removed(&self, id: Hash256) {
        #[cfg(not(target_arch = "wasm32"))]
        self.write
            .publish(|| notifications::SigningChange::Removed(id));
        #[cfg(target_arch = "wasm32")]
        let _ = id;
    }

    /// Publish the captured state after persistence, then release the scope lock.
    pub(crate) fn publish(self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(change) = self.change {
            self.write.publish(|| change);
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use futures::FutureExt;

    use crate::{fiber::channel::ChannelActorState, store::sample::StoreSample};

    use super::{
        notifications::{SigningChange, SigningScope},
        SigningEvents,
    };

    #[test]
    fn updates_publish_only_explicitly_and_preserve_channel_migration_order() {
        let events = SigningEvents::default();
        let publisher = events.channels(None);
        let mut changes = events.hub.subscribe(SigningScope::Channel(None));
        let mut state = ChannelActorState::samples(42).remove(0);
        let old_id = state.get_id();

        // Dropping a guard without publishing emits no captured event.
        drop(publisher.prepare(&state));
        assert!(changes.recv().now_or_never().is_none());

        let update = publisher.prepare(&state);
        assert!(changes.recv().now_or_never().is_none());
        update.publish();
        assert_eq!(
            changes.recv().now_or_never().unwrap().unwrap().channel_id(),
            old_id
        );

        let new_id = [8; 32].into();
        state.id = new_id;
        let update = publisher.prepare(&state);
        update.publish_removed(old_id);
        update.publish();
        assert!(
            matches!(changes.recv().now_or_never().unwrap().unwrap().as_ref(), SigningChange::Removed(id) if *id == old_id)
        );
        assert!(
            matches!(changes.recv().now_or_never().unwrap().unwrap().as_ref(), SigningChange::Channel { channel_id, .. } if *channel_id == new_id)
        );
    }
}
