//! Signing state streams, independent of JSON-RPC connections and credentials.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use anyhow::{anyhow, bail, Result};
use fiber_json_types::{
    GetChannelSigningStatusResult, SubscriptionEvent, SubscriptionOptions, SubscriptionTopic,
};
use fiber_types::{Hash256, NodeId};
use futures::{stream, stream::BoxStream, StreamExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    rpc::server::RpcServerStore,
    signing::{
        notifications::{SigningChange, SigningChanges, SigningScope},
        SigningEvents,
    },
};

const MAX_SUBSCRIPTIONS: usize = 16_384;
const MAX_SUBSCRIPTIONS_PER_TENANT: usize = 32;
const MAX_FILTER_CHANNELS: usize = 256;

pub(super) type EventStream = BoxStream<'static, Result<SubscriptionEvent>>;

struct TenantSubGuard {
    counts: Arc<parking_lot::Mutex<HashMap<String, usize>>>,
    tenant: String,
}

impl Drop for TenantSubGuard {
    fn drop(&mut self) {
        let mut counts = self.counts.lock();
        if let Some(count) = counts.get_mut(&self.tenant) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.tenant);
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct SubscriptionService {
    slots: Arc<Semaphore>,
    per_tenant_counts: Arc<parking_lot::Mutex<HashMap<String, usize>>>,
    channels_enabled: bool,
    watchtower_enabled: bool,
}

impl SubscriptionService {
    pub(super) fn new(channels_enabled: bool, watchtower_enabled: bool) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_SUBSCRIPTIONS)),
            per_tenant_counts: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            channels_enabled,
            watchtower_enabled,
        }
    }

    pub(super) async fn subscribe_async<S: RpcServerStore + Send + 'static>(
        self,
        source: Source<S>,
        topic: SubscriptionTopic,
        options: SubscriptionOptions,
    ) -> Result<EventStream> {
        tokio::task::spawn_blocking(move || self.subscribe(source, topic, options)).await?
    }

    pub(super) fn subscribe<S: RpcServerStore + Send + 'static>(
        &self,
        source: Source<S>,
        topic: SubscriptionTopic,
        options: SubscriptionOptions,
    ) -> Result<EventStream> {
        let enabled = match topic {
            SubscriptionTopic::ChannelSigning => self.channels_enabled,
            SubscriptionTopic::WatchtowerSigning => {
                cfg!(feature = "watchtower") && self.watchtower_enabled
            }
        };
        if !enabled {
            bail!("subscription topic is disabled");
        }
        if options
            .channel_ids
            .as_ref()
            .is_some_and(|ids| ids.len() > MAX_FILTER_CHANNELS)
        {
            bail!("too many channel filters");
        }
        let tenant_guard = if let Some(tenant) = &source.tenant {
            let mut counts = self.per_tenant_counts.lock();
            let count = counts.entry(tenant.clone()).or_default();
            if *count >= MAX_SUBSCRIPTIONS_PER_TENANT {
                bail!("tenant subscription limit reached");
            }
            *count += 1;
            Some(TenantSubGuard {
                counts: self.per_tenant_counts.clone(),
                tenant: tenant.clone(),
            })
        } else {
            None
        };
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!("subscription limit reached"))?;
        let mut changes = source.listen(topic);
        // Business persistence and publication use the same scope lock. This read is
        // the only database access retained by a subscription.
        let initial = changes.snapshot(|| source.current(topic, &options))?;
        let pending = initial
            .values()
            .filter(|event| is_pending(event))
            .cloned()
            .collect();
        let state = State {
            filters: options
                .channel_ids
                .map(|ids| ids.into_iter().map(Into::into).collect()),
            changes,
            previous: initial,
            pending,
            _permit: permit,
            _tenant_guard: tenant_guard,
        };
        // State owns no source or database handle after the initial read.
        Ok(stream::try_unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Ok(Some((event, state)));
                }
                let change = state.changes.recv().await.map_err(|error| {
                    anyhow!("signing event stream interrupted ({error}); reconnect to recover pending requests")
                })?;
                let id = change.channel_id();
                if state.filters.as_ref().is_some_and(|ids| !ids.contains(&id)) {
                    continue;
                }
                let event = match change.as_ref() {
                    SigningChange::Channel { status, .. } => {
                        SubscriptionEvent::ChannelSigning(Box::new(GetChannelSigningStatusResult {
                            channel_id: id.into(), status: status.clone().map_err(anyhow::Error::msg)?,
                        }))
                    }
                    #[cfg(feature = "watchtower")]
                    SigningChange::WatchtowerUpdated { .. } if !state.previous.contains_key(&id) => {
                        // A signer row without a watched channel is not visible.
                        continue;
                    }
                    #[cfg(feature = "watchtower")]
                    SigningChange::WatchtowerCreated { status, .. }
                    | SigningChange::WatchtowerUpdated { status, .. } => {
                        SubscriptionEvent::WatchtowerSigning(Box::new(
                            fiber_json_types::GetWatchtowerSigningStatusResult {
                                channel_id: id.into(),
                                status: status.clone(),
                            },
                        ))
                    }
                    SigningChange::Removed(_) => {
                        if state.previous.remove(&id).is_none() {
                            continue;
                        }
                        return Ok(Some((SubscriptionEvent::Removed { channel_id: id.into() }, state)));
                    }
                };
                if state.previous.get(&id) == Some(&event) {
                    continue;
                }
                state.previous.insert(id, event.clone());
                return Ok(Some((event, state)));
            }
        }).boxed())
    }
}

