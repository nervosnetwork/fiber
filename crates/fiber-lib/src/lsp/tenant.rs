use serde::{Deserialize, Serialize};

use crate::fiber_types::{Hash256, NodeId, Pubkey};

/// Watchtower `node_id` for a hosted tenant Fiber identity.
///
/// Matches the biscuit `node(...)` fact issued on the tenant access token so
/// get/submit watchtower RPCs read the same key the runtime writes.
pub fn tenant_watchtower_node_id(tenant_pubkey: &Pubkey) -> NodeId {
    NodeId::from_bytes(tenant_pubkey.serialize().to_vec())
}

pub use crate::fiber_types::TenantId;

/// Persisted state boundary of a hosted tenant. Runtime liveness is
/// intentionally excluded because it is rebuilt by the supervisor after
/// process restart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostedTenantRecord {
    pub tenant_id: TenantId,
    /// RootSigner identity that owns this tenant. Legacy operator-provisioned
    /// records may omit it until they are migrated through authenticated
    /// registration.
    #[serde(default)]
    pub root_signer_pubkey: Option<Pubkey>,
    /// Fiber protocol key used by the tenant side of its private channel and
    /// to authenticate its invoices. Public T uses it only for local peer
    /// addressing; it is never announced as a gossip-routable node identity.
    pub tenant_pubkey: Pubkey,
    /// Private channel bound to this tenant. This identifies channel state,
    /// not the tenant's authorization or transport endpoint.
    pub private_channel_id: Option<Hash256>,
    pub created_at: u64,
}

/// In-process lifecycle of a hosted tenant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenantRuntimeStatus {
    Cold,
    Active,
}

/// Combined persistent and in-process status returned by the LSP service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostedTenantStatus {
    pub record: HostedTenantRecord,
    pub runtime_status: TenantRuntimeStatus,
    pub channel_online: bool,
}
