//! Watchtower types (feature-gated).
//!
//! Contains the data structures used by the watchtower service to monitor channels
//! and handle force-close scenarios.

use crate::channel::{ChannelFeatures, TLCId};
use crate::invoice::HashAlgorithm;
use crate::serde_utils::{CompactSignatureAsBytes, EntityHex};
use crate::{Hash256, Privkey, Pubkey};
use ckb_types::packed::{Bytes, CellOutput, Script};
use musig2::CompactSignature;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

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

/// Data needed to authorize and execute a settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettlementData {
    /// The total amount of CKB/UDT being settled for the local party
    pub local_amount: u128,
    /// The total amount of CKB/UDT being settled for the remote party
    pub remote_amount: u128,
    /// The list of pending Time-Locked Contracts (TLCs) included in this settlement
    pub tlcs: Vec<SettlementTlc>,
}

/// Data needed to authorize and execute a Time-Locked Contract (TLC) settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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

/// Encode a TLC using the channel's exact on-chain witness layout.
pub fn settlement_tlc_witness(
    tlc: &SettlementTlc,
    for_remote: bool,
    features: ChannelFeatures,
) -> Vec<u8> {
    let mut bytes = vec![((tlc.hash_algorithm as u8) << 1) + u8::from(tlc.tlc_id.is_received())];
    bytes.extend_from_slice(&tlc.payment_amount.to_le_bytes());
    bytes.extend_from_slice(&tlc.payment_hash.as_ref()[..features.payment_hash_len()]);
    let local = tlc.local_key.pubkey();
    let keys = if for_remote {
        [tlc.remote_key, local]
    } else {
        [local, tlc.remote_key]
    };
    for key in keys {
        bytes.extend_from_slice(&ckb_hash::blake2b_256(key.serialize())[..20]);
    }
    // CKB absolute timestamp since, in seconds.
    bytes.extend_from_slice(&(0x4000000000000000u64 | (tlc.expiry / 1000)).to_le_bytes());
    bytes
}

/// Encode the settlement snapshot committed by the commitment-lock args.
pub fn settlement_data_witness(
    data: &SettlementData,
    for_remote: bool,
    features: ChannelFeatures,
    local: Pubkey,
    remote: Pubkey,
) -> Result<Vec<u8>, String> {
    let len = u8::try_from(data.tlcs.len())
        .map_err(|_| "TLC count exceeds witness encoding limit (max 255)")?;
    let mut bytes = vec![len];
    for tlc in &data.tlcs {
        bytes.extend_from_slice(&settlement_tlc_witness(tlc, for_remote, features));
    }
    let sides = if for_remote {
        [(remote, data.remote_amount), (local, data.local_amount)]
    } else {
        [(local, data.local_amount), (remote, data.remote_amount)]
    };
    for (key, amount) in sides {
        bytes.extend_from_slice(&ckb_hash::blake2b_256(key.serialize())[..20]);
        bytes.extend_from_slice(&amount.to_le_bytes());
    }
    Ok(bytes)
}

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
    /// The commitment-lock features used by this channel.
    #[serde(default)]
    pub channel_features: ChannelFeatures,
}
