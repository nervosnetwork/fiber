use ckb_types::packed::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::fiber::{
    channel::{RevocationData, SettlementData},
    serde_utils::EntityHex,
    types::{Hash256, NodeId},
};

pub trait WatchtowerStore {
    /// Get the channels that are currently being watched by the watchtower
    fn get_watch_channels(&self) -> Vec<ChannelData>;
    /// Insert a channel's funding tx lock script into the store, it will be used to monitor the channel,
    /// please note that the lock script should be globally unique, so that the watchtower can identify the channel.
    fn insert_watch_channel(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        funding_tx_lock: Script,
        remote_settlement_data: SettlementData,
    );
    /// Remove a channel from the store, the watchtower will stop monitoring the channel
    fn remove_watch_channel(&self, node_id: NodeId, channel_id: Hash256);
    /// Update the revocation data of a channel, the watchtower will use this data to revoke an old version commitment transaction and settle the remote commitment transaction of a force closed channel
    fn update_revocation(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        revocation_data: RevocationData,
        remote_settlement_data: SettlementData,
    );
    /// Update the settlement data of a channel, the watchtower will use this data to settle the local commitment transaction of a force closed channel
    fn update_local_settlement(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        local_settlement_data: SettlementData,
    );

    /// Insert a watch preimage into the store, the payment hash should be a 32 bytes hash result of the preimage after `HashAlgorithm` is applied.
    fn insert_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256, preimage: Hash256);

    /// Remove a watch preimage from the store.
    fn remove_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256);

    /// Insert a watch preimage into the store, the payment hash should be a 32 bytes hash result of the preimage after `HashAlgorithm` is applied.
    fn get_watch_preimage(&self, payment_hash: &Hash256) -> Option<Hash256>;

    /// Search for the stored preimage with the given payment hash prefix, should be the first 20 bytes of the payment hash.
    fn search_preimage(&self, payment_hash_prefix: &[u8]) -> Option<Hash256>;
}

/// The data of a channel that the watchtower is monitoring
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChannelData {
    pub channel_id: Hash256,
    #[serde_as(as = "EntityHex")]
    pub funding_tx_lock: Script,
    pub remote_settlement_data: SettlementData,
    pub local_settlement_data: Option<SettlementData>,
    pub revocation_data: Option<RevocationData>,
}
