use ckb_types::packed::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::fiber::{
    channel::{RevocationData, SettlementData},
    serde_utils::EntityHex,
    types::Hash256,
};

pub trait WatchtowerStore {
    /// Get the channels that are currently being watched by the watchtower
    fn get_watch_channels(&self) -> Vec<ChannelData>;
    /// Insert a channel's funding tx lock script into the store, it will be used to monitor the channel,
    /// please note that the lock script should be globally unique, so that the watchtower can identify the channel.
    fn insert_watch_channel(
        &self,
        channel_id: Hash256,
        funding_tx_lock: Script,
        remote_settlement_data: SettlementData,
    );
    /// Remove a channel from the store, the watchtower will stop monitoring the channel
    fn remove_watch_channel(&self, channel_id: Hash256);
    /// Update the revocation data of a channel, the watchtower will use this data to revoke an old version commitment transaction and settle the remote commitment transaction of a force closed channel
    fn update_revocation(
        &self,
        channel_id: Hash256,
        revocation_data: RevocationData,
        remote_settlement_data: SettlementData,
    );
    /// Update the settlement data of a channel, the watchtower will use this data to settle the local commitment transaction of a force closed channel
    fn update_local_settlement(&self, channel_id: Hash256, local_settlement_data: SettlementData);
}

/// Used for delegating the store trait
pub trait WatchtowerStoreDeref {
    type Target: WatchtowerStore;
    fn watchtower_store_deref(&self) -> &Self::Target;
}

impl<T: WatchtowerStoreDeref> WatchtowerStore for T {
    fn get_watch_channels(&self) -> Vec<ChannelData> {
        self.watchtower_store_deref().get_watch_channels()
    }

    fn insert_watch_channel(
        &self,
        channel_id: Hash256,
        funding_tx_lock: Script,
        remote_settlement_data: SettlementData,
    ) {
        self.watchtower_store_deref().insert_watch_channel(
            channel_id,
            funding_tx_lock,
            remote_settlement_data,
        );
    }

    fn remove_watch_channel(&self, channel_id: Hash256) {
        self.watchtower_store_deref()
            .remove_watch_channel(channel_id);
    }

    fn update_revocation(
        &self,
        channel_id: Hash256,
        revocation_data: RevocationData,
        remote_settlement_data: SettlementData,
    ) {
        self.watchtower_store_deref().update_revocation(
            channel_id,
            revocation_data,
            remote_settlement_data,
        );
    }

    fn update_local_settlement(&self, channel_id: Hash256, local_settlement_data: SettlementData) {
        self.watchtower_store_deref()
            .update_local_settlement(channel_id, local_settlement_data);
    }
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
