//! Dev/debug RPC types for the Fiber Network Node.

use crate::invoice::HashAlgorithm;
use crate::serde_utils::{U128Hex, U64Hex};
use crate::Hash256;

use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[derive(Serialize, Deserialize, Debug)]
pub struct CommitmentSignedParams {
    /// The channel ID of the channel to send the commitment_signed message to
    pub channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct AddTlcParams {
    /// The channel ID of the channel to add the TLC to
    pub channel_id: Hash256,
    /// The amount of the TLC
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The payment hash of the TLC
    pub payment_hash: Hash256,
    /// The expiry of the TLC
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// The hash algorithm of the TLC
    pub hash_algorithm: Option<HashAlgorithm>,
}

#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct AddTlcResult {
    /// The ID of the TLC
    #[serde_as(as = "U64Hex")]
    pub tlc_id: u64,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoveTlcParams {
    /// The channel ID of the channel to remove the TLC from
    pub channel_id: Hash256,
    #[serde_as(as = "U64Hex")]
    /// The ID of the TLC to remove
    pub tlc_id: u64,
    /// The reason for removing the TLC
    pub reason: RemoveTlcReason,
}

/// The reason for removing a TLC
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum RemoveTlcReason {
    /// The reason for removing the TLC is that it was fulfilled
    RemoveTlcFulfill { payment_preimage: Hash256 },
    /// The reason for removing the TLC is that it failed
    RemoveTlcFail { error_code: String },
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubmitCommitmentTransactionParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Commitment number
    #[serde_as(as = "U64Hex")]
    pub commitment_number: u64,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubmitCommitmentTransactionResult {
    /// Submitted commitment transaction hash
    pub tx_hash: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckChannelShutdownParams {
    /// Channel ID
    pub channel_id: Hash256,
}
