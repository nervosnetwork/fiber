//! Application-owned signing event channels, shared by business actors and RPC.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod notifications;

use fiber_types::{CommitDiff, Hash256};

use crate::fiber::channel::{ChannelActorState, ChannelActorStateStore};

/// One event hub per running application. It is independent of database handles.
#[derive(Clone, Default)]
pub struct SigningEvents {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) hub: std::sync::Arc<notifications::SigningNotifications>,
}

/// Trusted host or tenant channel scope assigned by the runtime, never by clients.
#[derive(Clone, Default)]
pub struct ChannelSigningEvents {
    #[cfg(not(target_arch = "wasm32"))]
    events: SigningEvents,
    #[cfg(not(target_arch = "wasm32"))]
    tenant: Option<String>,
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
            events: self.clone(),
            #[cfg(not(target_arch = "wasm32"))]
            tenant,
        }
    }
}

impl ChannelSigningEvents {
    fn persist(
        &self,
        state: ChannelActorState,
        removed: Option<Hash256>,
        commit: impl FnOnce(ChannelActorState),
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        let topic = self
            .events
            .hub
            .topic(notifications::SigningScope::Channel(self.tenant.clone()));
        #[cfg(not(target_arch = "wasm32"))]
        let write = topic.write();
        // Capture only the public projection before moving state into persistence.
        // No full channel clone or database lookup is needed for publication.
        #[cfg(not(target_arch = "wasm32"))]
        let change = write
            .observed()
            .then(|| notifications::SigningChange::Channel {
                channel_id: state.get_id(),
                status: crate::rpc::channel::to_rpc_channel_signing_status(&state),
            });
        commit(state);
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(id) = removed {
                write.publish(|| notifications::SigningChange::Removed(id));
            }
            if let Some(change) = change {
                write.publish(|| change);
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (self, removed);
    }

    pub(crate) fn save<S: ChannelActorStateStore>(&self, store: &S, state: ChannelActorState) {
        self.persist(state, None, |state| store.insert_channel_actor_state(state));
    }

    pub(crate) fn save_with_diff<S: ChannelActorStateStore>(
        &self,
        store: &S,
        state: ChannelActorState,
        diff: &CommitDiff,
    ) {
        self.persist(state, None, |state| {
            store.insert_channel_actor_state_with_pending_commit_diff(state, diff)
        });
    }

    pub(crate) fn move_channel<S: ChannelActorStateStore>(
        &self,
        store: &S,
        old_id: &Hash256,
        state: ChannelActorState,
    ) {
        self.persist(state, Some(*old_id), |state| {
            store.move_channel_actor_state(old_id, state)
        });
    }

    pub(crate) fn remove<S: ChannelActorStateStore>(&self, store: &S, id: &Hash256) {
        #[cfg(not(target_arch = "wasm32"))]
        let topic = self
            .events
            .hub
            .topic(notifications::SigningScope::Channel(self.tenant.clone()));
        #[cfg(not(target_arch = "wasm32"))]
        let write = topic.write();
        store.delete_channel_actor_state(id);
        #[cfg(not(target_arch = "wasm32"))]
        write.publish(|| notifications::SigningChange::Removed(*id));
    }
}

#[cfg(feature = "watchtower")]
impl SigningEvents {
    /// Register a watched channel and publish its freshly initialized signer status.
    pub(crate) fn create_watch(
        &self,
        node: &fiber_types::NodeId,
        id: Hash256,
        commit: impl FnOnce(),
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        let topic = self
            .hub
            .topic(notifications::SigningScope::Watchtower(node.clone()));
        #[cfg(not(target_arch = "wasm32"))]
        let write = topic.write();
        commit();
        #[cfg(not(target_arch = "wasm32"))]
        write.publish(|| notifications::SigningChange::WatchtowerCreated {
            channel_id: id,
            // Registration resets the signer to Internal or an empty External.
            status: fiber_json_types::WatchtowerSigningStatus::NoSignatureRequired,
        });
        #[cfg(target_arch = "wasm32")]
        let _ = (node, id);
    }

    pub(crate) fn remove_watch<S: crate::watchtower::WatchtowerStore>(
        &self,
        store: &S,
        node: fiber_types::NodeId,
        id: Hash256,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        let topic = self
            .hub
            .topic(notifications::SigningScope::Watchtower(node.clone()));
        #[cfg(not(target_arch = "wasm32"))]
        let write = topic.write();
        let removed = store.remove_watch_channel(node, id);
        #[cfg(not(target_arch = "wasm32"))]
        if removed {
            write.publish(|| notifications::SigningChange::Removed(id));
        }
        #[cfg(target_arch = "wasm32")]
        let _ = removed;
    }

    pub(crate) fn update_watch_signer<S: crate::watchtower::WatchtowerStore, R, E>(
        &self,
        store: &S,
        node: &fiber_types::NodeId,
        id: &Hash256,
        update: impl FnOnce(&mut fiber_types::WatchtowerSignerState) -> Result<R, E>,
    ) -> Result<R, E> {
        #[cfg(not(target_arch = "wasm32"))]
        let topic = self
            .hub
            .topic(notifications::SigningScope::Watchtower(node.clone()));
        #[cfg(not(target_arch = "wasm32"))]
        let write = topic.write();
        let result = store.update_watchtower_signer(node, id, |state| {
            let result = update(state)?;
            // Capture the final projection inside the ordinary read/modify/write.
            // Publish only after that operation has committed successfully.
            #[cfg(not(target_arch = "wasm32"))]
            let status = crate::rpc::watchtower::to_rpc_watchtower_signing_status(state.clone());
            #[cfg(target_arch = "wasm32")]
            let status = ();
            Ok((result, status))
        });
        result.map(|(result, status)| {
            #[cfg(not(target_arch = "wasm32"))]
            write.publish(|| notifications::SigningChange::WatchtowerUpdated {
                channel_id: *id,
                status,
            });
            #[cfg(target_arch = "wasm32")]
            let _ = status;
            result
        })
    }
}
