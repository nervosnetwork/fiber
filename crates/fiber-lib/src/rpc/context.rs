use crate::fiber::types::Hash256;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcContext {
    /// Node ID, read from user RPC biscuit token
    pub node_id: Option<Hash256>,
}
