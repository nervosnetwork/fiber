//! Watchtower types for the Fiber Network JSON-RPC API.

use crate::serde_utils::{Hash256, Pubkey};
use ckb_jsonrpc_types::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Parameters for creating a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Funding UDT type script
    pub funding_udt_type_script: Option<Script>,
    /// The local party's private key used to settle the commitment transaction (hex string)
    pub local_settlement_key: String,
    /// The remote party's public key used to settle the commitment transaction (hex without 0x prefix)
    pub remote_settlement_key: Pubkey,
    /// The local party's funding public key (hex without 0x prefix)
    pub local_funding_pubkey: Pubkey,
    /// The remote party's funding public key (hex without 0x prefix)
    pub remote_funding_pubkey: Pubkey,
    /// Settlement data (serialized)
    pub settlement_data: serde_json::Value,
}

/// Parameters for removing a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoveWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
}

/// Parameters for updating revocation.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateRevocationParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Revocation data (serialized)
    pub revocation_data: serde_json::Value,
    /// Settlement data (serialized)
    pub settlement_data: serde_json::Value,
}

/// Parameters for updating pending remote settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdatePendingRemoteSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data (serialized)
    pub settlement_data: serde_json::Value,
}

/// Parameters for updating local settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateLocalSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data (serialized)
    pub settlement_data: serde_json::Value,
}

/// Parameters for creating a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreatePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
    /// Preimage
    pub preimage: Hash256,
}

/// Parameters for removing a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemovePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
}
