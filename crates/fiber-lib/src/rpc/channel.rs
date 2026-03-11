use crate::fiber::{
    channel::{
        ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, ChannelOpenRecordStore,
        ShutdownCommand, UpdateCommand,
    },
    network::{AcceptChannelCommand, OpenChannelCommand, PendingAcceptChannel},
    NetworkActorCommand, NetworkActorMessage,
};
use crate::rpc::utils::{rpc_error, RpcResultExt};
use crate::{handle_actor_call, log_and_error};
use ckb_types::{
    core::{EpochNumberWithFraction as EpochNumberWithFractionCore, FeeRate},
    prelude::{IntoTransactionView, Unpack},
};
use fiber_types::Pubkey;
use fiber_types::{ChannelOpeningStatus, NegotiatingFundingFlags, TLCId};
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;

use jsonrpsee::types::ErrorObjectOwned;
use ractor::{call, ActorRef};
use std::cmp::Reverse;

pub use fiber_json_types::{
    AbandonChannelParams, AcceptChannelParams, AcceptChannelResult, Channel, ChannelState, Htlc,
    ListChannelsParams, ListChannelsResult, OpenChannelParams, OpenChannelResult,
    ShutdownChannelParams, TlcStatus as JsonTlcStatus, UpdateChannelParams,
};

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
}

