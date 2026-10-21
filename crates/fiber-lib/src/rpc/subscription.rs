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
        biscuit::{extract_tenant_id, scoped_rpc_node_id},
        middleware::RpcAuthorization,
        server::RpcServerStore,
    },
    store::notifications::SigningChanges,
};

const MAX_SUBSCRIPTIONS: usize = 16_384;
const MAX_FILTER_CHANNELS: usize = 256;

fn read_method(topic: SubscriptionTopic) -> &'static str {
    match topic {
        SubscriptionTopic::ChannelSigning => "get_channel_signing_status",
        SubscriptionTopic::WatchtowerSigning => "get_watchtower_signing_status",
    }
}

fn authorize_topic(
    authorization: &RpcAuthorization,
    topic: SubscriptionTopic,
) -> Result<(Option<TenantId>, NodeId)> {
    authorization
        .authorize("subscribe")
        .map_err(|_| anyhow!("Unauthorized"))?;
    let Some((biscuit, _)) = authorization
        .authorize(read_method(topic))
        .map_err(|_| anyhow!("Unauthorized"))?
    else {
        return Ok((None, NodeId::local()));
    };
    let tenant = extract_tenant_id(&biscuit)?;
    let node = if topic == SubscriptionTopic::WatchtowerSigning {
        scoped_rpc_node_id(&biscuit)?
    } else {
        NodeId::local()
    };
    Ok((tenant, node))
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
    authorization: &RpcAuthorization,
    topic: SubscriptionTopic,
    event: &SubscriptionEvent,
) -> Result<()> {
    let message = serde_json::value::to_raw_value(event)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        authorize_topic(authorization, topic)?;
        if sink.try_send(message.clone()).is_ok() {
            return Ok(());
        }
        if sink.is_closed() || tokio::time::Instant::now() >= deadline {
            bail!("subscription queue full or closed; reconnect to recover pending requests");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run<S: RpcServerStore>(
    source: Source<S>,
    topic: SubscriptionTopic,
    options: SubscriptionOptions,
    authorization: RpcAuthorization,
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
    // Signing topics always resend outstanding work using ordinary updates.
    for event in previous.values().filter(|event| is_pending(event)) {
        send(&mut sink, &authorization, topic, event).await?;
    }
    loop {
        tokio::select! {
            biased;
            _ = sink.closed() => return Ok(()),
            _ = changes.changed() => {}
        }
        authorize_topic(&authorization, topic)?;
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

/// <a id="subscription"></a>
/// ### Module `Subscription`
///
/// Subscribe to signing status over WebSocket on the existing RPC listener.
/// Ordinary RPC calls can use the same connection. HTTP does not support subscriptions.
///
/// Authenticate the WebSocket upgrade with the ordinary
/// `Authorization: Bearer <Biscuit>` header. Tokens in URL parameters are not supported.
/// A connection keeps one credential; reconnect to use a refreshed token.
/// When RPC authentication is enabled, both `subscribe` and `unsubscribe` require
/// `write("subscriptions")`, using the ordinary RPC authorization rules. Subscribing
/// also requires the selected topic's read permission. Tokens issued by hosted
/// tenant registration include the subscription capability; existing tokens without
/// it must be reissued.
///
/// Tenant and node identities come from verified token authority facts, rather than
/// subscription parameters. Tenant subscribers receive only their authorized tenant
/// data. Non-tenant subscribers follow host query permissions and cannot access all
/// tenant stores. Without RPC authentication, subscriptions use the local host scope.
///
/// <a id="subscription-subscribe"></a>
/// #### Method `subscribe`
///
/// Subscribe to a topic and return an ID for subsequent notifications and cancellation.
///
/// ##### Params
///
/// * `topic` - String, one of the topics below. In addition to `write("subscriptions")`,
///   each topic requires the same complete read policy as its corresponding query
///   RPC, including alternative capabilities. Missing permissions reject the
///   subscription before it is established.
/// * `options` - Optional object with a single field, `channel_ids`, an optional array
///   of [Hash256](#type-hash256). Omission selects all authorized channels; an empty
///   array selects none. At most 256 IDs are allowed. Unknown fields are rejected.
///
/// | Topic | Read permission | Scope |
/// | --- | --- | --- |
/// | `channel_signing` | [get_channel_signing_status](#channel-get_channel_signing_status) | Host or authenticated tenant channels |
/// | `watchtower_signing` | [get_watchtower_signing_status](#watchtower-get_watchtower_signing_status) | Watchtower channels for the authorized `node(...)` |
///
/// A topic is available only when its corresponding RPC module is enabled.
/// `watchtower_signing` also requires the watchtower build feature. A tenant token's
/// watchtower node identity must match that tenant's registered watchtower identity.
///
/// ##### Returns
///
/// An opaque subscription ID (string or number). Preserve its value and JSON type
/// when passing it to `unsubscribe` or matching notifications.
///
/// ##### Examples
///
/// Subscribe to all accessible channel signing requests:
///
/// ```json
/// {"jsonrpc":"2.0","id":1,"method":"subscribe","params":["channel_signing"]}
/// ```
///
/// ```json
/// {"jsonrpc":"2.0","id":1,"result":"0xabc"}
/// ```
///
/// Subscribe to one channel (replace the example ID with an accessible channel ID):
///
/// ```json
/// {"jsonrpc":"2.0","id":2,"method":"subscribe","params":["channel_signing",{"channel_ids":["0x0000000000000000000000000000000000000000000000000000000000000001"]}]}
/// ```
///
/// ##### Notifications
///
/// Notifications use method `subscribe`. `params.subscription` identifies the
/// subscription, and `params.result` contains a typed update:
///
/// ```json
/// {
///   "jsonrpc": "2.0",
///   "method": "subscribe",
///   "params": {
///     "subscription": "0xabc",
///     "result": {
///       "type": "channel_signing",
///       "data": {
///         "channel_id": "0x0000000000000000000000000000000000000000000000000000000000000001",
///         "status": {"type": "NoSignatureRequired"}
///       }
///     }
///   }
/// }
/// ```
///
/// * `channel_signing`: `data` is the result of `get_channel_signing_status`, with
///   `channel_id` and `status` ([ChannelSigningStatus](#type-channelsigningstatus)).
/// * `watchtower_signing`: `data` is the result of `get_watchtower_signing_status`,
///   with `channel_id` and `status` ([WatchtowerSigningStatus](#type-watchtowersigningstatus)).
/// * `removed`: a previously visible channel was removed. `data` contains its
///   `channel_id`, for example:
///
/// ```json
/// {"type":"removed","data":{"channel_id":"0x0000000000000000000000000000000000000000000000000000000000000001"}}
/// ```
///
/// ##### Pending requests and reconnection
///
/// Signing subscriptions always send currently pending requests when established,
/// then send status changes, including transitions to no pending request. Initial
/// requests use the same `SignatureRequired` status and notification format as live
/// requests. There is no separate completion marker or option to enable this replay.
/// Changes during the initial read are picked up afterwards; the initial requests
/// are not an atomic view across channels.
///
/// Unchanged states are deduplicated per subscription, and intermediate states may
/// be coalesced. This API reports current state, with no historical cursor or
/// exactly-once guarantee. Subscriptions end on disconnect, cancellation,
/// authorization failure, or server shutdown. Reconnect and subscribe again to
/// receive pending requests. Deduplicate signing work by request ID and submit
/// signatures through the existing signature-submission RPCs.
///
/// ##### Limits and errors
///
/// Each listener allows 16,384 active general subscriptions, with at most 32 per
/// WebSocket connection. The connection output buffer holds 64 messages. A queue
/// that remains full for five seconds terminates the affected subscription;
/// reconnect and subscribe again to recover pending requests.
///
/// Subscription and topic permissions are checked on creation, when signing state
/// changes, and before enqueueing each notification. An update that finds expired
/// or invalid credentials terminates the subscription without sending it. Token
/// expiry alone does not proactively close an idle subscription.
///
/// Invalid topics or options, disabled topics, excessive filters, and exhausted
/// subscription capacity reject the request. A missing channel and a channel
/// outside the authorized scope both return `channel not found`.
///
/// ---
///
/// <a id="subscription-unsubscribe"></a>
/// #### Method `unsubscribe`
///
/// Cancel a subscription created on the same WebSocket connection.
/// Requires a valid token granting `write("subscriptions")` when RPC authentication
/// is enabled. An expired token cannot cancel a subscription; disconnecting still
/// releases the connection's subscriptions.
///
/// ##### Params
///
/// * `subscription_id` - The opaque string or number returned by `subscribe`.
///
/// ##### Returns
///
/// A boolean: `true` if the subscription was cancelled, `false` if the ID does not
/// identify an active subscription on this connection. Another connection cannot
/// cancel it. Multiple subscribers for the same tenant are independent.
///
/// ##### Examples
///
/// ```json
/// {"jsonrpc":"2.0","id":3,"method":"unsubscribe","params":["0xabc"]}
/// ```
///
/// ```json
/// {"jsonrpc":"2.0","id":3,"result":true}
/// ```
///
/// ---
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
                        .get::<RpcAuthorization>()
                        .cloned()
                        .ok_or_else(|| anyhow!("RPC authorization missing"))?;
                    let (tenant, node) = authorize_topic(&authorization, topic)?;
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
