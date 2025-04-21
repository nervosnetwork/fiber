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

/// RPC module for development purposes, this module is not intended to be used in production.
/// This module will be disabled in release build.
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

    /// Remove a watched channel from the watchtower store
    #[method(name = "remove_watch_channel")]
    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;
}

pub(crate) struct DevRpcServerImpl<S> {
    ckb_chain_actor: ActorRef<CkbChainMessage>,
    network_actor: ActorRef<NetworkActorMessage>,
    commitment_txs: Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
    store: S,
}

impl<S> DevRpcServerImpl<S> {
    pub(crate) fn new(
        ckb_chain_actor: ActorRef<CkbChainMessage>,
        network_actor: ActorRef<NetworkActorMessage>,
        commitment_txs: Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
        store: S,
    ) -> Self {
        Self {
            ckb_chain_actor,
            network_actor,
            commitment_txs,
            store,
        }
    }
}

#[async_trait]
impl<S> DevRpcServer for DevRpcServerImpl<S>
where
    S: WatchtowerStore + Send + Sync + 'static,
{
    async fn commitment_signed(
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

    async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: params.amount,
                            payment_hash: params.payment_hash,
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

    async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned> {
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

    async fn submit_commitment_transaction(
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

    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.store.remove_watch_channel(params.channel_id);
        Ok(())
    }
}
