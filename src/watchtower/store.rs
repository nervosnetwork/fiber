use ckb_types::packed::{Bytes, CellOutput, Script};
use musig2::CompactSignature;

use crate::fiber::types::Hash256;

pub trait WatchtowerStore {
    /// Get the channels that are currently being watched by the watchtower
    fn get_channels(&self) -> Vec<ChannelData>;
    /// Insert a channel's funding tx lock script into the store, it will be used to monitor the channel,
    /// please note that the lock script should be globally unique, so that the watchtower can identify the channel.
    fn insert_channel(&self, channel_id: Hash256, funding_tx_lock: Script);
    /// Remove a channel from the store, the watchtower will stop monitoring the channel
    fn remove_channel(&self, channel_id: Hash256);
    /// Update the revocation data of a channel, the watchtower will use this data to revoke an old version commitment transaction
    fn update_revocation(&self, channel_id: Hash256, revocation_data: RevocationData);
}

/// The data of a channel that the watchtower is monitoring
#[derive(Debug)]
pub struct ChannelData {
    pub channel_id: Hash256,
    pub funding_tx_lock: Script,
    pub revocation_data: Option<RevocationData>,
}

#[derive(Debug)]
pub struct RevocationData {
    pub commitment_number: u64,
    pub x_only_aggregated_pubkey: [u8; 32],
    pub aggregated_signature: CompactSignature,
    pub output: CellOutput,
    pub output_data: Bytes,
}
