use ckb_sdk::util::blake160;
use ckb_types::packed::Script;
use musig2::{secp::Point, KeyAggContext};

use crate::{
    ckb::contracts::{get_script_by_contract, Contract},
    fiber::{
        channel::{RevocationData, SettlementData},
        types::{Hash256, NodeId, Privkey, Pubkey},
    },
};

// ChannelData is defined in fiber-types and re-exported from here via mod.rs
pub use fiber_types::ChannelData;

pub trait WatchtowerStore {
    /// Get the channels that are currently being watched by the watchtower
    fn get_watch_channels(&self) -> Vec<ChannelData>;
    /// Insert a channel's funding tx lock script into the store, it will be used to monitor the channel,
    /// please note that the lock script should be globally unique, so that the watchtower can identify the channel.
    #[allow(clippy::too_many_arguments)]
    fn insert_watch_channel(
        &self,
        node_id: NodeId,
        channel_id: Hash256,
        funding_udt_type_script: Option<Script>,
        local_settlement_key: Privkey,
        remote_settlement_key: Pubkey,
        local_funding_pubkey: Pubkey,
        remote_funding_pubkey: Pubkey,
        settlement_data: SettlementData,
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

    /// Mark a tlc as settled on chain
    fn update_tlc_settled(&self, channel_id: &Hash256, payment_hash: [u8; 20]);
}

/// Compute the x-only aggregated public key for a channel.
///
/// Free function replacement for `ChannelData::x_only_aggregated_pubkey()` because the method
/// depends on `musig2::KeyAggContext` which is used with `ckb_sdk` types not in fiber-types.
pub fn channel_data_x_only_aggregated_pubkey(cd: &ChannelData, for_remote: bool) -> [u8; 32] {
    let keys = if for_remote {
        vec![cd.remote_funding_pubkey, cd.local_funding_pubkey]
    } else {
        vec![cd.local_funding_pubkey, cd.remote_funding_pubkey]
    };

    KeyAggContext::new(keys)
        .map(|key_agg_ctx| key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly())
        .unwrap_or_default()
}

/// Compute the local settlement pubkey hash for a channel.
///
/// Free function replacement for `ChannelData::local_settlement_pubkey_hash()`.
pub fn channel_data_local_settlement_pubkey_hash(cd: &ChannelData) -> [u8; 20] {
    blake160(cd.local_settlement_key.pubkey().serialize().as_ref()).0
}

/// Compute the funding transaction lock script for a channel.
///
/// Free function replacement for `ChannelData::funding_tx_lock()`.
pub fn channel_data_funding_tx_lock(cd: &ChannelData) -> Script {
    let mut keys = [cd.local_funding_pubkey, cd.remote_funding_pubkey];
    keys.sort();

    let x_only_agg_pubkey = KeyAggContext::new(keys)
        .map(|key_agg_ctx| key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly())
        .unwrap_or_default();
    let pubkey_hash = blake160(&x_only_agg_pubkey);

    get_script_by_contract(Contract::FundingLock, pubkey_hash.as_bytes())
}
