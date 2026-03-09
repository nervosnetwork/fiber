//! RPC context types for the Fiber Network JSON-RPC API.

use serde::{Deserialize, Serialize};

/// RPC context for watchtower operations.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcContext {
    /// Node ID (base58 encoded), read from user RPC biscuit token
    pub node_id: String,
}
