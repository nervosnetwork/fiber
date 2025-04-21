use crate::fiber::serde_utils::EntityHex;
use crate::fiber::{
    channel::{
        AwaitingChannelReadyFlags, AwaitingTxSignaturesFlags, ChannelActorStateStore,
        ChannelCommand, ChannelCommandWithId, ChannelState as RawChannelState, CloseFlags,
        CollaboratingFundingTxFlags, NegotiatingFundingFlags, ShutdownCommand, ShuttingDownFlags,
        SigningCommitmentFlags, UpdateCommand,
    },
    network::{AcceptChannelCommand, OpenChannelCommand},
    serde_utils::{U128Hex, U64Hex},
    types::Hash256,
    NetworkActorCommand, NetworkActorMessage,
};
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::{EpochNumberWithFraction, Script};
use ckb_types::{
    core::{EpochNumberWithFraction as EpochNumberWithFractionCore, FeeRate},
    packed::OutPoint,
    prelude::{IntoTransactionView, Unpack},
    H256,
};
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use ractor::{call, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::cmp::Reverse;
use tentacle::secio::PeerId;

/// RPC module for channel management.
#[rpc(server)]
trait ChannelRpc {
    /// Attempts to open a channel with a peer.
    #[method(name = "open_channel")]
    async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned>;

    /// Accepts a channel opening request from a peer.
    #[method(name = "accept_channel")]
    async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned>;

    /// Abandon a channel, this will remove the channel from the channel manager and DB.
    /// Only channels not in Ready or Closed state can be abandoned.
    #[method(name = "abandon_channel")]
    async fn abandon_channel(&self, params: AbandonChannelParams) -> Result<(), ErrorObjectOwned>;

    /// Lists all channels.
    #[method(name = "list_channels")]
    async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned>;

    /// Shuts down a channel.
    #[method(name = "shutdown_channel")]
    async fn shutdown_channel(&self, params: ShutdownChannelParams)
        -> Result<(), ErrorObjectOwned>;

    /// Updates a channel.
    #[method(name = "update_channel")]
    async fn update_channel(&self, params: UpdateChannelParams) -> Result<(), ErrorObjectOwned>;
}

pub(crate) struct ChannelRpcServerImpl<S> {
    actor: ActorRef<NetworkActorMessage>,
    store: S,
}

impl<S> ChannelRpcServerImpl<S> {
    pub(crate) fn new(actor: ActorRef<NetworkActorMessage>, store: S) -> Self {
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
                    public: params.public.unwrap_or(true),
                    shutdown_script: params.shutdown_script.clone().map(|s| s.into()),
                    commitment_delay_epoch: params
                        .commitment_delay_epoch
                        .map(|e| EpochNumberWithFractionCore::from_full_value(e.value())),
                    funding_udt_type_script: params
                        .funding_udt_type_script
                        .clone()
                        .map(|s| s.into()),
                    commitment_fee_rate: params.commitment_fee_rate,
                    funding_fee_rate: params.funding_fee_rate,
                    tlc_expiry_delta: params.tlc_expiry_delta,
                    tlc_min_value: params.tlc_min_value,
                    tlc_fee_proportional_millionths: params.tlc_fee_proportional_millionths,
                    max_tlc_value_in_flight: params.max_tlc_value_in_flight,
                    max_tlc_number_in_flight: params.max_tlc_number_in_flight,
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
                    shutdown_script: params.shutdown_script.clone().map(|s| s.into()),
                    max_tlc_number_in_flight: params.max_tlc_number_in_flight,
                    max_tlc_value_in_flight: params.max_tlc_value_in_flight,
                    min_tlc_value: params.tlc_min_value,
                    tlc_fee_proportional_millionths: params.tlc_fee_proportional_millionths,
                    tlc_expiry_delta: params.tlc_expiry_delta,
                },
                rpc_reply,
            ))
        };

        handle_actor_call!(self.actor, message, params).map(|response| AcceptChannelResult {
            channel_id: response.new_channel_id,
        })
    }

    async fn abandon_channel(&self, params: AbandonChannelParams) -> Result<(), ErrorObjectOwned> {
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AbandonChannel(
                params.channel_id,
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params)
    }

    async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned> {
        let channel_states = if params.include_closed.unwrap_or_default() {
            self.store.get_channel_states(params.peer_id)
        } else {
            self.store.get_active_channel_states(params.peer_id)
        };
        let mut channels: Vec<_> = channel_states
            .into_iter()
            .filter_map(|(peer_id, channel_id, _state)| {
                self.store
                    .get_channel_actor_state(&channel_id)
                    .map(|state| Channel {
                        channel_id,
                        is_public: state.is_public(),
                        channel_outpoint: state.get_funding_transaction_outpoint(),
                        peer_id,
                        funding_udt_type_script: state
                            .funding_udt_type_script
                            .clone()
                            .map(Into::into),
                        state: state.state.into(),
                        local_balance: state.get_local_balance(),
                        remote_balance: state.get_remote_balance(),
                        offered_tlc_balance: state.get_offered_tlc_balance(),
                        received_tlc_balance: state.get_received_tlc_balance(),
                        latest_commitment_transaction_hash: state
                            .latest_commitment_transaction
                            .as_ref()
                            .map(|tx| tx.clone().into_view().hash().unpack()),
                        created_at: state.get_created_at_in_millis(),
                        enabled: state.local_tlc_info.enabled,
                        tlc_expiry_delta: state.local_tlc_info.tlc_expiry_delta,
                        tlc_fee_proportional_millionths: state
                            .local_tlc_info
                            .tlc_fee_proportional_millionths,
                    })
            })
            .collect();
        // Sort by created_at in descending order
        channels.sort_by_key(|channel| Reverse(channel.created_at));
        Ok(ListChannelsResult { channels })
    }

    async fn shutdown_channel(
        &self,
        params: ShutdownChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        if params.force.unwrap_or_default() {
            if params.close_script.is_some() || params.fee_rate.is_some() {
                return Err(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    "close_script and fee_rate should not be set when force is true",
                    Some(params),
                ));
            }
        } else if params.close_script.is_none() || params.fee_rate.is_none() {
            return Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "close_script and fee_rate should be set when force is false",
                Some(params),
            ));
        }

        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::Shutdown(
                        ShutdownCommand {
                            close_script: params
                                .close_script
                                .clone()
                                .map(Into::into)
                                .unwrap_or_default(),
                            fee_rate: params.fee_rate.map(FeeRate::from_u64).unwrap_or_default(),
                            force: params.force.unwrap_or_default(),
                        },
                        rpc_reply,
                    ),
                },
            ))
        };
        handle_actor_call!(self.actor, message, params)
    }

    async fn update_channel(&self, params: UpdateChannelParams) -> Result<(), ErrorObjectOwned> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::Update(
                        UpdateCommand {
                            enabled: params.enabled,
                            tlc_expiry_delta: params.tlc_expiry_delta,
                            tlc_minimum_value: params.tlc_minimum_value,
                            tlc_fee_proportional_millionths: params.tlc_fee_proportional_millionths,
                        },
                        rpc_reply,
                    ),
                },
            ))
        };
        handle_actor_call!(self.actor, message, params)
    }
}