/// Convert a `PendingAcceptChannel` (inbound, not yet accepted) into a minimal `Channel`
/// response suitable for inclusion in `list_channels(only_pending = true)`.
///
/// These channels are held in-memory by the network actor in `to_be_accepted_channels`.
/// They have no `ChannelActorState` yet since `create_inbound_channel` has not been called.
fn pending_accept_channel_to_rpc(pending: PendingAcceptChannel) -> Channel {
    Channel {
        channel_id: pending.channel_id.into(),
        // The accepting node is the non-initiator, so is_acceptor = true
        is_acceptor: true,
        is_public: false,
        is_one_way: false,
        channel_outpoint: None,
        pubkey: pending.pubkey.into(),
        funding_udt_type_script: pending.udt_type_script.map(Into::into),
        // Report as NegotiatingFunding since we're still awaiting local acceptance
        state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty().bits().into()),
        // The remote peer's funding amount is what they're contributing
        local_balance: 0,
        remote_balance: pending.funding_amount,
        offered_tlc_balance: 0,
        received_tlc_balance: 0,
        pending_tlcs: vec![],
        latest_commitment_transaction_hash: None,
        created_at: pending.created_at,
        enabled: false,
        tlc_expiry_delta: 0,
        tlc_fee_proportional_millionths: 0,
        shutdown_transaction_hash: None,
        failure_detail: None,
    }
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
    S: ChannelActorStateStore + ChannelOpenRecordStore + Send + Sync + 'static,
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
}
impl<S> ChannelRpcServerImpl<S>
where
    S: ChannelActorStateStore + ChannelOpenRecordStore + Send + Sync + 'static,
{
    pub async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned> {
        let pubkey = Pubkey::try_from(params.pubkey).rpc_err(&params)?;
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
                OpenChannelCommand {
                    pubkey,
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
            temporary_channel_id: response.channel_id.into(),
        })
    }

    pub async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned> {
        let temp_channel_id = params.temporary_channel_id.into();
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
                AcceptChannelCommand {
                    temp_channel_id,
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
            channel_id: response.new_channel_id.into(),
        })
    }

    pub async fn abandon_channel(
        &self,
        params: AbandonChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let channel_id = params.channel_id.into();
        let message = |rpc_reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AbandonChannel(channel_id, rpc_reply))
        };
        handle_actor_call!(self.actor, message, params)
    }

    pub async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned> {
        let only_pending = params.only_pending.unwrap_or_default();
        let include_closed = params.include_closed.unwrap_or_default();

        // Convert the optional String pubkey filter to internal Pubkey
        let filter_pubkey = params
            .pubkey
            .map(Pubkey::try_from)
            .transpose()
            .rpc_err(&params)?;

        // The two filter options are mutually exclusive: `only_pending` narrows to channels
        // that are still opening (or failed to open), while `include_closed` broadens to
        // all channels including successfully closed ones. Allowing both simultaneously
        // would produce confusing results.
        if only_pending && include_closed {
            return Err(rpc_error(
                "only_pending and include_closed are mutually exclusive",
                params,
            ));
        }

        let channel_states = if only_pending {
            // For pending mode, fetch all channel states (including non-active ones
            // like ABANDONED / FUNDING_ABORTED which are "closed" but represent failed openings)
            self.store.get_channel_states(filter_pubkey)
        } else if include_closed {
            self.store.get_channel_states(filter_pubkey)
        } else {
            self.store.get_active_channel_states(filter_pubkey)
        };
        let mut channels: Vec<_> = channel_states
            .into_iter()
            .filter_map(|(_pubkey, channel_id, _state)| {
                self.store
                    .get_channel_actor_state(&channel_id)
                    .and_then(|state| {
                        let rpc_state: ChannelState = state.state.into();
                        // When only_pending is set, skip channels that are not in a pending state
                        if only_pending && !rpc_state.is_pending() {
                            return None;
                        }
                        // Enrich with failure_detail from ChannelOpenRecord when available
                        let failure_detail = self
                            .store
                            .get_channel_open_record(&channel_id)
                            .and_then(|r| r.failure_detail);
                        Some(Channel {
                            channel_id: channel_id.into(),
                            is_public: state.is_public(),
                            is_acceptor: state.is_acceptor,
                            is_one_way: state.is_one_way,
                            channel_outpoint: state.get_funding_transaction_outpoint(),
                            pubkey: state.remote_pubkey.into(),
                            funding_udt_type_script: state
                                .funding_udt_type_script
                                .clone()
                                .map(Into::into),
                            state: rpc_state,
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
                                        payment_hash: tlc.payment_hash.into(),
                                        forwarding_channel_id: tlc
                                            .forwarding_tlc
                                            .map(|(channel_id, _)| channel_id.into()),
                                        forwarding_tlc_id: tlc.forwarding_tlc.map(|(_, id)| id),
                                        status: tlc.status.clone().into(),
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
                            shutdown_transaction_hash: state.shutdown_transaction_hash.clone(),
                            failure_detail,
                        })
                    })
            })
            .collect();

        if only_pending {
            // Also include channel-opening records (outbound) whose ChannelActorState is not yet
            // in the store or has been deleted. This covers two cases:
            //
            // 1. **WaitingForPeer** (and other in-progress statuses): the outbound channel actor
            //    only persists its ChannelActorState when its `handle()` method is first called
            //    (i.e., after the first message from the peer). Before that, the channel exists
            //    only in the ChannelOpenRecord. Without this path the initiator would see nothing
            //    when calling list_channels(only_pending=true) on an unaccepted channel.
            //
            // 2. **Failed**: the ChannelActorState was already deleted after Abandon/AbortFunding.
            for record in self.store.get_channel_open_records() {
                // ChannelReady is the "done" state — those channels appear in the normal list.
                if record.status == ChannelOpeningStatus::ChannelReady {
                    continue;
                }
                // Inbound channels still waiting for acceptance (WaitingForPeer) are covered
                // by the to_be_accepted_channels loop below which has the actual funding amount.
                // Skip them here to avoid duplicate entries.
                if record.is_acceptor && record.status == ChannelOpeningStatus::WaitingForPeer {
                    continue;
                }
                // If there's already a ChannelActorState for this channel it was included
                // above (with accurate state from the actor). Skip to avoid duplicates.
                if self
                    .store
                    .get_channel_actor_state(&record.channel_id)
                    .is_some()
                {
                    continue;
                }
                // Apply pubkey filter if provided
                if let Some(ref filter_pk) = filter_pubkey {
                    if filter_pk != &record.pubkey {
                        continue;
                    }
                }
                // Map the ChannelOpenRecord status to the closest ChannelState representation.
                let synthetic_state = match record.status {
                    ChannelOpeningStatus::Failed => {
                        ChannelState::Closed(fiber_json_types::channel::CloseFlags(
                            fiber_json_types::channel::CloseFlags::FUNDING_ABORTED,
                        ))
                    }
                    // Any other in-progress status: show as NegotiatingFunding since we lack
                    // the exact channel sub-state when the actor hasn't yet stored its state.
                    _ => ChannelState::NegotiatingFunding(
                        fiber_json_types::channel::NegotiatingFundingFlags(
                            fiber_json_types::channel::NegotiatingFundingFlags::OUR_INIT_SENT,
                        ),
                    ),
                };
                // For outbound channels the local node contributes funding_amount.
                // For inbound channels (post-failure) funding_amount was the remote peer's share.
                let (local_balance, remote_balance) = if record.is_acceptor {
                    (0u128, record.funding_amount)
                } else {
                    (record.funding_amount, 0u128)
                };
                channels.push(Channel {
                    channel_id: record.channel_id.into(),
                    is_public: false,
                    is_acceptor: record.is_acceptor,
                    is_one_way: false,
                    channel_outpoint: None,
                    pubkey: record.pubkey.into(),
                    funding_udt_type_script: None,
                    state: synthetic_state,
                    local_balance,
                    remote_balance,
                    offered_tlc_balance: 0,
                    received_tlc_balance: 0,
                    pending_tlcs: vec![],
                    latest_commitment_transaction_hash: None,
                    created_at: record.created_at,
                    enabled: false,
                    tlc_expiry_delta: 0,
                    tlc_fee_proportional_millionths: 0,
                    shutdown_transaction_hash: None,
                    failure_detail: record.failure_detail,
                });
            }

            // Include inbound channel requests that are waiting for acceptance
            // (held in the network actor's `to_be_accepted_channels`).
            let pending_accept_msg = |rpc_reply| {
                NetworkActorMessage::Command(NetworkActorCommand::GetPendingAcceptChannels(
                    rpc_reply,
                ))
            };
            let pending_accept = match call!(self.actor, pending_accept_msg) {
                Ok(Ok(list)) => list,
                _ => vec![],
            };
            for pending in pending_accept {
                // Apply pubkey filter if provided
                if let Some(ref filter_pk) = filter_pubkey {
                    if filter_pk != &pending.pubkey {
                        continue;
                    }
                }
                // Skip if there's already a ChannelActorState (unlikely but possible in a race)
                if self
                    .store
                    .get_channel_actor_state(&pending.channel_id)
                    .is_some()
                {
                    continue;
                }
                channels.push(pending_accept_channel_to_rpc(pending));
            }
        }

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
            return Err(rpc_error(
                "close_script and fee_rate should not be set when force is true",
                params,
            ));
        }

        let channel_id = params.channel_id.into();
        let close_script = params.close_script.clone().map(|s| s.into());
        let fee_rate = params.fee_rate.map(FeeRate::from_u64);

        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
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
        let channel_id = params.channel_id.into();
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
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
