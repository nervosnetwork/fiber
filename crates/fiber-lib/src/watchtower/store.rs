use ckb_sdk::util::blake160;
use ckb_types::packed::Script;
use musig2::{secp::Point, KeyAggContext};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    ckb::contracts::{get_script_by_contract, Contract},
    fiber::{
        channel::{RevocationData, SettlementData},
        serde_utils::EntityHex,
        types::{Hash256, NodeId, Privkey, Pubkey},
    },
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
        funding_udt_type_script: Option<Script>,
        local_settlement_key: Privkey,
        remote_settlement_key: Pubkey,
        local_funding_pubkey: Pubkey,
        remote_funding_pubkey: Pubkey,
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

    /// Update the pending settlement data of a channel, the watchtower will use this data to settle the remote commitment transaction of a force closed channel
    fn update_pending_remote_settlement(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        pending_remote_settlement_data: SettlementData,
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
    #[serde_as(as = "Option<EntityHex>")]
    pub funding_udt_type_script: Option<Script>,
    /// The local party's private key used to settle the commitment transaction
    pub local_settlement_key: Privkey,
    /// The remote party's public key used to settle the commitment transaction
    pub remote_settlement_key: Pubkey,
    /// The local party's funding public key
    pub local_funding_pubkey: Pubkey,
    /// The remote party's funding public key
    pub remote_funding_pubkey: Pubkey,
    pub remote_settlement_data: SettlementData,
    // TODO: @quake more test
    // There might be a corner case that the remote node has received the commitment message from the local node,
    // but the local node hasn't received the revocation message from the remote node yet, then the remote node
    // may force close the channel with the latest commitment transaction, in this case, the watchtower
    // should have pending_remote_settlement_data to settle the remote commitment transaction.
    pub pending_remote_settlement_data: SettlementData,
    pub local_settlement_data: Option<SettlementData>,
    pub revocation_data: Option<RevocationData>,
}

impl ChannelData {
    pub fn x_only_aggregated_pubkey(&self, for_remote: bool) -> [u8; 32] {
        let keys = if for_remote {
            vec![self.remote_funding_pubkey, self.local_funding_pubkey]
        } else {
            vec![self.local_funding_pubkey, self.remote_funding_pubkey]
        };

        KeyAggContext::new(keys)
            .map(|key_agg_ctx| key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly())
            .unwrap_or_default()
    }

    pub fn local_settlement_pubkey_hash(&self) -> [u8; 20] {
        blake160(self.local_settlement_key.pubkey().serialize().as_ref()).0
    }

    pub fn funding_tx_lock(&self) -> Script {
        let mut keys = [self.local_funding_pubkey, self.remote_funding_pubkey];
        keys.sort();

        let x_only_agg_pubkey = KeyAggContext::new(keys)
            .map(|key_agg_ctx| key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly())
            .unwrap_or_default();
        let pubkey_hash = blake160(&x_only_agg_pubkey);

        get_script_by_contract(Contract::FundingLock, pubkey_hash.as_bytes())
    }
}
