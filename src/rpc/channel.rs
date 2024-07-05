use std::cmp::Reverse;

use crate::ckb::{
    channel::{
        AddTlcCommand, ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, ChannelState,
        RemoveTlcCommand, ShutdownCommand,
    },
    hash_algorithm::HashAlgorithm,
    network::{AcceptChannelCommand, OpenChannelCommand},
    serde_utils::{U128Hex, U32Hex, U64Hex},
    types::{Hash256, LockTime, RemoveTlcFail, RemoveTlcFulfill},
    NetworkActorCommand, NetworkActorMessage,
};
use crate::error;
use crate::{handle_actor_call, handle_actor_cast, log_and_error};
use ckb_jsonrpc_types::Script;
use ckb_types::core::FeeRate;
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::{call, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tentacle::secio::PeerId;

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct OpenChannelParams {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,
    pub funding_udt_type_script: Option<Script>,
    #[serde_as(as = "Option<U64Hex>")]
    pub commitment_fee_rate: Option<u64>,
    #[serde_as(as = "Option<U64Hex>")]
    pub funding_fee_rate: Option<u64>,
}

#[derive(Clone, Serialize)]
pub struct OpenChannelResult {
    pub temporary_channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct AcceptChannelParams {
    pub temporary_channel_id: Hash256,
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,
}

#[derive(Clone, Serialize)]
pub struct AcceptChannelResult {
    pub channel_id: Hash256,
}

// TODO @quake remove this unnecessary struct and rpc after refactoring
#[derive(Serialize, Deserialize, Debug)]
pub struct CommitmentSignedParams {
    pub channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ListChannelsParams {
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub peer_id: Option<PeerId>,
}

#[derive(Clone, Serialize)]
pub struct ListChannelsResult {
    pub channels: Vec<Channel>,
}

#[serde_as]
#[derive(Clone, Serialize)]
pub struct Channel {
    pub channel_id: Hash256,
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub state: ChannelState,
    #[serde_as(as = "U128Hex")]
    pub local_balance: u128,
    #[serde_as(as = "U128Hex")]
    pub sent_tlc_balance: u128,
    #[serde_as(as = "U128Hex")]
    pub remote_balance: u128,
    #[serde_as(as = "U128Hex")]
    pub received_tlc_balance: u128,
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct AddTlcParams {
    pub channel_id: Hash256,
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: LockTime,
    pub hash_algorithm: Option<HashAlgorithm>,
}

#[serde_as]
#[derive(Clone, Serialize)]
pub struct AddTlcResult {
    #[serde_as(as = "U64Hex")]
    pub tlc_id: u64,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct RemoveTlcParams {
    pub channel_id: Hash256,
    #[serde_as(as = "U64Hex")]
    pub tlc_id: u64,
    pub reason: RemoveTlcReason,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum RemoveTlcReason {
    RemoveTlcFulfill {
        payment_preimage: Hash256,
    },
    RemoveTlcFail {
        #[serde_as(as = "U32Hex")]
        error_code: u32,
    },
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownChannelParams {
    pub channel_id: Hash256,
    pub close_script: Script,
    #[serde_as(as = "U64Hex")]
    pub fee_rate: u64,
}

#[rpc(server)]
pub trait ChannelRpc {
    #[method(name = "open_channel")]
    async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned>;

    #[method(name = "accept_channel")]
    async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned>;

    #[method(name = "list_channels")]
    async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned>;

    #[method(name = "commitment_signed")]
    async fn commitment_signed(
        &self,
        params: CommitmentSignedParams,
    ) -> Result<(), ErrorObjectOwned>;

    #[method(name = "add_tlc")]
    async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned>;

    #[method(name = "remove_tlc")]
    async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned>;

    #[method(name = "shutdown_channel")]
    async fn shutdown_channel(&self, params: ShutdownChannelParams)
        -> Result<(), ErrorObjectOwned>;
}

pub struct ChannelRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    store: S,
}

impl<S> ChannelRpcServerImpl<S> {
    pub fn new(actor: ActorRef<NetworkActorMessage>, store: S) -> Self {
        ChannelRpcServerImpl { actor, store }
    }
}

#[async_trait]
impl<S> ChannelRpcServer for ChannelRpcServerImpl<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
    async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned> {
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: params.peer_id.clone(),
                    funding_amount: params.funding_amount,
                    funding_udt_type_script: params
                        .funding_udt_type_script
                        .clone()
                        .map(|s| s.into()),
                    commitment_fee_rate: params.commitment_fee_rate,
                    funding_fee_rate: params.funding_fee_rate,
                },
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params).map(|response| OpenChannelResult {
            temporary_channel_id: response.channel_id,
        })
    }

    async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned> {
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id: params.temporary_channel_id,
                    funding_amount: params.funding_amount,
                },
                rpc_reply,
            ))
        };

        handle_actor_call!(self.actor, message, params).map(|response| AcceptChannelResult {
            channel_id: response.new_channel_id,
        })
    }

    async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned> {
        let mut channels: Vec<_> = self
            .store
            .get_channel_states(params.peer_id)
            .into_iter()
            .filter_map(|(peer_id, channel_id, _state)| {
                self.store
                    .get_channel_actor_state(&channel_id)
                    .map(|state| Channel {
                        channel_id,
                        peer_id,
                        state: state.state,
                        local_balance: state.get_local_balance(),
                        remote_balance: state.get_remote_balance(),
                        sent_tlc_balance: state.get_sent_tlc_balance(),
                        received_tlc_balance: state.get_received_tlc_balance(),
                        created_at: state.get_created_at_in_microseconds(),
                    })
            })
            .collect();
        // Sort by created_at in descending order
        channels.sort_by_key(|channel| Reverse(channel.created_at));
        Ok(ListChannelsResult { channels })
    }

    async fn commitment_signed(
        &self,
        params: CommitmentSignedParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = NetworkActorMessage::Command(NetworkActorCommand::ControlCfnChannel(
            ChannelCommandWithId {
                channel_id: params.channel_id,
                command: ChannelCommand::CommitmentSigned(),
            },
        ));
        handle_actor_cast!(self.actor, message, params)
    }

    async fn add_tlc(&self, params: AddTlcParams) -> Result<AddTlcResult, ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlCfnChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: params.amount,
                            preimage: None,
                            payment_hash: Some(params.payment_hash),
                            expiry: params.expiry,
                            hash_algorithm: params.hash_algorithm.unwrap_or_default(),
                        },
                        rpc_reply,
                    ),
                },
            ))
        };
        handle_actor_call!(self.actor, message, params).map(|response| AddTlcResult {
            tlc_id: response.tlc_id,
        })
    }

    async fn remove_tlc(&self, params: RemoveTlcParams) -> Result<(), ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlCfnChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::RemoveTlc(
                        RemoveTlcCommand {
                            id: params.tlc_id,
                            reason: match params.reason {
                                RemoveTlcReason::RemoveTlcFulfill { payment_preimage } => {
                                    crate::ckb::types::RemoveTlcReason::RemoveTlcFulfill(
                                        RemoveTlcFulfill { payment_preimage },
                                    )
                                }
                                RemoveTlcReason::RemoveTlcFail { error_code } => {
                                    crate::ckb::types::RemoveTlcReason::RemoveTlcFail(
                                        RemoveTlcFail { error_code },
                                    )
                                }
                            },
                        },
                        rpc_reply,
                    ),
                },
            ))
        };

        handle_actor_call!(self.actor, message, params)
    }

    async fn shutdown_channel(
        &self,
        params: ShutdownChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlCfnChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::Shutdown(
                        ShutdownCommand {
                            close_script: params.close_script.clone().into(),
                            fee_rate: FeeRate::from_u64(params.fee_rate),
                        },
                        rpc_reply,
                    ),
                },
            ))
        };
        handle_actor_call!(self.actor, message, params)
    }
}
