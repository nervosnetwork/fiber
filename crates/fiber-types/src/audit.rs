use crate::Hash256;
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

pub trait RestoreAuditStore {
    fn get_restore_audit_map(&self) -> Option<RestoreAuditMap>;
    fn insert_restore_audit_map(&self, map: RestoreAuditMap);
    fn delete_restore_audit_map(&self);
    fn resolve_channel_audit(&self, channel_id: &Hash256);
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
}
