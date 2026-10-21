//! Scoped JSON-RPC subscriptions over a dedicated WebSocket listener.

mod service;
mod transport;

use anyhow::{anyhow, bail, Result};
use fiber_json_types::{SubscriptionOptions, SubscriptionTopic};
use fiber_types::NodeId;
use jsonrpsee::{
    core::SubscriptionResult, proc_macros::rpc, types::ErrorObjectOwned, Extensions,
    PendingSubscriptionSink,
};

use crate::{
    lsp::LspServiceMessage,
    rpc::{
        biscuit::{extract_tenant_id, scoped_rpc_node_id},
        middleware::RpcAuthorization,
        server::RpcServerStore,
    },
};

use service::{EventStream, Source, SubscriptionService};

/// <a id="subscription"></a>
/// ### Module `Subscription`
///
/// Subscribe to signing status over WebSocket on the dedicated `rpc.ws_listening_addr` listener.
/// Ordinary RPC calls can use the same connection. The original `rpc.listening_addr`
/// listener retains its existing RPCs and CCH subscriptions, but does not expose
/// these general signing subscriptions. Omit `ws_listening_addr` to disable them.
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
/// delivery failure, or server shutdown. Reconnect and subscribe again to
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
/// Subscription and topic permissions are checked when establishing the subscription.
/// The resulting tenant/node scope is fixed for its lifetime. Updates do not
/// revalidate the token; token expiry does not terminate an established subscription.
/// New `subscribe` and `unsubscribe` requests still require valid credentials.
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
#[rpc(server)]
pub(crate) trait SubscriptionRpc {
    /// Subscribe to signing state in the authenticated namespace.
    #[subscription(name = "subscribe", unsubscribe = "unsubscribe", item = fiber_json_types::SubscriptionEvent, with_extensions)]
    async fn subscribe(
        &self,
        topic: SubscriptionTopic,
        options: Option<SubscriptionOptions>,
    ) -> SubscriptionResult;
}

#[derive(Clone)]
pub(crate) struct SubscriptionRpcServerImpl<S> {
    store: S,
    lsp: Option<ractor::ActorRef<LspServiceMessage>>,
    service: SubscriptionService,
}

impl<S: RpcServerStore + Clone + Send + Sync + 'static> SubscriptionRpcServerImpl<S> {
    pub(crate) fn new(
        store: S,
        lsp: Option<ractor::ActorRef<LspServiceMessage>>,
        channels_enabled: bool,
        watchtower_enabled: bool,
    ) -> Self {
        Self {
            store,
            lsp,
            service: SubscriptionService::new(channels_enabled, watchtower_enabled),
        }
    }

    async fn stream(
        &self,
        extensions: &Extensions,
        topic: SubscriptionTopic,
        options: SubscriptionOptions,
    ) -> Result<EventStream> {
        // The middleware has already checked write("subscriptions"). Only the
        // selected topic's read policy and scope are resolved here, once.
        let method = match topic {
            SubscriptionTopic::ChannelSigning => "get_channel_signing_status",
            SubscriptionTopic::WatchtowerSigning => "get_watchtower_signing_status",
        };
        let (tenant, node) = {
            let authorization = extensions
                .get::<RpcAuthorization>()
                .ok_or_else(|| anyhow!("RPC authorization missing"))?;
            match authorization
                .authorize(method)
                .map_err(|_| anyhow!("Unauthorized"))?
            {
                Some((biscuit, _)) => (
                    extract_tenant_id(&biscuit)?,
                    if topic == SubscriptionTopic::WatchtowerSigning {
                        scoped_rpc_node_id(&biscuit)?
                    } else {
                        NodeId::local()
                    },
                ),
                None => (None, NodeId::local()),
            }
        };
        if let Some(tenant) = tenant {
            let actor = self
                .lsp
                .as_ref()
                .ok_or_else(|| anyhow!("hosted LSP service is not enabled"))?;
            let context = ractor::call_t!(
                actor,
                LspServiceMessage::GetTenantSubscriptionContext,
                5_000,
                tenant
            )
            .map_err(|_| anyhow!("hosted LSP service unavailable"))?
            .map_err(anyhow::Error::msg)?;
            if topic == SubscriptionTopic::WatchtowerSigning && node != context.node_id {
                bail!("watchtower identity does not match tenant");
            }
            self.service.subscribe(
                Source {
                    channels: context.store,
                    watchtower: context.watchtower_store,
                    node,
                },
                topic,
                options,
            )
        } else {
            self.service.subscribe(
                Source {
                    channels: self.store.clone(),
                    watchtower: self.store.clone(),
                    node,
                },
                topic,
                options,
            )
        }
    }
}

#[async_trait::async_trait]
impl<S: RpcServerStore + Clone + Send + Sync + 'static> SubscriptionRpcServer
    for SubscriptionRpcServerImpl<S>
{
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        extensions: &Extensions,
        topic: SubscriptionTopic,
        options: Option<SubscriptionOptions>,
    ) -> SubscriptionResult {
        let stream = self
            .stream(extensions, topic, options.unwrap_or_default())
            .await;
        let stream = match stream {
            Ok(stream) => stream,
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
        };
        // Credentials and RPC extensions are not retained by the event stream.
        let sink = pending.accept().await?;
        transport::forward(sink, stream)
            .await
            .map_err(|error| error.to_string().into())
    }
}

#[cfg(test)]
mod tests;
