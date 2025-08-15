// #[cfg(not(target_arch = "wasm32"))]
// use crate::watchtower::WatchtowerStore;
use crate::{
    fiber::{
        channel::{AddTlcCommand, ChannelCommand, ChannelCommandWithId, RemoveTlcCommand},
        hash_algorithm::HashAlgorithm,
        serde_utils::{U128Hex, U64Hex},
        types::{Hash256, RemoveTlcFulfill, TlcErr, TlcErrPacket, TlcErrorCode, NO_SHARED_SECRET},
        NetworkActorCommand, NetworkActorMessage,
    },
    handle_actor_cast,
};
use ckb_types::core::TransactionView;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};

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
pub struct CheckChannelShutdownParams {
    /// Channel ID
    pub channel_id: Hash256,
}

/// RPC module for development purposes, this module is not intended to be used in production.
/// This module will be disabled in release build.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait DevRpc {
    /// Sends a commitment_signed message to the peer.
    #[method(name = "commitment_signed")]
    async fn commitment_signed(
        &self,
        params: CommitmentSignedParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Adds a TLC to a channel.
    #[method(name = "add_tlc")]
    async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned>;

    /// Removes a TLC from a channel.
    #[method(name = "remove_tlc")]
    async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned>;

    /// Submit a commitment transaction to the chain
    #[method(name = "submit_commitment_transaction")]
    async fn submit_commitment_transaction(
        &self,
        params: SubmitCommitmentTransactionParams,
    ) -> Result<SubmitCommitmentTransactionResult, ErrorObjectOwned>;

    /// Manually trigger CheckShutdownTx on all channels
    #[method(name = "check_channel_shutdown")]
    async fn check_channel_shutdown(
        &self,
        params: CheckChannelShutdownParams,
    ) -> Result<(), ErrorObjectOwned>;
}

pub struct DevRpcServerImpl {
    ckb_chain_actor: ActorRef<CkbChainMessage>,
    network_actor: ActorRef<NetworkActorMessage>,
    commitment_txs: Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
}

impl DevRpcServerImpl {
    pub fn new(
        ckb_chain_actor: ActorRef<CkbChainMessage>,
        network_actor: ActorRef<NetworkActorMessage>,
        commitment_txs: Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
    ) -> Self {
        Self {
            ckb_chain_actor,
            network_actor,
            commitment_txs,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl DevRpcServer for DevRpcServerImpl {
    /// Sends a commitment_signed message to the peer.
    async fn commitment_signed(
        &self,
        params: CommitmentSignedParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.commitment_signed(params).await
    }

    /// Adds a TLC to a channel.
    async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned> {
        self.add_tlc(params).await
    }

    /// Removes a TLC from a channel.
    async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned> {
        self.remove_tlc(params).await
    }

    /// Submit a commitment transaction to the chain
    async fn submit_commitment_transaction(
        &self,
        params: SubmitCommitmentTransactionParams,
    ) -> Result<SubmitCommitmentTransactionResult, ErrorObjectOwned> {
        self.submit_commitment_transaction(params).await
    }

    async fn check_channel_shutdown(
        &self,
        params: CheckChannelShutdownParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.check_channel_shutdown(params).await
    }
}
impl DevRpcServerImpl {
    pub async fn commitment_signed(
        &self,
        params: CommitmentSignedParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: params.channel_id,
                command: ChannelCommand::CommitmentSigned(),
            },
        ));
        handle_actor_cast!(self.network_actor, message, params)
    }

    pub async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: params.amount,
                            payment_hash: params.payment_hash,
                            attempt_id: None,
                            expiry: params.expiry,
                            hash_algorithm: params.hash_algorithm.unwrap_or_default(),
                            onion_packet: None,
                            shared_secret: NO_SHARED_SECRET,
                            previous_tlc: None,
                        },
                        rpc_reply,
                    ),
                },
            ))
        };
        handle_actor_call!(self.network_actor, message, params).map(|response| AddTlcResult {
            tlc_id: response.tlc_id,
        })
    }

    pub async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned> {
        let err_code = match &params.reason {
            RemoveTlcReason::RemoveTlcFail { error_code } => {
                let Ok(err) = TlcErrorCode::from_str(error_code) else {
                    return log_and_error!(params, format!("invalid error code: {}", error_code));
                };
                Some(err)
            }
            _ => None,
        };
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::RemoveTlc(
                        RemoveTlcCommand {
                            id: params.tlc_id,
                            reason: match &params.reason {
                                RemoveTlcReason::RemoveTlcFulfill { payment_preimage } => {
                                    crate::fiber::types::RemoveTlcReason::RemoveTlcFulfill(
                                        RemoveTlcFulfill {
                                            payment_preimage: *payment_preimage,
                                        },
                                    )
                                }
                                RemoveTlcReason::RemoveTlcFail { .. } => {
                                    // TODO: maybe we should remove this PRC or move add_tlc and remove_tlc to `test` module?
                                    crate::fiber::types::RemoveTlcReason::RemoveTlcFail(
                                        TlcErrPacket::new(
                                            TlcErr::new(err_code.expect("expect error code")),
                                            // Do not encrypt the error message when removing the TLC via RPC.
                                            // TODO: use tlc id to look up the shared secret in the store
                                            &NO_SHARED_SECRET,
                                        ),
                                    )
                                }
                            },
                        },
                        rpc_reply,
                    ),
                },
            ))
        };

        handle_actor_call!(self.network_actor, message, params)
    }

    pub async fn submit_commitment_transaction(
        &self,
        params: SubmitCommitmentTransactionParams,
    ) -> Result<SubmitCommitmentTransactionResult, ErrorObjectOwned> {
        if let Some(tx) = self
            .commitment_txs
            .read()
            .await
            .get(&(params.channel_id, params.commitment_number))
        {
            if let Err(err) = call_t!(
                &self.ckb_chain_actor,
                CkbChainMessage::SendTx,
                DEFAULT_CHAIN_ACTOR_TIMEOUT,
                tx.clone()
            )
            .unwrap()
            {
                Err(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    err.to_string(),
                    Some(params),
                ))
            } else {
                Ok(SubmitCommitmentTransactionResult {
                    tx_hash: tx.hash().into(),
                })
            }
        } else {
            Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "Commitment transaction not found".to_string(),
                Some(params),
            ))
        }
    }

    pub async fn check_channel_shutdown(
        &self,
        params: CheckChannelShutdownParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::CheckChannelShutdown(
            params.channel_id,
        ));

        handle_actor_cast!(self.network_actor, message, params)
    }
}
