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

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct OpenChannelParams {
    /// The peer ID to open a channel with, the peer must be connected through the [connect_peer](#peer-connect_peer) rpc first.
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,

    /// The amount of CKB or UDT to fund the channel with.
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,

    /// Whether this is a public channel (will be broadcasted to network, and can be used to forward TLCs), an optional parameter, default value is true.
    pub public: Option<bool>,

    /// The type script of the UDT to fund the channel with, an optional parameter.
    pub funding_udt_type_script: Option<Script>,

    /// The script used to receive the channel balance, an optional parameter, default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key.
    pub shutdown_script: Option<Script>,

    /// The delay time for the commitment transaction, must be an [EpochNumberWithFraction](https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0017-tx-valid-since/e-i-l-encoding.png) in u64 format, an optional parameter, default value is 24 hours.
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,

    /// The fee rate for the commitment transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub commitment_fee_rate: Option<u64>,

    /// The fee rate for the funding transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub funding_fee_rate: Option<u64>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 1 day, which is 24 * 60 * 60 * 1000 milliseconds
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_delta: Option<u64>,

    /// The minimum value for a TLC our side can send,
    /// an optional parameter, default is 0, which means we can send any TLC is larger than 0.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_min_value: Option<u128>,

    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// The maximum value in flight for TLCs, an optional parameter.
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_tlc_value_in_flight: Option<u128>,

    /// The maximum number of TLCs that can be accepted, an optional parameter, default is 125
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U64Hex>")]
    pub max_tlc_number_in_flight: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct OpenChannelResult {
    /// The temporary channel ID of the channel being opened
    pub temporary_channel_id: Hash256,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AbandonChannelParams {
    /// The temporary channel ID or real channel ID of the channel being abandoned
    pub channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct AcceptChannelParams {
    /// The temporary channel ID of the channel to accept
    pub temporary_channel_id: Hash256,

    /// The amount of CKB or UDT to fund the channel with
    #[serde_as(as = "U128Hex")]
    pub funding_amount: u128,

    /// The script used to receive the channel balance, an optional parameter,
    /// default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key
    pub shutdown_script: Option<Script>,

    /// The max tlc sum value in flight for the channel, default is u128::MAX
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U128Hex>")]
    pub max_tlc_value_in_flight: Option<u128>,

    /// The max tlc number in flight send from our side, default is 125
    /// This parameter can not be updated after channel is opened.
    #[serde_as(as = "Option<U64Hex>")]
    pub max_tlc_number_in_flight: Option<u64>,

    /// The minimum value for a TLC our side can send,
    /// an optional parameter, default is 0, which means we can send any TLC is larger than 0.
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_min_value: Option<u128>,

    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 1 day, which is 24 * 60 * 60 * 1000 milliseconds
    /// This parameter can be updated with rpc `update_channel` later.
    pub tlc_expiry_delta: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AcceptChannelResult {
    /// The final ID of the channel that was accepted, it's different from the temporary channel ID
    pub channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ListChannelsParams {
    /// The peer ID to list channels for, an optional parameter, if not provided, all channels will be listed
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub peer_id: Option<PeerId>,
    /// Whether to include closed channels in the list, an optional parameter, default value is false
    pub include_closed: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ListChannelsResult {
    /// The list of channels
    pub channels: Vec<Channel>,
}

/// The state of a channel
// `ChannelState` is a copy of `ChannelState` with `#[serde(...)]` attributes for compatibility
// `bincode` does not support deserialize_identifier
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "SCREAMING_SNAKE_CASE",
    tag = "state_name",
    content = "state_flags"
)]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    NegotiatingFunding(NegotiatingFundingFlags),
    /// We're collaborating with the other party on the funding transaction.
    CollaboratingFundingTx(CollaboratingFundingTxFlags),
    /// We have collaborated over the funding and are now waiting for CommitmentSigned messages.
    SigningCommitment(SigningCommitmentFlags),
    /// We've received and sent `commitment_signed` and are now waiting for both
    /// party to collaborate on creating a valid funding transaction.
    AwaitingTxSignatures(AwaitingTxSignaturesFlags),
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady,
    /// We've successfully negotiated a `closing_signed` dance. At this point, the `ChannelManager`
    ShuttingDown(ShuttingDownFlags),
    /// This channel is closed.
    Closed(CloseFlags),
}

impl From<RawChannelState> for ChannelState {
    fn from(state: RawChannelState) -> Self {
        match state {
            RawChannelState::NegotiatingFunding(flags) => ChannelState::NegotiatingFunding(flags),
            RawChannelState::CollaboratingFundingTx(flags) => {
                ChannelState::CollaboratingFundingTx(flags)
            }
            RawChannelState::SigningCommitment(flags) => ChannelState::SigningCommitment(flags),
            RawChannelState::AwaitingTxSignatures(flags) => {
                ChannelState::AwaitingTxSignatures(flags)
            }
            RawChannelState::AwaitingChannelReady(flags) => {
                ChannelState::AwaitingChannelReady(flags)
            }
            RawChannelState::ChannelReady => ChannelState::ChannelReady,
            RawChannelState::ShuttingDown(flags) => ChannelState::ShuttingDown(flags),
            RawChannelState::Closed(flags) => ChannelState::Closed(flags),
        }
    }
}

/// The channel data structure
#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct Channel {
    /// The channel ID
    pub channel_id: Hash256,
    /// Whether the channel is public
    pub is_public: bool,
    #[serde_as(as = "Option<EntityHex>")]
    /// The outpoint of the channel
    pub channel_outpoint: Option<OutPoint>,
    /// The peer ID of the channel
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    /// The UDT type script of the channel
    pub funding_udt_type_script: Option<Script>,
    /// The state of the channel
    pub state: ChannelState,
    /// The local balance of the channel
    #[serde_as(as = "U128Hex")]
    pub local_balance: u128,
    /// The offered balance of the channel
    #[serde_as(as = "U128Hex")]
    pub offered_tlc_balance: u128,
    /// The remote balance of the channel
    #[serde_as(as = "U128Hex")]
    pub remote_balance: u128,
    /// The received balance of the channel
    #[serde_as(as = "U128Hex")]
    pub received_tlc_balance: u128,
    /// The hash of the latest commitment transaction
    pub latest_commitment_transaction_hash: Option<H256>,
    /// The time the channel was created at, in milliseconds from UNIX epoch
    #[serde_as(as = "U64Hex")]
    pub created_at: u64,
    /// Whether the channel is enabled
    pub enabled: bool,
    /// The expiry delta to forward a tlc, in milliseconds, default to 1 day, which is 24 * 60 * 60 * 1000 milliseconds
    /// This parameter can be updated with rpc `update_channel` later.
    #[serde_as(as = "U64Hex")]
    pub tlc_expiry_delta: u64,
    /// The fee proportional millionths for a TLC, proportional to the amount of the forwarded tlc.
    /// The unit is millionths of the amount. default is 1000 which means 0.1%.
    /// This parameter can be updated with rpc `update_channel` later.
    /// Not that, we use outbound channel to calculate the fee for TLC forwarding. For example,
    /// if we have a path A -> B -> C, then the fee B requires for TLC forwarding, is calculated
    /// the channel configuration of B and C, not A and B.
    #[serde_as(as = "U128Hex")]
    pub tlc_fee_proportional_millionths: u128,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownChannelParams {
    /// The channel ID of the channel to shut down
    pub channel_id: Hash256,
    /// The script used to receive the channel balance, only support secp256k1_blake160_sighash_all script for now
    pub close_script: Script,
    /// Whether to force the channel to close
    pub force: Option<bool>,
    /// The fee rate for the closing transaction, the fee will be deducted from the closing initiator's channel balance
    #[serde_as(as = "U64Hex")]
    pub fee_rate: u64,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateChannelParams {
    /// The channel ID of the channel to update
    pub channel_id: Hash256,
    /// Whether the channel is enabled
    pub enabled: Option<bool>,
    /// The CLTV delta from the current height that should be used to set the timelock for the final hop
    #[serde_as(as = "Option<U64Hex>")]
    /// The expiry delta for the TLC locktime
    pub tlc_expiry_delta: Option<u64>,
    /// The minimum value for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_minimum_value: Option<u128>,
    /// The fee proportional millionths for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,
}

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
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::Shutdown(
                        ShutdownCommand {
                            close_script: params.close_script.clone().into(),
                            fee_rate: FeeRate::from_u64(params.fee_rate),
                            force: params.force.unwrap_or(false),
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
