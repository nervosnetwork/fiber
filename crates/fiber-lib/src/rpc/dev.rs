// #[cfg(not(target_arch = "wasm32"))]
// use crate::watchtower::WatchtowerStore;
use crate::rpc::utils::rpc_error;
use crate::{
    fiber::{
        channel::{ChannelCommand, ChannelCommandWithId, RemoveTlcCommand},
        NetworkActorCommand, NetworkActorMessage,
    },
    handle_actor_cast,
};
use ckb_types::core::TransactionView;
use ckb_types::prelude::Entity;
use fiber_json_types::serde_utils::Hash256 as JsonHash256;
use fiber_types::{
    AddTlcCommand, Hash256, HashAlgorithm, RemoveTlcFulfill, TlcErr, TlcErrPacket, TlcErrorCode,
    NO_SHARED_SECRET,
};
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;

use ractor::call;
use std::str::FromStr;
use std::{collections::HashMap, sync::Arc};

use ractor::{call_t, ActorRef};
use tokio::sync::RwLock;

use crate::{
    ckb::CkbChainMessage, fiber::network::DEFAULT_CHAIN_ACTOR_TIMEOUT, handle_actor_call,
    log_and_error,
};

pub use fiber_json_types::{
    AddTlcParams, AddTlcResult, CheckChannelShutdownParams, CommitmentSignedParams,
    RemoveTlcParams, RemoveTlcReason, SubmitCommitmentTransactionParams,
    SubmitCommitmentTransactionResult,
};

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
        let channel_id = params.channel_id.into();
        let message = NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::CommitmentSigned(),
            },
        ));
        handle_actor_cast!(self.network_actor, message, params)
    }

    pub async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned> {
        let channel_id = params.channel_id.into();
        let payment_hash = params.payment_hash.into();
        let hash_algorithm = params
            .hash_algorithm
            .map(HashAlgorithm::from)
            .unwrap_or_default();

        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: params.amount,
                            payment_hash,
                            attempt_id: None,
                            expiry: params.expiry,
                            hash_algorithm,
                            onion_packet: None,
                            shared_secret: NO_SHARED_SECRET,
                            is_trampoline_hop: false,
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
        let channel_id = params.channel_id.into();
        let err_code = match &params.reason {
            RemoveTlcReason::RemoveTlcFail { error_code } => {
                let Ok(err) = TlcErrorCode::from_str(error_code) else {
                    return log_and_error!(params, format!("invalid error code: {}", error_code));
                };
                Some(err)
            }
            _ => None,
        };
        let reason = match &params.reason {
            RemoveTlcReason::RemoveTlcFulfill { payment_preimage } => {
                let preimage = (*payment_preimage).into();
                crate::fiber::types::RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                    payment_preimage: preimage,
                })
            }
            RemoveTlcReason::RemoveTlcFail { .. } => {
                // TODO: maybe we should remove this PRC or move add_tlc and remove_tlc to `test` module?
                crate::fiber::types::RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                    TlcErr::new(err_code.expect("expect error code")),
                    // Do not encrypt the error message when removing the TLC via RPC.
                    // TODO: use tlc id to look up the shared secret in the store
                    &NO_SHARED_SECRET,
                ))
            }
        };
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::RemoveTlc(
                        RemoveTlcCommand {
                            id: params.tlc_id,
                            reason,
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
        let channel_id = params.channel_id.into();
        if let Some(tx) = self
            .commitment_txs
            .read()
            .await
            .get(&(channel_id, params.commitment_number))
        {
            if let Err(err) = call_t!(
                &self.ckb_chain_actor,
                CkbChainMessage::SendTx,
                DEFAULT_CHAIN_ACTOR_TIMEOUT,
                tx.clone()
            )
            .unwrap()
            {
                Err(rpc_error(err.to_string(), params))
            } else {
                Ok(SubmitCommitmentTransactionResult {
                    tx_hash: JsonHash256(
                        tx.hash().as_slice().try_into().expect("Byte32 is 32 bytes"),
                    ),
                })
            }
        } else {
            Err(rpc_error(
                "Commitment transaction not found".to_string(),
                params,
            ))
        }
    }

    pub async fn check_channel_shutdown(
        &self,
        params: CheckChannelShutdownParams,
    ) -> Result<(), ErrorObjectOwned> {
        let channel_id = params.channel_id.into();
        let message =
            NetworkActorMessage::Command(NetworkActorCommand::CheckChannelShutdown(channel_id));

        handle_actor_cast!(self.network_actor, message, params)
    }
}
