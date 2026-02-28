use crate::ckb::contracts::get_cell_deps_by_script;
use crate::fiber::channel::{TLCId, TlcStatus};
use crate::fiber::serde_utils::EntityHex;
use crate::fiber::{
    channel::{
        AwaitingChannelReadyFlags, AwaitingTxSignaturesFlags, ChannelActorStateStore,
        ChannelCommand, ChannelCommandWithId, ChannelState as RawChannelState, CloseFlags,
        CollaboratingFundingTxFlags, NegotiatingFundingFlags, ShutdownCommand, ShuttingDownFlags,
        SigningCommitmentFlags, UpdateCommand,
    },
    network::{AcceptChannelCommand, OpenChannelCommand, OpenChannelWithExternalFundingCommand},
    serde_utils::{U128Hex, U64Hex},
    types::Hash256,
    NetworkActorCommand, NetworkActorMessage,
};
use crate::{handle_actor_call, log_and_error};
use ckb_jsonrpc_types::{CellDep as JsonCellDep, EpochNumberWithFraction, Script};
use ckb_types::{
    core::{EpochNumberWithFraction as EpochNumberWithFractionCore, FeeRate},
    packed::{self, OutPoint},
    prelude::{IntoTransactionView, Unpack},
    H256,
};
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;

use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
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

    /// Whether this is a one-way channel (will not be broadcasted to network, and can only be used to send payment one way), an optional parameter, default value is false.
    pub one_way: Option<bool>,

    /// The type script of the UDT to fund the channel with, an optional parameter.
    pub funding_udt_type_script: Option<Script>,

    /// The script used to receive the channel balance, an optional parameter, default value is the secp256k1_blake160_sighash_all script corresponding to the configured private key.
    pub shutdown_script: Option<Script>,

    /// The delay time for the commitment transaction, must be an [EpochNumberWithFraction](https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0017-tx-valid-since/e-i-l-encoding.png) in u64 format, an optional parameter, default value is 1 epoch, which is 4 hours.
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,

    /// The fee rate for the commitment transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub commitment_fee_rate: Option<u64>,

    /// The fee rate for the funding transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub funding_fee_rate: Option<u64>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 4 hours, which is 4 * 60 * 60 * 1000 milliseconds
    /// Expect it >= 2/3 commitment_delay_epoch.
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
    #[serde_as(as = "Option<U64Hex>")]
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

#[derive(Clone, Serialize, Deserialize, Debug)]
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
    /// We're waiting for the user to sign and submit the funding transaction externally.
    AwaitingExternalFunding,
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
            RawChannelState::AwaitingExternalFunding => ChannelState::AwaitingExternalFunding,
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
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Channel {
    /// The channel ID
    pub channel_id: Hash256,
    /// Whether the channel is public
    pub is_public: bool,
    /// Is this channel initially inbound?
    /// An inbound channel is one where the counterparty is the funder of the channel.
    pub is_acceptor: bool,
    /// Is this channel one-way?
    /// Combines with is_acceptor to determine if the channel able to send payment to the counterparty or not.
    pub is_one_way: bool,
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
    /// The list of pending tlcs
    pub pending_tlcs: Vec<Htlc>,
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
    /// The hash of the shutdown transaction
    pub shutdown_transaction_hash: Option<H256>,
}

