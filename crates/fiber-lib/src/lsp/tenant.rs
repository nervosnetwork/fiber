use std::collections::HashSet;

use ckb_types::packed::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::fiber_types::{EntityHex, Hash256, NodeId, Pubkey};

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
    pub created_at: u64,
}

/// One private channel bound to a hosted tenant.
///
/// A Fiber channel carries exactly one asset, so a tenant uses one channel per
/// asset (and possibly more than one channel for the same asset later).
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TenantChannelRecord {
    pub channel_id: Hash256,
    /// `None` for CKB, `Some` for the channel's UDT type script.
    #[serde_as(as = "Option<EntityHex>")]
    pub funding_udt_type_script: Option<Script>,
    pub bound_at: u64,
}

/// Persistent tenant status plus in-process channel readiness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostedTenantChannelStatus {
    pub channel: TenantChannelRecord,
    pub online: bool,
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
    pub channels: Vec<HostedTenantChannelStatus>,
}

/// Select the channel that should receive a delivery for `asset`.
///
/// Preference order:
/// 1. an online channel with the requested asset;
/// 2. the lowest-channel-id bound channel with the requested asset (it may
///    come online and resume the delivery later).
///
/// A tenant with no channel for the requested asset is rejected rather than
/// silently routing over a channel denominated in another asset.
pub(crate) fn select_tenant_channel<'a>(
    channels: &'a [TenantChannelRecord],
    ready: Option<&HashSet<Hash256>>,
    asset: Option<&Script>,
) -> Result<&'a TenantChannelRecord, String> {
    let mut matching: Vec<&TenantChannelRecord> = channels
        .iter()
        .filter(|channel| channel.funding_udt_type_script.as_ref() == asset)
        .collect();
    if matching.is_empty() {
        return Err("tenant has no private channel for the requested asset".to_string());
    }
    matching.sort_by_key(|channel| channel.channel_id);
    if let Some(ready) = ready {
        if let Some(channel) = matching
            .iter()
            .find(|channel| ready.contains(&channel.channel_id))
        {
            return Ok(*channel);
        }
    }
    Ok(matching[0])
}
