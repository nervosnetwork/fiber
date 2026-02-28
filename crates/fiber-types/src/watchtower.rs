//! Watchtower types (feature-gated).
//!
//! Contains the data structures used by the watchtower service to monitor channels
//! and handle force-close scenarios.

use crate::channel::TLCId;
use crate::invoice::HashAlgorithm;
use crate::serde_utils::{CompactSignatureAsBytes, EntityHex};
use crate::{Hash256, Privkey, Pubkey};
use ckb_types::packed::{Bytes, CellOutput, Script};
use musig2::CompactSignature;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

// ============================================================
// RevocationData
// ============================================================

/// Data needed to revoke an outdated commitment transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RevocationData {
    /// The commitment transaction version number that was revoked
    pub commitment_number: u64,
    /// The aggregated signature from both parties that authorizes the revocation
    #[serde_as(as = "CompactSignatureAsBytes")]
    pub aggregated_signature: CompactSignature,
    /// The output cell from the revoked commitment transaction
    #[serde_as(as = "EntityHex")]
    pub output: CellOutput,
    /// The associated data for the output cell (e.g., UDT amount for token transfers)
    #[serde_as(as = "EntityHex")]
    pub output_data: Bytes,
}

// ============================================================
// SettlementData
// ============================================================

/// Data needed to authorize and execute a settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettlementData {
    /// The total amount of CKB/UDT being settled for the local party
    pub local_amount: u128,
    /// The total amount of CKB/UDT being settled for the remote party
    pub remote_amount: u128,
    /// The list of pending Time-Locked Contracts (TLCs) included in this settlement
    pub tlcs: Vec<SettlementTlc>,
}

// ============================================================
// SettlementTlc
// ============================================================

/// Data needed to authorize and execute a Time-Locked Contract (TLC) settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettlementTlc {
    /// The ID of the TLC (either offered or received)
    pub tlc_id: TLCId,
    /// The hash algorithm used for the TLC
    pub hash_algorithm: HashAlgorithm,
    /// The amount of CKB/UDT involved in the TLC
    pub payment_amount: u128,
    /// The hash of the payment preimage
    pub payment_hash: Hash256,
    /// The expiry time for the TLC in milliseconds
    pub expiry: u64,
    /// The local party's private key used to sign the TLC
    pub local_key: Privkey,
    /// The remote party's public key used to verify the TLC
    pub remote_key: Pubkey,
}

// ============================================================
// ChannelData
// ============================================================

/// The data of a channel that the watchtower is monitoring.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChannelData {
    /// The unique identifier of the channel
    pub channel_id: Hash256,
    /// The UDT type script if this is a UDT channel, None for CKB channels
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
    /// Settlement data for the remote commitment transaction
    pub remote_settlement_data: SettlementData,
    /// Pending settlement data for the remote commitment transaction
    /// (in case revocation hasn't been received yet)
    pub pending_remote_settlement_data: SettlementData,
    /// Settlement data for the local commitment transaction
    pub local_settlement_data: SettlementData,
    /// Data needed to revoke an outdated commitment transaction
    pub revocation_data: Option<RevocationData>,
}
