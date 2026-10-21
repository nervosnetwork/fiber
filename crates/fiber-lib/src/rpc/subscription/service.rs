//! Signing state streams, independent of JSON-RPC connections and credentials.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use anyhow::{anyhow, bail, Result};
use fiber_json_types::{
    GetChannelSigningStatusResult, SubscriptionEvent, SubscriptionOptions, SubscriptionTopic,
};
use fiber_types::{Hash256, NodeId};
use futures::{stream, stream::BoxStream, StreamExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{rpc::server::RpcServerStore, store::notifications::SigningChanges};

const MAX_SUBSCRIPTIONS: usize = 16_384;
const MAX_FILTER_CHANNELS: usize = 256;

pub(super) type EventStream = BoxStream<'static, Result<SubscriptionEvent>>;

#[derive(Clone)]
pub(super) struct SubscriptionService {
    slots: Arc<Semaphore>,
    channels_enabled: bool,
    watchtower_enabled: bool,
}

impl SubscriptionService {
    pub(super) fn new(channels_enabled: bool, watchtower_enabled: bool) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_SUBSCRIPTIONS)),
            channels_enabled,
            watchtower_enabled,
        }
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
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!("subscription limit reached"))?;
        // Listen before reading, so changes during the initial read remain pending.
        let changes = source.listen(topic);
        if options
            .channel_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| !source.exists(topic, &(*id).into())))
        {
            bail!("channel not found");
        }
        let previous = source.current(topic, &options)?;
        let pending = previous
            .values()
            .filter(|event| is_pending(event))
            .cloned()
            .collect();
        let state = State {
            source,
            topic,
            options,
            changes,
            previous,
            pending,
            _permit: permit,
        };
        Ok(stream::try_unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Ok(Some((event, state)));
                }
                state.changes.changed().await;
                let current = state.source.current(state.topic, &state.options)?;
                for (id, event) in &current {
                    if state
                        .previous
                        .get(id)
                        .map(serde_json::to_value)
                        .transpose()?
                        != Some(serde_json::to_value(event)?)
                    {
                        state.pending.push_back(event.clone());
                    }
                }
                for id in state.previous.keys().filter(|id| !current.contains_key(id)) {
                    state.pending.push_back(SubscriptionEvent::Removed {
                        channel_id: (*id).into(),
                    });
                }
                state.previous = current;
            }
        })
        .boxed())
    }
}

struct State<S> {
    source: Source<S>,
    topic: SubscriptionTopic,
    options: SubscriptionOptions,
    changes: SigningChanges,
    previous: HashMap<Hash256, SubscriptionEvent>,
    pending: VecDeque<SubscriptionEvent>,
    _permit: OwnedSemaphorePermit,
}

pub(super) struct Source<S> {
    pub(super) channels: S,
    pub(super) watchtower: S,
    pub(super) node: NodeId,
}

impl<S: RpcServerStore> Source<S> {
    fn listen(&self, topic: SubscriptionTopic) -> SigningChanges {
        match topic {
            SubscriptionTopic::ChannelSigning => self.channels.subscribe_channel_signing(),
            SubscriptionTopic::WatchtowerSigning => {
                self.watchtower.subscribe_watchtower_signing(&self.node)
            }
        }
    }

    fn exists(&self, topic: SubscriptionTopic, channel: &Hash256) -> bool {
        match topic {
            SubscriptionTopic::ChannelSigning => {
                self.channels.get_channel_actor_state(channel).is_some()
            }
            SubscriptionTopic::WatchtowerSigning => {
                #[cfg(feature = "watchtower")]
                {
                    self.watchtower
                        .get_watch_channel(&self.node, channel)
                        .is_some()
                }
                #[cfg(not(feature = "watchtower"))]
                {
                    false
                }
            }
        }
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
                        self.watchtower
                            .get_watch_channels_for_node(&self.node)
                            .into_iter()
                            .map(|channel| channel.channel_id)
                            .collect()
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
                    let Some(state) = self.channels.get_channel_actor_state(&id) else {
                        continue;
                    };
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
                            continue;
                        }
                        SubscriptionEvent::WatchtowerSigning(Box::new(
                            fiber_json_types::GetWatchtowerSigningStatusResult {
                                channel_id: id.into(),
                                status: crate::rpc::watchtower::to_rpc_watchtower_signing_status(
                                    self.watchtower.get_watchtower_signer(&self.node, &id),
                                ),
                            },
                        ))
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
