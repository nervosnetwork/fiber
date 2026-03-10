//! Watchtower types for the Fiber Network JSON-RPC API.

use crate::invoice::HashAlgorithm;
use crate::schema_helpers::*;
use crate::serde_utils::{EntityHex, Hash256, Privkey, Pubkey, SliceHex, U128Hex, U64Hex};
use ckb_jsonrpc_types::Script;
use ckb_types::packed::{Bytes, CellOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// The id of a TLC, it can be either offered or received.
#[serde_as]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum TLCId {
    /// Offered TLC id
    Offered(
        #[serde_as(as = "U64Hex")]
        #[schemars(schema_with = "schema_as_uint_hex")]
        u64,
    ),
    /// Received TLC id
    Received(
        #[serde_as(as = "U64Hex")]
        #[schemars(schema_with = "schema_as_uint_hex")]
        u64,
    ),
}

/// Data needed to authorize and execute a Time-Locked Contract (TLC) settlement transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SettlementTlc {
    /// The ID of the TLC (either offered or received)
    pub tlc_id: TLCId,
    /// The hash algorithm used for the TLC
    pub hash_algorithm: HashAlgorithm,
    /// The amount of CKB/UDT involved in the TLC
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub payment_amount: u128,
    /// The hash of the payment preimage
    pub payment_hash: Hash256,
    /// The expiry time for the TLC in milliseconds
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expiry: u64,
    /// The local party's private key used to sign the TLC (hex without 0x prefix)
    pub local_key: Privkey,
    /// The remote party's public key used to verify the TLC (hex without 0x prefix)
    pub remote_key: Pubkey,
}

/// Data needed to authorize and execute a settlement transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SettlementData {
    /// The total amount of CKB/UDT being settled for the local party
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub local_amount: u128,
    /// The total amount of CKB/UDT being settled for the remote party
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub remote_amount: u128,
    /// The list of pending Time-Locked Contracts (TLCs) included in this settlement
    pub tlcs: Vec<SettlementTlc>,
}

/// Data needed to revoke an outdated commitment transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RevocationData {
    /// The commitment transaction version number that was revoked
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub commitment_number: u64,
    /// The aggregated signature from both parties that authorizes the revocation (hex string, 64 bytes)
    #[serde_as(as = "SliceHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub aggregated_signature: Vec<u8>,
    /// The output cell from the revoked commitment transaction (hex-encoded molecule bytes)
    #[serde_as(as = "EntityHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub output: CellOutput,
    /// The associated data for the output cell (e.g., UDT amount for token transfers, hex-encoded molecule bytes)
    #[serde_as(as = "EntityHex")]
    #[schemars(schema_with = "schema_as_hex_bytes")]
    pub output_data: Bytes,
}

/// Parameters for creating a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct CreateWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Funding UDT type script
    pub funding_udt_type_script: Option<Script>,
    /// The local party's private key used to settle the commitment transaction (hex without 0x prefix)
    pub local_settlement_key: Privkey,
    /// The remote party's public key used to settle the commitment transaction (hex without 0x prefix)
    pub remote_settlement_key: Pubkey,
    /// The local party's funding public key (hex without 0x prefix)
    pub local_funding_pubkey: Pubkey,
    /// The remote party's funding public key (hex without 0x prefix)
    pub remote_funding_pubkey: Pubkey,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for removing a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RemoveWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
}

/// Parameters for updating revocation.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct UpdateRevocationParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Revocation data
    pub revocation_data: RevocationData,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for updating pending remote settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct UpdatePendingRemoteSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for updating local settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct UpdateLocalSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data
    pub settlement_data: SettlementData,
}

/// Parameters for creating a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct CreatePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
    /// Preimage
    pub preimage: Hash256,
}

/// Parameters for removing a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RemovePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct QueryTlcStatusParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Payment hash
    pub payment_hash: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct QueryTlcStatusResult {
    /// Found preimage when the TLC has been settled on chain with the preimage path.
    pub preimage: Option<Hash256>,
    /// Whether the TLC has been settled on chain
    pub is_settled: bool,
}
