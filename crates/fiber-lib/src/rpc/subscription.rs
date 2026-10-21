//! Scoped JSON-RPC subscriptions over WebSocket.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Result};
use fiber_json_types::{
    GetChannelSigningStatusResult, SubscriptionEvent, SubscriptionOptions, SubscriptionTopic,
};
use fiber_types::{Hash256, NodeId};
use jsonrpsee::{
    core::SubscriptionError, types::ErrorObjectOwned, Extensions, PendingSubscriptionSink,
    RpcModule, SubscriptionSink,
};
use tokio::sync::Semaphore;

use crate::{
    lsp::{LspServiceMessage, TenantId},
    rpc::{
        biscuit::{extract_tenant_id, scoped_rpc_node_id, BiscuitAuth},
        server::RpcServerStore,
        tenant::enforce_tenant_method_allowlist,
    },
    store::notifications::SigningChanges,
};

const AUTH_INTERVAL: Duration = Duration::from_secs(10);
const MAX_SUBSCRIPTIONS: usize = 16_384;
const MAX_FILTER_CHANNELS: usize = 256;

/// Inserted by RPC middleware, never constructed from RPC parameters.
#[derive(Clone)]
pub(crate) struct SubscriptionAuthorization {
    pub(crate) auth: Arc<BiscuitAuth>,
    pub(crate) token: Option<String>,
}

fn read_method(topic: SubscriptionTopic) -> &'static str {
    match topic {
        SubscriptionTopic::ChannelSigning => "get_channel_signing_status",
        SubscriptionTopic::WatchtowerSigning => "get_watchtower_signing_status",
    }
}

impl SubscriptionAuthorization {
    fn authorize(&self, topic: SubscriptionTopic) -> Result<(Option<TenantId>, NodeId)> {
        let Some(token) = &self.token else {
            return Ok((None, NodeId::local()));
        };
        let (biscuit, _) = self
            .auth
            .check_permission(read_method(topic), token)
            .map_err(|_| anyhow!("Unauthorized"))?;
        enforce_tenant_method_allowlist(read_method(topic), &biscuit)?;
        let tenant = extract_tenant_id(&biscuit)?;
        let node = if topic == SubscriptionTopic::WatchtowerSigning {
            scoped_rpc_node_id(&biscuit)?
        } else {
            NodeId::local()
        };
        Ok((tenant, node))
    }
}