struct State {
    filters: Option<HashSet<Hash256>>,
    changes: SigningChanges,
    previous: HashMap<Hash256, SubscriptionEvent>,
    pending: VecDeque<SubscriptionEvent>,
    _permit: OwnedSemaphorePermit,
    _tenant_guard: Option<TenantSubGuard>,
}

pub(super) struct Source<S> {
    pub(super) events: SigningEvents,
    pub(super) tenant: Option<String>,
    pub(super) channels: S,
    #[cfg(feature = "watchtower")]
    pub(super) watchtower: S,
    pub(super) node: NodeId,
}

impl<S: RpcServerStore> Source<S> {
    fn listen(&self, topic: SubscriptionTopic) -> SigningChanges {
        self.events.hub.subscribe(match topic {
            SubscriptionTopic::ChannelSigning => SigningScope::Channel(self.tenant.clone()),
            SubscriptionTopic::WatchtowerSigning => SigningScope::Watchtower(self.node.clone()),
        })
    }

    fn current(
        &self,
        topic: SubscriptionTopic,
        options: &SubscriptionOptions,
    ) -> Result<HashMap<Hash256, SubscriptionEvent>> {
        let ids: Vec<Hash256> = match &options.channel_ids {
            Some(ids) => ids.iter().map(|id| (*id).into()).collect(),
            None => match topic {
                SubscriptionTopic::ChannelSigning => self
                    .channels
                    .get_channel_states(None)
                    .into_iter()
                    .map(|(_, id, _)| id)
                    .collect(),
                SubscriptionTopic::WatchtowerSigning => {
                    #[cfg(feature = "watchtower")]
                    {
                        return self
                            .watchtower
                            .get_watch_channels_for_node(&self.node)
                            .into_iter()
                            .map(|channel| {
                                let id = channel.channel_id;
                                Ok((id, self.watchtower_event(id)))
                            })
                            .collect();
                    }
                    #[cfg(not(feature = "watchtower"))]
                    {
                        Vec::new()
                    }
                }
            },
        };
        let mut current = HashMap::new();
        for id in ids {
            let event = match topic {
                SubscriptionTopic::ChannelSigning => {
                    let state = self
                        .channels
                        .get_channel_actor_state(&id)
                        .ok_or_else(|| anyhow!("channel not found"))?;
                    let status = crate::rpc::channel::to_rpc_channel_signing_status(&state)
                        .map_err(|error| anyhow!(error))?;
                    SubscriptionEvent::ChannelSigning(Box::new(GetChannelSigningStatusResult {
                        channel_id: id.into(),
                        status,
                    }))
                }
                SubscriptionTopic::WatchtowerSigning => {
                    #[cfg(feature = "watchtower")]
                    {
                        if self.watchtower.get_watch_channel(&self.node, &id).is_none() {
                            bail!("channel not found");
                        }
                        self.watchtower_event(id)
                    }
                    #[cfg(not(feature = "watchtower"))]
                    {
                        continue;
                    }
                }
            };
            current.insert(id, event);
        }
        Ok(current)
    }

    #[cfg(feature = "watchtower")]
    fn watchtower_event(&self, id: Hash256) -> SubscriptionEvent {
        let (_, _, node_retry_after_ms) = self.watchtower.watchtower_fee_hold_stats(&self.node);
        SubscriptionEvent::WatchtowerSigning(Box::new(
            fiber_json_types::GetWatchtowerSigningStatusResult {
                channel_id: id.into(),
                status: crate::rpc::watchtower::to_rpc_watchtower_signing_status_with_backoff(
                    self.watchtower.get_watchtower_signer(&self.node, &id),
                    node_retry_after_ms,
                    crate::now_timestamp_as_millis_u64(),
                ),
            },
        ))
    }
}

fn is_pending(event: &SubscriptionEvent) -> bool {
    match event {
        SubscriptionEvent::ChannelSigning(result) => matches!(
            result.status,
            fiber_json_types::ChannelSigningStatus::SignatureRequired { .. }
        ),
        #[cfg(feature = "watchtower")]
        SubscriptionEvent::WatchtowerSigning(result) => matches!(
            result.status,
            fiber_json_types::WatchtowerSigningStatus::SignatureRequired { .. }
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
