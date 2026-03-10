//! Development/debug types for the Fiber Network JSON-RPC API.

#[cfg(feature = "cli")]
use fiber_cli_derive::CliArgs;

use crate::invoice::HashAlgorithm;
use crate::serde_utils::{Hash256, U128Hex, U64Hex};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Parameters for sending a commitment_signed message.
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct CommitmentSignedParams {
    /// The channel ID of the channel to send the commitment_signed message to
    pub channel_id: Hash256,
}

/// Parameters for adding a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
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
    #[cfg_attr(feature = "cli", cli(serde_enum))]
    pub hash_algorithm: Option<HashAlgorithm>,
}

/// Result of adding a TLC.
#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct AddTlcResult {
    /// The ID of the TLC
    #[serde_as(as = "U64Hex")]
    pub tlc_id: u64,
}

/// Parameters for removing a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct RemoveTlcParams {
    /// The channel ID of the channel to remove the TLC from
    pub channel_id: Hash256,
    #[serde_as(as = "U64Hex")]
    /// The ID of the TLC to remove
    pub tlc_id: u64,
    /// The reason for removing the TLC, either a 32-byte hash for preimage fulfillment or an u32 error code for removal
    #[cfg_attr(feature = "cli", cli(json))]
    pub reason: RemoveTlcReason,
}

/// The reason for removing a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum RemoveTlcReason {
    /// The reason for removing the TLC is that it was fulfilled
    RemoveTlcFulfill { payment_preimage: Hash256 },
    /// The reason for removing the TLC is that it failed
    RemoveTlcFail { error_code: String },
}

/// Parameters for submitting a commitment transaction.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct SubmitCommitmentTransactionParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Commitment number
    #[serde_as(as = "U64Hex")]
    pub commitment_number: u64,
}

/// Result of submitting a commitment transaction.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubmitCommitmentTransactionResult {
    /// Submitted commitment transaction hash
    pub tx_hash: Hash256,
}

/// Parameters for checking channel shutdown.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "cli", derive(CliArgs))]
pub struct CheckChannelShutdownParams {
    /// Channel ID
    pub channel_id: Hash256,
}