/// The htlc data structure
#[serde_as]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Htlc {
    /// The id of the htlc
    #[serde_as(as = "U64Hex")]
    pub id: u64,
    /// The amount of the htlc
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    /// The payment hash of the htlc
    pub payment_hash: Hash256,
    /// The expiry of the htlc
    #[serde_as(as = "U64Hex")]
    pub expiry: u64,
    /// If this HTLC is involved in a forwarding operation, this field indicates the forwarding channel.
    /// For an outbound htlc, it is the inbound channel. For an inbound htlc, it is the outbound channel.
    pub forwarding_channel_id: Option<Hash256>,
    /// If this HTLC is involved in a forwarding operation, this field indicates the forwarding tlc id.
    #[serde_as(as = "Option<U64Hex>")]
    pub forwarding_tlc_id: Option<u64>,
    /// The status of the htlc
    pub status: TlcStatus,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShutdownChannelParams {
    /// The channel ID of the channel to shut down
    pub channel_id: Hash256,
    /// The script used to receive the channel balance, only support secp256k1_blake160_sighash_all script for now
    /// default is `default_funding_lock_script` in `CkbConfig`
    pub close_script: Option<Script>,
    /// The fee rate for the closing transaction, the fee will be deducted from the closing initiator's channel balance
    /// default is 1000 shannons/KW
    #[serde_as(as = "Option<U64Hex>")]
    pub fee_rate: Option<u64>,
    /// Whether to force the channel to close, when set to false, `close_script` and `fee_rate` should be set, default is false.
    /// When set to true, `close_script` and `fee_rate` will be ignored and will use the default value when opening the channel.
    pub force: Option<bool>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateChannelParams {
    /// The channel ID of the channel to update
    pub channel_id: Hash256,
    /// Whether the channel is enabled
    pub enabled: Option<bool>,
    /// The expiry delta for the TLC locktime
    #[serde_as(as = "Option<U64Hex>")]
    pub tlc_expiry_delta: Option<u64>,
    /// The minimum value for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_minimum_value: Option<u128>,
    /// The fee proportional millionths for a TLC
    #[serde_as(as = "Option<U128Hex>")]
    pub tlc_fee_proportional_millionths: Option<u128>,
}

/// Parameters for opening a channel with external funding.
/// This allows end-users to open channels by signing the funding transaction
/// with their own wallet instead of having the node sign automatically.
#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub struct OpenChannelWithExternalFundingParams {
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

    /// The script used to receive the channel balance when the channel is closed. This is REQUIRED for external funding.
    pub shutdown_script: Script,

    /// The lock script that controls the funding cells. The node will collect cells with this lock script
    /// to build the funding transaction. The user must be able to sign for this lock script.
    pub funding_lock_script: Script,

    /// Additional cell deps required by the funding source lock script.
    /// These deps are appended to the unsigned funding transaction before it is returned for signing.
    pub funding_source_extra_cell_deps: Option<Vec<JsonCellDep>>,

    /// The delay time for the commitment transaction, must be an [EpochNumberWithFraction](https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0017-tx-valid-since/e-i-l-encoding.png) in u64 format, an optional parameter, default value is 1 epoch, which is 4 hours.
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,

    /// The fee rate for the commitment transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub commitment_fee_rate: Option<u64>,

    /// The fee rate for the funding transaction, an optional parameter.
    #[serde_as(as = "Option<U64Hex>")]
    pub funding_fee_rate: Option<u64>,

    /// The expiry delta to forward a tlc, in milliseconds, default to 4 hours, which is 4 * 60 * 60 * 1000 milliseconds
    /// Expect it >= 2/3 commitment_delay_epoch.
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

/// Result of opening a channel with external funding.
#[derive(Clone, Serialize, Deserialize)]
pub struct OpenChannelWithExternalFundingResult {
    /// The channel ID of the channel being opened.
    /// Use this ID to submit the signed funding transaction.
    pub channel_id: Hash256,

    /// The final unsigned funding transaction that needs to be signed.
    /// The user should sign this transaction with their wallet and submit it
    /// using `submit_signed_funding_tx` directly, without changing structure.
    pub unsigned_funding_tx: ckb_jsonrpc_types::Transaction,
}

/// Parameters for submitting a signed funding transaction for external funding.
#[derive(Serialize, Deserialize, Debug)]
pub struct SubmitSignedFundingTxParams {
    /// The channel ID returned from `open_channel_with_external_funding`.
    pub channel_id: Hash256,

    /// The signed funding transaction. This must be the same final transaction structure
    /// that was returned from `open_channel_with_external_funding`, with valid
    /// witnesses (signatures) added, and should be ready for direct broadcast.
    pub signed_funding_tx: ckb_jsonrpc_types::Transaction,
}

/// Result of submitting a signed funding transaction.
#[derive(Clone, Serialize, Deserialize)]
pub struct SubmitSignedFundingTxResult {
    /// The channel ID.
    pub channel_id: Hash256,

    /// The hash of the funding transaction that was submitted.
    pub funding_tx_hash: H256,
}

/// RPC module for channel management.
#[cfg(not(target_arch = "wasm32"))]
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

    /// Opens a channel with external funding. The node will negotiate the channel with the peer,
    /// but the user must sign the funding transaction themselves using their own wallet.
    ///
    /// This is useful when the user wants to fund a channel from an external wallet
    /// rather than having the node sign with its internal key.
    ///
    /// Returns a final unsigned funding transaction that the user must sign and submit
    /// using `submit_signed_funding_tx` without changing transaction structure.
    #[method(name = "open_channel_with_external_funding")]
    async fn open_channel_with_external_funding(
        &self,
        params: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, ErrorObjectOwned>;

    /// Submits a signed funding transaction for an externally funded channel.
    ///
    /// After calling `open_channel_with_external_funding`, the user signs the returned
    /// final unsigned transaction with their wallet and submits it here. The signed
    /// transaction should be directly broadcastable and will not be structurally modified.
    #[method(name = "submit_signed_funding_tx")]
    async fn submit_signed_funding_tx(
        &self,
        params: SubmitSignedFundingTxParams,
    ) -> Result<SubmitSignedFundingTxResult, ErrorObjectOwned>;
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
#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> ChannelRpcServer for ChannelRpcServerImpl<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
    /// Attempts to open a channel with a peer.
    async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned> {
        self.open_channel(params).await
    }

    /// Accepts a channel opening request from a peer.
    async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned> {
        self.accept_channel(params).await
    }

    /// Abandon a channel, this will remove the channel from the channel manager and DB.
    /// Only channels not in Ready or Closed state can be abandoned.
    async fn abandon_channel(&self, params: AbandonChannelParams) -> Result<(), ErrorObjectOwned> {
        self.abandon_channel(params).await
    }

    /// Lists all channels.
    async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned> {
        self.list_channels(params).await
    }

    /// Shuts down a channel.
    async fn shutdown_channel(
        &self,
        params: ShutdownChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.shutdown_channel(params).await
    }

    /// Updates a channel.
    async fn update_channel(&self, params: UpdateChannelParams) -> Result<(), ErrorObjectOwned> {
        self.update_channel(params).await
    }

    /// Opens a channel with external funding.
    async fn open_channel_with_external_funding(
        &self,
        params: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, ErrorObjectOwned> {
        self.open_channel_with_external_funding(params).await
    }

    /// Submits a signed funding transaction for an externally funded channel.
    async fn submit_signed_funding_tx(
        &self,
        params: SubmitSignedFundingTxParams,
    ) -> Result<SubmitSignedFundingTxResult, ErrorObjectOwned> {
        self.submit_signed_funding_tx(params).await
    }
}
impl<S> ChannelRpcServerImpl<S>
where
    S: ChannelActorStateStore + Send + Sync + 'static,
{
    pub async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned> {
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    peer_id: params.peer_id.clone(),
                    funding_amount: params.funding_amount,
                    public: params.public.unwrap_or(true),
                    one_way: params.one_way.unwrap_or(false),
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

    pub async fn accept_channel(
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

    pub async fn abandon_channel(
        &self,
        params: AbandonChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AbandonChannel(
                params.channel_id,
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor, message, params)
    }

    pub async fn list_channels(
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
                        is_acceptor: state.is_acceptor,
                        is_one_way: state.is_one_way,
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
                        pending_tlcs: state
                            .tlc_state
                            .all_tlcs()
                            .map(|tlc| {
                                let id = match tlc.tlc_id {
                                    TLCId::Offered(id) => id,
                                    TLCId::Received(id) => id,
                                };
                                Htlc {
                                    id,
                                    amount: tlc.amount,
                                    expiry: tlc.expiry,
                                    payment_hash: tlc.payment_hash,
                                    forwarding_channel_id: tlc
                                        .forwarding_tlc
                                        .map(|(channel_id, _)| channel_id),
                                    forwarding_tlc_id: tlc.forwarding_tlc.map(|(_, id)| id),
                                    status: tlc.status.clone(),
                                }
                            })
                            .collect(),
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
                        shutdown_transaction_hash: state.shutdown_transaction_hash,
                    })
            })
            .collect();
        // Sort by created_at in descending order
        channels.sort_by_key(|channel| Reverse(channel.created_at));
        Ok(ListChannelsResult { channels })
    }

    pub async fn shutdown_channel(
        &self,
        params: ShutdownChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        if params.force.unwrap_or_default()
            && (params.close_script.is_some() || params.fee_rate.is_some())
        {
            return Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "close_script and fee_rate should not be set when force is true",
                Some(params),
            ));
        }

        let close_script = params.close_script.clone().map(|s| s.into());
        let fee_rate = params.fee_rate.map(FeeRate::from_u64);

        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id: params.channel_id,
                    command: ChannelCommand::Shutdown(
                        ShutdownCommand {
                            close_script,
                            fee_rate,
                            force: params.force.unwrap_or_default(),
                        },
                        rpc_reply,
                    ),
                },
            ))
        };
        handle_actor_call!(self.actor, message, params)
    }

    pub async fn update_channel(
        &self,
        params: UpdateChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
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

    /// Opens a channel with external funding.
    pub async fn open_channel_with_external_funding(
        &self,
        params: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, ErrorObjectOwned> {
        let funding_lock_script: packed::Script = params.funding_lock_script.clone().into();
        let mut funding_source_extra_cell_deps: Vec<packed::CellDep> = params
            .funding_source_extra_cell_deps
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect();

        let auto_cell_deps = get_cell_deps_by_script(&funding_lock_script)
            .await
            .map_err(|err| {
                ErrorObjectOwned::owned(CALL_EXECUTION_FAILED_CODE, err.to_string(), Some(&params))
            })?;
        if let Some(auto_cell_deps) = auto_cell_deps {
            for cell_dep in auto_cell_deps {
                if !funding_source_extra_cell_deps
                    .iter()
                    .any(|existing| existing == &cell_dep)
                {
                    funding_source_extra_cell_deps.push(cell_dep);
                }
            }
        }

        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannelWithExternalFunding(
                OpenChannelWithExternalFundingCommand {
                    peer_id: params.peer_id.clone(),
                    funding_amount: params.funding_amount,
                    public: params.public.unwrap_or(true),
                    shutdown_script: params.shutdown_script.clone().into(),
                    funding_lock_script: funding_lock_script.clone(),
                    funding_source_extra_cell_deps: funding_source_extra_cell_deps.clone(),
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
        handle_actor_call!(self.actor, message, params).map(|response| {
            OpenChannelWithExternalFundingResult {
                channel_id: response.channel_id,
                unsigned_funding_tx: response.unsigned_funding_tx.into(),
            }
        })
    }

    /// Submits a signed funding transaction for an externally funded channel.
    pub async fn submit_signed_funding_tx(
        &self,
        params: SubmitSignedFundingTxParams,
    ) -> Result<SubmitSignedFundingTxResult, ErrorObjectOwned> {
        let channel_id = params.channel_id;
        let signed_tx: packed::Transaction = params.signed_funding_tx.clone().into();
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::SubmitSignedFundingTx {
                channel_id,
                signed_tx: signed_tx.clone(),
                reply: rpc_reply,
            })
        };
        handle_actor_call!(self.actor, message, params).map(|tx_hash| SubmitSignedFundingTxResult {
            channel_id,
            funding_tx_hash: tx_hash.into(),
        })
    }
}
