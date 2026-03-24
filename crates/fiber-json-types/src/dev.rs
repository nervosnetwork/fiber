//! Development/debug types for the Fiber Network JSON-RPC API.

use crate::invoice::HashAlgorithm;
use crate::schema_helpers::*;
use crate::serde_utils::{Hash256, U128Hex, U64Hex};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Parameters for sending a commitment_signed message.
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
pub struct CommitmentSignedParams {
    /// The channel ID of the channel to send the commitment_signed message to
    pub channel_id: Hash256,
}

/// Parameters for adding a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, JsonSchema)]
pub struct AddTlcParams {
    /// The channel ID of the channel to add the TLC to
    pub channel_id: Hash256,
    /// The amount of the TLC
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// The payment hash of the TLC
    pub payment_hash: Hash256,
    /// The expiry of the TLC
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expiry: u64,
    /// The hash algorithm of the TLC
    pub hash_algorithm: Option<HashAlgorithm>,
}

/// Result of adding a TLC.
#[serde_as]
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddTlcResult {
    /// The ID of the TLC
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub tlc_id: u64,
}

/// Parameters for removing a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct RemoveTlcParams {
    /// The channel ID of the channel to remove the TLC from
    pub channel_id: Hash256,
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    /// The ID of the TLC to remove
    pub tlc_id: u64,
    /// The reason for removing the TLC, either a 32-byte hash for preimage fulfillment or an u32 error code for removal
    pub reason: RemoveTlcReason,
}

/// The reason for removing a TLC.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(untagged)]
pub enum RemoveTlcReason {
    /// The reason for removing the TLC is that it was fulfilled
    RemoveTlcFulfill { payment_preimage: Hash256 },
    /// The reason for removing the TLC is that it failed
    RemoveTlcFail { error_code: String },
}

/// Parameters for submitting a commitment transaction.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SubmitCommitmentTransactionParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Commitment number
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub commitment_number: u64,
}

/// Result of submitting a commitment transaction.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SubmitCommitmentTransactionResult {
    /// Submitted commitment transaction hash
    pub tx_hash: Hash256,
}

/// Parameters for checking channel shutdown.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct CheckChannelShutdownParams {
    /// Channel ID
    pub channel_id: Hash256,
}

/// Parameters for signing an external funding transaction.
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SignExternalFundingTxParams {
    /// The unsigned funding transaction returned from `open_channel_with_external_funding`.
    pub unsigned_funding_tx: ckb_jsonrpc_types::Transaction,
    /// The private key to sign the transaction, as a 0x-prefixed 32-byte hex string.
    /// Note: This is a development-only RPC and the private key is provided directly.
    pub private_key: String,
    /// The lock scripts for the transaction inputs that need to be signed.
    /// Each tuple contains: (input_index, lock_script).
    /// The lock script should be the secp256k1 sighash script corresponding to the private key.
    pub input_lock_scripts: Vec<(u32, ckb_jsonrpc_types::Script)>,
}

/// Result of signing an external funding transaction.
#[derive(Clone, Serialize, Deserialize, Debug, JsonSchema)]
pub struct SignExternalFundingTxResult {
    /// The signed funding transaction that can be submitted via `submit_signed_funding_tx`.
    pub signed_funding_tx: ckb_jsonrpc_types::Transaction,
}
