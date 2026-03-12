use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber_types::ChannelState;
use fiber_types::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Restore Audit Map: Records a snapshot of all channel states at the moment nodes are restored from backup.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreAuditMap {
    /// Map of ChannelId (Hash256) and ChannelAuditInfo
    pub channels: HashMap<Hash256, ChannelAuditInfo>,
}

/// Channel Audit Information: Reference Evidence for Peer Data Comparison
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ChannelAuditInfo {
    /// Local Commitment Number
    pub local_commitment_number: u64,
}

impl RestoreAuditMap {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    pub fn add_channel(&mut self, channel_id: Hash256, info: ChannelAuditInfo) {
        self.channels.insert(channel_id, info);
    }

    pub fn build_from_store<S: ChannelActorStateStore>(store: &S) -> Self {
        let mut audit_map = Self::new();

        for channel in store.get_all_channel_states() {
            if is_risk_of_penalty(&channel.state) {
                audit_map.add_channel(
                    channel.id,
                    ChannelAuditInfo {
                        local_commitment_number: channel.commitment_numbers.get_local(),
                    },
                );
            }
        }
        audit_map
    }
}

fn is_risk_of_penalty(state: &ChannelState) -> bool {
    !matches!(
        state,
        ChannelState::NegotiatingFunding(_)
            | ChannelState::CollaboratingFundingTx(_)
            | ChannelState::Closed(_)
    )
}

pub trait RestoreAuditStore {
    fn get_restore_audit_map(&self) -> Option<RestoreAuditMap>;
    fn insert_restore_audit_map(&self, map: RestoreAuditMap);
    fn delete_restore_audit_map(&self);
    fn resolve_channel_audit(&self, channel_id: &Hash256);
}
