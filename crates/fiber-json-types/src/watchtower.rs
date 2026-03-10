//! Watchtower types for the Fiber Network JSON-RPC API.

#[cfg(feature = "cli")]
use fiber_cli_derive::CliArgs;

use crate::invoice::HashAlgorithm;
use crate::serde_utils::{EntityHex, Hash256, Privkey, Pubkey, SliceHex, U128Hex, U64Hex};
use ckb_jsonrpc_types::Script;
use ckb_types::packed::{Bytes, CellOutput};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// The id of a TLC, it can be either offered or received.
#[serde_as]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TLCId {
    /// Offered TLC id
    Offered(#[serde_as(as = "U64Hex")] u64),
    /// Received TLC id
    Received(#[serde_as(as = "U64Hex")] u64),
}

/// Data needed to authorize and execute a Time-Locked Contract (TLC) settlement transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettlementTlc {
    /// The ID of the TLC (either offered or received)
    pub tlc_id: TLCId,
    /// The hash algorithm used for the TLC
    pub hash_algorithm: HashAlgorithm,
    /// The amount of CKB/UDT involved in the TLC
    #[serde_as(as = "U128Hex")]
    pub payment_amount: u128,
    /// The hash of the payment preimage
    pub payment_hash: Hash256,
    /// The expiry time for the TLC in milliseconds
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// The local party's private key used to sign the TLC (hex without 0x prefix)
    pub local_key: Privkey,
    /// The remote party's public key used to verify the TLC (hex without 0x prefix)
    pub remote_key: Pubkey,
}

/// Data needed to authorize and execute a settlement transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettlementData {
    /// The total amount of CKB/UDT being settled for the local party
    #[serde_as(as = "U128Hex")]
    pub local_amount: u128,
    /// The total amount of CKB/UDT being settled for the remote party
    #[serde_as(as = "U128Hex")]
    pub remote_amount: u128,
    /// The list of pending Time-Locked Contracts (TLCs) included in this settlement
    pub tlcs: Vec<SettlementTlc>,
}

/// Data needed to revoke an outdated commitment transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevocationData {
    /// The commitment transaction version number that was revoked
    #[serde_as(as = "U64Hex")]
    pub commitment_number: u64,
    /// The aggregated signature from both parties that authorizes the revocation (hex string, 64 bytes)
    #[serde_as(as = "SliceHex")]
    pub aggregated_signature: Vec<u8>,
    /// The output cell from the revoked commitment transaction (hex-encoded molecule bytes)
    #[serde_as(as = "EntityHex")]
    pub output: CellOutput,
    /// The associated data for the output cell (e.g., UDT amount for token transfers, hex-encoded molecule bytes)
    #[serde_as(as = "EntityHex")]
    pub output_data: Bytes,
}

/// Parameters for creating a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct CreateWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Funding UDT type script
    #[cfg_attr(feature = "cli", cli(json))]
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
    #[cfg_attr(feature = "cli", cli(json))]
    pub settlement_data: SettlementData,
}

/// Parameters for removing a watch channel.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct RemoveWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
}

/// Parameters for updating revocation.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct UpdateRevocationParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Revocation data
    #[cfg_attr(feature = "cli", cli(json))]
    pub revocation_data: RevocationData,
    /// Settlement data
    #[cfg_attr(feature = "cli", cli(json))]
    pub settlement_data: SettlementData,
}

/// Parameters for updating pending remote settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct UpdatePendingRemoteSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data
    #[cfg_attr(feature = "cli", cli(json))]
    pub settlement_data: SettlementData,
}

/// Parameters for updating local settlement.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct UpdateLocalSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data
    #[cfg_attr(feature = "cli", cli(json))]
    pub settlement_data: SettlementData,
}

/// Parameters for creating a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct CreatePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
    /// Preimage
    pub preimage: Hash256,
}

/// Parameters for removing a preimage.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct RemovePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
}
