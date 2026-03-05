use fiber_types::NodeId;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RpcContext {
    /// Node ID, read from user RPC biscuit token
    #[schemars(schema_with = "crate::rpc::schema_as_string")]
    pub node_id: NodeId,
}
