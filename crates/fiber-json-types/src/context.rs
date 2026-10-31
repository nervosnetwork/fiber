//! RPC context types for the Fiber Network JSON-RPC API.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// RPC context for watchtower operations.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RpcContext {
    /// Node ID (base58 encoded), read from user RPC biscuit token
    pub node_id: String,
    /// True when the request was authenticated as a hosted tenant.
    /// Tenant tokens may only address their issued `node(...)` namespace.
    #[serde(default)]
    pub tenant_scoped: bool,
}
