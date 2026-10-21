//! Topics and payloads for the WebSocket `subscribe` / `unsubscribe` RPCs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "watchtower")]
use crate::GetWatchtowerSigningStatusResult;
use crate::{GetChannelSigningStatusResult, Hash256};

/// A subscription topic. Each topic uses the corresponding query RPC's authorization.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionTopic {
    /// Current channel signing state in the authenticated node namespace.
    ChannelSigning,
    /// Current signing state in the authenticated watchtower namespace.
    WatchtowerSigning,
}

/// Optional filters for `subscribe(topic, options)`; identity is always server-derived.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionOptions {
    /// Restrict updates to these channels. Omission selects all authorized channels.
    pub channel_ids: Option<Vec<Hash256>>,
    /// Send current pending requests followed by `snapshot_complete` before live updates.
    #[serde(default)]
    pub include_snapshot: bool,
}

/// A typed state update. Reconnect and request a new snapshot to recover pending work.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SubscriptionEvent {
    /// Channel status, identical to `get_channel_signing_status`.
    ChannelSigning(Box<GetChannelSigningStatusResult>),
    /// Watchtower status, identical to `get_watchtower_signing_status`.
    #[cfg(feature = "watchtower")]
    WatchtowerSigning(Box<GetWatchtowerSigningStatusResult>),
    /// All currently pending requests in the initial snapshot have been sent.
    SnapshotComplete,
    /// A previously visible channel was removed from this topic.
    Removed {
        /// Identifier of the removed channel.
        channel_id: Hash256,
    },
}
