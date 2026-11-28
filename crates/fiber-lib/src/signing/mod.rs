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
                update.changes.push(notifications::SigningChange::Channel {
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

    /// Prepare a creation event for a newly inserted watchtower channel row.
    ///
    /// Callers must publish the returned update only when a new row is actually
    /// inserted; re-registering an existing channel preserves its signer state
    /// and must not emit a `NoSignatureRequired` creation event.
    pub(crate) fn prepare_created(&self, id: Hash256) -> SigningUpdate<'_> {
        let update = self.prepare_update();
        #[cfg(not(target_arch = "wasm32"))]
        let update = {
            let mut update = update;
            if update.write.observed() {
                update
                    .changes
                    .push(notifications::SigningChange::WatchtowerCreated {
                        channel_id: id,
                        // A newly inserted row initializes the signer to Internal or an empty External.
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
    changes: Vec<notifications::SigningChange>,
    #[cfg(target_arch = "wasm32")]
    _scope: std::marker::PhantomData<&'a ()>,
}

impl<'a> SigningUpdate<'a> {
    fn new(#[cfg(not(target_arch = "wasm32"))] topic: &'a notifications::SigningTopic) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            write: topic.write(),
            #[cfg(not(target_arch = "wasm32"))]
            changes: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            _scope: std::marker::PhantomData,
        }
    }

    #[cfg(feature = "watchtower")]
    pub(crate) fn is_observed(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.write.observed()
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }

    fn removed(self, id: Hash256) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut update = self;
            update
                .changes
                .push(notifications::SigningChange::Removed(id));
            update
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = id;
            self
        }
    }

    /// Capture the final signer projection inside a successful read/modify/write.
    #[cfg(all(test, feature = "watchtower"))]
    pub(crate) fn watchtower_state(
        &mut self,
        id: Hash256,
        state: &fiber_types::WatchtowerSignerState,
    ) {
        self.watchtower_state_with_backoff(id, state, 0);
    }

    /// Capture the final signer projection with effective tenant-wide backoff after persistence.
    #[cfg(feature = "watchtower")]
    pub(crate) fn watchtower_state_with_backoff(
        &mut self,
        id: Hash256,
        state: &fiber_types::WatchtowerSignerState,
        node_retry_after_ms: u64,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.write.observed() {
            self.changes
                .push(notifications::SigningChange::WatchtowerUpdated {
                    channel_id: id,
                    status: crate::rpc::watchtower::to_rpc_watchtower_signing_status_with_backoff(
                        state.clone(),
                        node_retry_after_ms,
                        crate::now_timestamp_as_millis_u64(),
                    ),
                });
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (id, state, node_retry_after_ms);
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
        for change in self.changes {
            self.write.publish(|| change);
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[cfg(test)]
mod tests;
