use crate::{
    fiber::{
        channel::{AddTlcCommand, ChannelCommand, ChannelCommandWithId, RemoveTlcCommand},
        hash_algorithm::HashAlgorithm,
        serde_utils::{U128Hex, U64Hex},
        types::{Hash256, RemoveTlcFulfill, TlcErr, TlcErrPacket, TlcErrorCode, NO_SHARED_SECRET},
        NetworkActorCommand, NetworkActorMessage,
    },
    handle_actor_cast,
    watchtower::WatchtowerStore,
};
use ckb_types::core::TransactionView;
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::call;
use std::str::FromStr;
use std::{collections::HashMap, sync::Arc};

use ractor::{call_t, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::sync::RwLock;

use crate::{
    ckb::CkbChainMessage, fiber::network::DEFAULT_CHAIN_ACTOR_TIMEOUT, handle_actor_call,
    log_and_error,
};

// TODO @quake remove this unnecessary pub(crate) struct and rpc after refactoring
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
    /// The reason for removing the TLC, either a 32-byte hash for preimage fulfillment or an u32 error code for removal
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
pub struct RemoveWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
}