struct Source<S> {
    channels: S,
    watchtower: S,
    node: NodeId,
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
                    let status = super::channel::to_rpc_channel_signing_status(&state)
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
                                status: super::watchtower::to_rpc_watchtower_signing_status(
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

async fn send(
    sink: &mut SubscriptionSink,
    authorization: &SubscriptionAuthorization,
    topic: SubscriptionTopic,
    event: &SubscriptionEvent,
) -> Result<()> {
    let message = serde_json::value::to_raw_value(event)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        authorization.authorize(topic)?;
        if sink.try_send(message.clone()).is_ok() {
            return Ok(());
        }
        if sink.is_closed() || tokio::time::Instant::now() >= deadline {
            bail!("subscription queue full or closed; reconnect and request a snapshot");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run<S: RpcServerStore>(
    source: Source<S>,
    topic: SubscriptionTopic,
    options: SubscriptionOptions,
    authorization: SubscriptionAuthorization,
    pending: PendingSubscriptionSink,
) -> Result<()> {
    // Register first: changes during the initial read remain pending in this listener.
    let mut changes = source.listen(topic);
    if let Some(ids) = &options.channel_ids {
        if ids.iter().any(|id| !source.exists(topic, &(*id).into())) {
            pending
                .reject(ErrorObjectOwned::owned(
                    -32001,
                    "channel not found",
                    None::<()>,
                ))
                .await;
            return Ok(());
        }
    }
    let mut previous = source.current(topic, &options)?;
    let mut sink = pending.accept().await?;
    if options.include_snapshot {
        for event in previous.values().filter(|event| is_pending(event)) {
            send(&mut sink, &authorization, topic, event).await?;
        }
        send(
            &mut sink,
            &authorization,
            topic,
            &SubscriptionEvent::SnapshotComplete,
        )
        .await?;
    }
    let mut recheck = tokio::time::interval(AUTH_INTERVAL);
    loop {
        tokio::select! {
            biased;
            _ = sink.closed() => return Ok(()),
            _ = recheck.tick() => { authorization.authorize(topic)?; continue; }
            _ = changes.changed() => {}
        }
        authorization.authorize(topic)?;
        let current = source.current(topic, &options)?;
        for (id, event) in &current {
            if previous.get(id).map(serde_json::to_value).transpose()?
                != Some(serde_json::to_value(event)?)
            {
                send(&mut sink, &authorization, topic, event).await?;
            }
        }
        for id in previous.keys().filter(|id| !current.contains_key(id)) {
            send(
                &mut sink,
                &authorization,
                topic,
                &SubscriptionEvent::Removed {
                    channel_id: (*id).into(),
                },
            )
            .await?;
        }
        previous = current;
    }
}

pub(crate) fn register_subscriptions<S: RpcServerStore + Clone + Send + Sync + 'static>(
    modules: &mut RpcModule<()>,
    store: S,
    lsp: Option<ractor::ActorRef<LspServiceMessage>>,
    channels_enabled: bool,
    watchtower_enabled: bool,
) -> Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_SUBSCRIPTIONS));
    modules.register_subscription(
        "subscribe",
        "subscribe",
        "unsubscribe",
        move |params, pending, _, extensions: Extensions| {
            let store = store.clone();
            let lsp = lsp.clone();
            let slots = slots.clone();
            async move {
                let prepare = || -> Result<_> {
                    let mut sequence = params.sequence();
                    let topic: SubscriptionTopic = sequence.next()?;
                    let options: SubscriptionOptions =
                        sequence.optional_next()?.unwrap_or_default();
                    if sequence.optional_next::<serde_json::Value>()?.is_some() {
                        bail!("expected topic and optional options");
                    }
                    let enabled = match topic {
                        SubscriptionTopic::ChannelSigning => channels_enabled,
                        SubscriptionTopic::WatchtowerSigning => {
                            cfg!(feature = "watchtower") && watchtower_enabled
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
                    let authorization = extensions
                        .get::<SubscriptionAuthorization>()
                        .cloned()
                        .ok_or_else(|| anyhow!("subscription authorization missing"))?;
                    let (tenant, node) = authorization.authorize(topic)?;
                    let permit = slots
                        .try_acquire_owned()
                        .map_err(|_| anyhow!("subscription limit reached"))?;
                    Ok((topic, options, authorization, tenant, node, permit))
                };
                let (topic, options, authorization, tenant, node, _permit) = match prepare() {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        pending
                            .reject(ErrorObjectOwned::owned(
                                -32001,
                                error.to_string(),
                                None::<()>,
                            ))
                            .await;
                        return Ok::<(), SubscriptionError>(());
                    }
                };
                let result = if let Some(tenant) = tenant {
                    let context = async {
                        let actor =
                            lsp.ok_or_else(|| anyhow!("hosted LSP service is not enabled"))?;
                        ractor::call_t!(
                            actor,
                            LspServiceMessage::GetTenantSubscriptionContext,
                            5_000,
                            tenant
                        )
                        .map_err(|_| anyhow!("hosted LSP service unavailable"))?
                        .map_err(anyhow::Error::msg)
                    }
                    .await;
                    match context {
                        Ok(context) => {
                            if topic == SubscriptionTopic::WatchtowerSigning
                                && node != context.node_id
                            {
                                pending
                                    .reject(ErrorObjectOwned::owned(
                                        -32001,
                                        "watchtower identity does not match tenant",
                                        None::<()>,
                                    ))
                                    .await;
                                return Ok(());
                            }
                            run(
                                Source {
                                    channels: context.store,
                                    watchtower: context.watchtower_store,
                                    node,
                                },
                                topic,
                                options,
                                authorization,
                                pending,
                            )
                            .await
                        }
                        Err(error) => {
                            pending
                                .reject(ErrorObjectOwned::owned(
                                    -32001,
                                    error.to_string(),
                                    None::<()>,
                                ))
                                .await;
                            return Ok(());
                        }
                    }
                } else {
                    run(
                        Source {
                            channels: store.clone(),
                            watchtower: store,
                            node,
                        },
                        topic,
                        options,
                        authorization,
                        pending,
                    )
                    .await
                };
                result.map_err(|error| SubscriptionError::from(error.to_string()))
            }
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
