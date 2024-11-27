use ckb_types::packed::{Bytes, CellOutput, Script};
use musig2::CompactSignature;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::fiber::{serde_utils::CompactSignatureAsBytes, serde_utils::EntityHex, types::Hash256};

pub trait WatchtowerStore {
    /// Get the channels that are currently being watched by the watchtower
    fn get_watch_channels(&self) -> Vec<ChannelData>;
    /// Insert a channel's funding tx lock script into the store, it will be used to monitor the channel,
    /// please note that the lock script should be globally unique, so that the watchtower can identify the channel.
    fn insert_watch_channel(&self, channel_id: Hash256, funding_tx_lock: Script);
    /// Remove a channel from the store, the watchtower will stop monitoring the channel
    fn remove_watch_channel(&self, channel_id: Hash256);
    /// Update the revocation data of a channel, the watchtower will use this data to revoke an old version commitment transaction
    fn update_revocation(&self, channel_id: Hash256, revocation_data: RevocationData);
}

/// The data of a channel that the watchtower is monitoring
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChannelData {
    pub channel_id: Hash256,
    #[serde_as(as = "EntityHex")]
    pub funding_tx_lock: Script,
    pub revocation_data: Option<RevocationData>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RevocationData {
    pub commitment_number: u64,
    pub x_only_aggregated_pubkey: [u8; 32],
    #[serde_as(as = "CompactSignatureAsBytes")]
    pub aggregated_signature: CompactSignature,
    #[serde_as(as = "EntityHex")]
    pub output: CellOutput,
    #[serde_as(as = "EntityHex")]
    pub output_data: Bytes,
}
