use crate::fiber::{
    channel::{
        ChannelActorStateStore, ChannelCommand, ChannelCommandWithId, ChannelOpenRecordStore,
        ShutdownCommand, UpdateCommand,
    },
    channel_signer::SubmitChannelSignatureCommand,
    network::{
        AcceptChannelCommand, OpenChannelCommand, OpenChannelWithExternalFundingCommand,
        PendingAcceptChannel,
    },
    FiberActorCommand, FiberActorMessage, FiberActorRef, NetworkActorMessage,
};
use crate::rpc::utils::{rpc_error, RpcResultExt};
use crate::rpc_scope::{BackendResolver, RpcBackendStore, ScopedBackend};
use crate::{handle_actor_call, log_and_error};
use ckb_types::{
    core::{
        DepType, EpochNumberWithFraction as EpochNumberWithFractionCore, FeeRate, ScriptHashType,
    },
    packed::{self},
    prelude::{IntoTransactionView, Unpack},
};
use fiber_types::{
    ChannelBasePublicKeys, ChannelOpenSignerMaterial, ChannelOpeningStatus,
    ChannelSigningStatus as InternalChannelSigningStatus, NextChannelSignerMaterial, Pubkey,
    SignatureRequestId, TLCId,
};
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;

use jsonrpsee::types::ErrorObjectOwned;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::Extensions;
use musig2::{PartialSignature, PubNonce};
use ractor::{call, ActorRef};
use std::cmp::Reverse;

pub use fiber_json_types::channel::ChannelFeatures as JsonChannelFeatures;
pub use fiber_json_types::{
    AbandonChannelParams, AcceptChannelParams, AcceptChannelResult, Channel, ChannelSigningStatus,
    ChannelState, GetChannelSigningStatusParams, GetChannelSigningStatusResult, Hash256, Htlc,
    ListChannelsParams, ListChannelsResult, OpenChannelParams, OpenChannelResult,
    OpenChannelWithExternalFundingParams, OpenChannelWithExternalFundingResult,
    ShutdownChannelParams, SubmitChannelSignatureParams, SubmitChannelSignatureResult,
    SubmitSignedFundingTxParams, SubmitSignedFundingTxResult, UpdateChannelParams,
};

/// RPC module for channel management.
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait ChannelRpc {
    /// Attempts to open a channel with a peer.
    #[method(name = "open_channel", with_extensions)]
    async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned>;

    /// Accepts a channel opening request from a peer.
    #[method(name = "accept_channel", with_extensions)]
    async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned>;

    /// Abandon a channel, this will remove the channel from the channel manager and DB.
    ///
    /// Without `force`, only channels that have not reached a funded/protected state
    /// can be abandoned. With `force = true`, `AwaitingChannelReady`, `ChannelReady`,
    /// funded and externally funded channels can be abandoned. Channels in
    /// `ShuttingDown` or `Closed` still require shutdown.
    ///
    /// ##### Warning
    /// Forcing an abandon does not close the funding transaction on chain. It
    /// deletes the local channel state and removes watchtower protection, so if
    /// the funding transaction later confirms, funds may be permanently lost.
    /// Use this only when the funding transaction can no longer confirm, and back
    /// up the database first.
    #[method(name = "abandon_channel", with_extensions)]
    async fn abandon_channel(&self, params: AbandonChannelParams) -> Result<(), ErrorObjectOwned>;

    /// Lists all channels.
    #[method(name = "list_channels", with_extensions)]
    async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned>;

    /// Shuts down a channel.
    #[method(name = "shutdown_channel", with_extensions)]
    async fn shutdown_channel(&self, params: ShutdownChannelParams)
        -> Result<(), ErrorObjectOwned>;

    /// Updates a channel.
    #[method(name = "update_channel", with_extensions)]
    async fn update_channel(&self, params: UpdateChannelParams) -> Result<(), ErrorObjectOwned>;

    /// Opens a channel with external funding. The node will negotiate the channel with the peer,
    /// but the user must sign the funding transaction themselves using their own wallet.
    ///
    /// This is useful when the user wants to fund a channel from an external wallet
    /// rather than having the node sign with its internal key.
    ///
    /// Returns the final unsigned funding transaction after internal tx collaboration
    /// has frozen the structure. The user must sign it and submit it with
    /// `submit_signed_funding_tx` without changing the transaction structure.
    ///
    /// For an authenticated hosted tenant, this is also the tenant opening RPC. It requires
    /// the LSP public node as `pubkey`, explicit `public: false`, and
    /// `external_channel_signer`, and the response includes `opening_context`.
    /// Repeating identical parameters resumes the pending opening or returns its frozen result.
    /// With an external channel signer, the result is a proposal for independent signer review.
    /// Complete both initial channel-signing lanes and verify a usable local commitment before
    /// signing or broadcasting wallet inputs. `submit_signed_funding_tx` enforces that ordering
    /// on the node, but cannot prevent a wallet from broadcasting directly to CKB.
    #[method(name = "open_channel_with_external_funding", with_extensions)]
    async fn open_channel_with_external_funding(
        &self,
        params: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, ErrorObjectOwned>;

    /// Submits a signed funding transaction for an externally funded channel.
    ///
    /// After calling `open_channel_with_external_funding`, the user signs the returned
    /// final negotiated unsigned transaction with their wallet and submits it here.
    /// The signed transaction should be directly broadcastable and will not be structurally modified.
    ///
    /// External signers must keep `inputs`, `outputs`, `outputs_data`, and `cell_deps`
    /// unchanged. See the [external funding guide](../../../../docs/external-funding.md)
    /// for signing details and examples.
    #[method(name = "submit_signed_funding_tx", with_extensions)]
    async fn submit_signed_funding_tx(
        &self,
        params: SubmitSignedFundingTxParams,
    ) -> Result<SubmitSignedFundingTxResult, ErrorObjectOwned>;

    /// Reads the current external signing status for a channel.
    ///
    /// This method reads persisted channel state and does not require the channel actor
    /// to process a command. When the status is `SignatureRequired`, the response includes
    /// the structured MuSig2 plaintext. An external signer hashes that plaintext independently
    /// and submits the resulting partial signature with `submit_channel_signature`.
    #[method(name = "get_channel_signing_status", with_extensions)]
    async fn get_channel_signing_status(
        &self,
        params: GetChannelSigningStatusParams,
    ) -> Result<GetChannelSigningStatusResult, ErrorObjectOwned>;

    /// Submits a partial signature for the channel's current outstanding signing request.
    ///
    /// The node verifies `request_id` against the persisted request, checks the partial
    /// signature against the saved plaintext, then resumes the channel
    /// state machine. The caller cannot replace the transaction or signing content.
    /// Optional `next_material` supplies the next commitment point and public nonces.
    #[method(name = "submit_channel_signature", with_extensions)]
    async fn submit_channel_signature(
        &self,
        params: SubmitChannelSignatureParams,
    ) -> Result<SubmitChannelSignatureResult, ErrorObjectOwned>;
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
        channel_features: JsonChannelFeatures::default(),
        // Report as NegotiatingFunding since we're still awaiting local acceptance
        state: ChannelState::NegotiatingFunding(
            fiber_json_types::channel::NegotiatingFundingFlags(0),
        ),
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

/// RAII guard for an in-flight hosted tenant watchtower channel quota reservation.
///
/// Dropping the guard without calling [`Self::confirm`] automatically releases the
/// reservation in `LspService` (covering RPC errors, timeouts, and dropped/cancelled
/// futures) and refreshes that tenant's persisted pending count.
#[cfg(not(target_arch = "wasm32"))]
struct WatchtowerQuotaReservationGuard {
    #[cfg(feature = "lsp")]
    lsp_actor: ractor::ActorRef<crate::lsp::LspServiceMessage>,
    #[cfg(feature = "lsp")]
    tenant_id: crate::fiber_types::TenantId,
    #[cfg(feature = "lsp")]
    reservation_id: u64,
    #[cfg(feature = "lsp")]
    released: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl WatchtowerQuotaReservationGuard {
    fn confirm(#[allow(unused_mut)] mut self, opened_channel_id: fiber_types::Hash256) {
        #[cfg(feature = "lsp")]
        {
            self.released = true;
            let _ = self.lsp_actor.send_message(
                crate::lsp::LspServiceMessage::ReleaseTenantWatchtowerChannelQuota {
                    tenant_id: self.tenant_id.clone(),
                    reservation_id: self.reservation_id,
                    opened_channel_id: Some(opened_channel_id),
                },
            );
        }
        #[cfg(not(feature = "lsp"))]
        let _ = opened_channel_id;
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for WatchtowerQuotaReservationGuard {
    fn drop(&mut self) {
        #[cfg(feature = "lsp")]
        if !self.released {
            let _ = self.lsp_actor.send_message(
                crate::lsp::LspServiceMessage::ReleaseTenantWatchtowerChannelQuota {
                    tenant_id: self.tenant_id.clone(),
                    reservation_id: self.reservation_id,
                    opened_channel_id: None,
                },
            );
        }
    }
}

pub struct ChannelRpcServerImpl<S> {
    backend: BackendResolver<S>,
}

impl<S: RpcBackendStore> ChannelRpcServerImpl<S> {
    pub fn new(actor: ActorRef<NetworkActorMessage>, store: S) -> Self {
        Self::new_fiber(FiberActorRef::from_network(&actor), store)
    }

    pub(crate) fn new_fiber(actor: FiberActorRef, store: S) -> Self {
        ChannelRpcServerImpl {
            backend: BackendResolver::new(Some(actor), store, None),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_lsp_actor(mut self, lsp_actor: crate::rpc::tenant::LspActorHandle) -> Self {
        self.backend = self.backend.with_lsp_actor(lsp_actor);
        self
    }

    /// Resolve the caller's tenant and wrap it for the handler bodies.
    #[cfg(not(target_arch = "wasm32"))]
    async fn backend_for(
        &self,
        extensions: &Extensions,
    ) -> Result<ChannelBackend<S>, ErrorObjectOwned> {
        Ok(ChannelBackend(self.backend.resolve(extensions).await?))
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn reserve_tenant_watchtower_admission(
        &self,
        tenant_id: &crate::fiber_types::TenantId,
        opening_temp_channel_id: Option<fiber_types::Hash256>,
    ) -> Result<WatchtowerQuotaReservationGuard, ErrorObjectOwned> {
        #[cfg(feature = "lsp")]
        {
            let lsp_actor = self
                .backend
                .lsp()
                .as_ref()
                .ok_or_else(|| rpc_error("hosted LSP service is not enabled"))?
                .clone();
            let reservation_id = ractor::call_t!(
                lsp_actor,
                |reply| crate::lsp::LspServiceMessage::ReserveTenantWatchtowerChannelQuota {
                    tenant_id: tenant_id.clone(),
                    opening_temp_channel_id,
                    reply,
                },
                10_000
            )
            .rpc_err()?
            .rpc_err()?;
            Ok(WatchtowerQuotaReservationGuard {
                lsp_actor,
                tenant_id: tenant_id.clone(),
                reservation_id,
                released: false,
            })
        }
        #[cfg(not(feature = "lsp"))]
        {
            let _ = (tenant_id, opening_temp_channel_id);
            Ok(WatchtowerQuotaReservationGuard {})
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_tenant_watchtower_pending(&self, tenant_id: &crate::fiber_types::TenantId) {
        #[cfg(feature = "lsp")]
        if let Some(lsp_actor) = self.backend.lsp().as_ref() {
            let _ = lsp_actor.send_message(
                crate::lsp::LspServiceMessage::RefreshTenantWatchtowerChannelQuota(
                    tenant_id.clone(),
                ),
            );
        }
        #[cfg(not(feature = "lsp"))]
        let _ = tenant_id;
    }

    /// The public backend, for callers that hold no request (wasm bindings).
    ///
    /// Not available to handlers: they must resolve the request's tenant first.
    #[cfg(any(test, target_arch = "wasm32"))]
    pub fn public_handler(&self) -> ChannelBackend<S> {
        ChannelBackend(self.backend.public_scope())
    }
}

/// Everything a channel handler runs against.
///
/// A thin view over the resolved [`ScopedBackend`]; the bodies live here, so the
/// registered RPC type holds no actor at all.
#[doc(hidden)]
pub struct ChannelBackend<S>(ScopedBackend<S>);

impl<S> std::ops::Deref for ChannelBackend<S> {
    type Target = ScopedBackend<S>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> ChannelRpcServer for ChannelRpcServerImpl<S>
where
    S: ChannelActorStateStore + ChannelOpenRecordStore + RpcBackendStore + Send + Sync + 'static,
{
    /// Attempts to open a channel with a peer.
    async fn open_channel(
        &self,
        extensions: &Extensions,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned> {
        let backend = self.backend_for(extensions).await?;
        let reservation = if let Some(tenant) = backend.tenant() {
            let pubkey = Pubkey::try_from(params.pubkey).rpc_err()?;
            if pubkey != tenant.public_node_id {
                return Err(rpc_error(
                    "hosted tenants may only open a channel to the public LSP node",
                ));
            }
            if params.public.unwrap_or(true) {
                return Err(rpc_error("hosted tenant channels must be private"));
            }
            Some(
                self.reserve_tenant_watchtower_admission(&tenant.tenant_id, None)
                    .await?,
            )
        } else {
            None
        };
        let result = backend.open_channel(params).await;
        if let (Ok(opened), Some(guard)) = (&result, reservation) {
            guard.confirm(opened.temporary_channel_id.into());
        }
        result
    }

    /// Accepts a channel opening request from a peer.
    async fn accept_channel(
        &self,
        extensions: &Extensions,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned> {
        self.backend_for(extensions)
            .await?
            .accept_channel(params)
            .await
    }

    /// Abandon a channel, this will remove the channel from the channel manager and DB.
    /// Without `force`, only channels that have not reached a funded/protected state
    /// can be abandoned. `force = true` also permits `AwaitingChannelReady` and
    /// `ChannelReady` channels, but still rejects `ShuttingDown` and `Closed`.
    async fn abandon_channel(
        &self,
        extensions: &Extensions,
        params: AbandonChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let backend = self.backend_for(extensions).await?;
        let tenant_id = backend.tenant().map(|tenant| tenant.tenant_id.clone());
        let result = backend.abandon_channel(params).await;
        if result.is_ok() {
            if let Some(tenant_id) = tenant_id {
                self.refresh_tenant_watchtower_pending(&tenant_id);
            }
        }
        result
    }

    /// Lists all channels.
    async fn list_channels(
        &self,
        extensions: &Extensions,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned> {
        self.backend_for(extensions)
            .await?
            .list_channels(params)
            .await
    }

    /// Shuts down a channel.
    async fn shutdown_channel(
        &self,
        extensions: &Extensions,
        params: ShutdownChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let backend = self.backend_for(extensions).await?;
        let tenant_id = backend.tenant().map(|tenant| tenant.tenant_id.clone());
        let result = backend.shutdown_channel(params).await;
        if result.is_ok() {
            if let Some(tenant_id) = tenant_id {
                self.refresh_tenant_watchtower_pending(&tenant_id);
            }
        }
        result
    }

    /// Updates a channel.
    async fn update_channel(
        &self,
        extensions: &Extensions,
        params: UpdateChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.backend_for(extensions)
            .await?
            .update_channel(params)
            .await
    }

    /// Opens a channel with external funding.
    async fn open_channel_with_external_funding(
        &self,
        extensions: &Extensions,
        params: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, ErrorObjectOwned> {
        let backend = self.backend_for(extensions).await?;
        if let Some(tenant) = backend.tenant() {
            validate_tenant_opening_params(&params, tenant.public_node_id.into())?;
            let command = external_funding_command(&params)?;
            let actor = backend.actor_opt().cloned().ok_or_else(|| {
                rpc_error("tenant channel opening requires a running Fiber actor")
            })?;
            let opening_temp_channel_id = command.external_channel_signer.as_ref().map(|signer| {
                crate::fiber::channel::derive_temp_channel_id_from_tlc_key(
                    &signer.base_public_keys.tlc_base_key,
                )
            });
            let (reservation, admission_error) = match self
                .reserve_tenant_watchtower_admission(&tenant.tenant_id, opening_temp_channel_id)
                .await
            {
                Ok(guard) => (Some(guard), None),
                Err(error) => (None, Some(error.message().to_string())),
            };
            // The actor owns the durable tenant opening; a disconnected caller can retry an
            // existing in-flight opening even when quota is full, while any new opening is gated
            // on `admission_error` inside `open_tenant_channel_inner`.
            let result = ractor::call_t!(
                actor,
                |reply| FiberActorMessage::new_command(FiberActorCommand::OpenTenantChannel {
                    command,
                    admission_error,
                    reply
                }),
                30_000
            )
            .map_err(|e| {
                rpc_error(format!(
                    "tenant opening interrupted; retry the same request: {e}"
                ))
            })?
            .map_err(rpc_error);
            if let (Ok(opened), Some(guard)) = (&result, reservation) {
                guard.confirm(opened.channel_id.into());
            }
            return result;
        }
        backend.open_channel_with_external_funding(params).await
    }

    /// Submits a signed funding transaction for an externally funded channel.
    async fn submit_signed_funding_tx(
        &self,
        extensions: &Extensions,
        params: SubmitSignedFundingTxParams,
    ) -> Result<SubmitSignedFundingTxResult, ErrorObjectOwned> {
        let backend = self.backend_for(extensions).await?;
        let tenant_id = backend.tenant().map(|tenant| tenant.tenant_id.clone());
        let result = backend.submit_signed_funding_tx(params).await;
        if result.is_ok() {
            if let Some(tenant_id) = tenant_id {
                self.refresh_tenant_watchtower_pending(&tenant_id);
            }
        }
        result
    }

    async fn get_channel_signing_status(
        &self,
        extensions: &Extensions,
        params: GetChannelSigningStatusParams,
    ) -> Result<GetChannelSigningStatusResult, ErrorObjectOwned> {
        #[cfg(not(feature = "lsp"))]
        let _ = extensions;
        #[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
        if let Some(tenant) = extensions.get::<crate::rpc::tenant::AuthenticatedTenant>() {
            let actor = self
                .backend
                .lsp()
                .as_ref()
                .ok_or_else(|| rpc_error("hosted LSP service is not enabled"))?;
            let channel_id: fiber_types::Hash256 = params.channel_id.into();
            let state = call!(actor, |reply| {
                crate::lsp::LspServiceMessage::GetTenantSigningStatus {
                    tenant_id: tenant.0.clone(),
                    channel_id,
                    reply,
                }
            })
            .rpc_err()?
            .rpc_err()?;
            return Ok(GetChannelSigningStatusResult {
                channel_id: params.channel_id,
                status: to_rpc_channel_signing_status(&state).rpc_err()?,
            });
        }
        self.backend_for(extensions)
            .await?
            .get_channel_signing_status(params)
            .await
    }

    async fn submit_channel_signature(
        &self,
        extensions: &Extensions,
        params: SubmitChannelSignatureParams,
    ) -> Result<SubmitChannelSignatureResult, ErrorObjectOwned> {
        self.backend_for(extensions)
            .await?
            .submit_channel_signature(params)
            .await
    }
}
impl<S> ChannelBackend<S>
where
    S: ChannelActorStateStore + ChannelOpenRecordStore + Send + Sync + 'static,
{
    pub async fn open_channel(
        &self,
        params: OpenChannelParams,
    ) -> Result<OpenChannelResult, ErrorObjectOwned> {
        let pubkey = Pubkey::try_from(params.pubkey).rpc_err()?;
        let message = |rpc_reply| {
            FiberActorMessage::new_command(FiberActorCommand::OpenChannel(
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
        handle_actor_call!(self.actor()?, message, params).map(|response| OpenChannelResult {
            temporary_channel_id: response.channel_id.into(),
        })
    }

    pub async fn accept_channel(
        &self,
        params: AcceptChannelParams,
    ) -> Result<AcceptChannelResult, ErrorObjectOwned> {
        let temp_channel_id = params.temporary_channel_id.into();
        let message = |rpc_reply| {
            FiberActorMessage::new_command(FiberActorCommand::AcceptChannel(
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

        handle_actor_call!(self.actor()?, message, params).map(|response| AcceptChannelResult {
            channel_id: response.new_channel_id.into(),
        })
    }

    pub async fn abandon_channel(
        &self,
        params: AbandonChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let channel_id = params.channel_id.into();
        let force = params.force.unwrap_or_default();
        let message = |rpc_reply| {
            FiberActorMessage::new_command(FiberActorCommand::AbandonChannel(
                channel_id, force, rpc_reply,
            ))
        };
        handle_actor_call!(self.actor()?, message, params)
    }

    pub async fn list_channels(
        &self,
        params: ListChannelsParams,
    ) -> Result<ListChannelsResult, ErrorObjectOwned> {
        let only_pending = params.only_pending.unwrap_or_default();
        let include_closed = params.include_closed.unwrap_or_default();

        // Convert the optional String pubkey filter to internal Pubkey
        let filter_pubkey = params.pubkey.map(Pubkey::try_from).transpose().rpc_err()?;

        // The two filter options are mutually exclusive: `only_pending` narrows to channels
        // that are still opening (or failed to open), while `include_closed` broadens to
        // all channels including successfully closed ones. Allowing both simultaneously
        // would produce confusing results.
        if only_pending && include_closed {
            return Err(rpc_error(
                "only_pending and include_closed are mutually exclusive",
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
        let mut channels = Vec::new();
        for (_pubkey, channel_id, channel_state) in channel_states {
            // The index carries the channel state written in the same batch as the record, so the
            // pending filter belongs here: it skips the record read of every channel it rejects.
            if only_pending && !ChannelState::from(channel_state).is_pending() {
                continue;
            }
            let Some(state) = self.store.get_channel_actor_state(&channel_id) else {
                continue;
            };
            let rpc_state: ChannelState = state.state.into();
            let offered_tlc_balance = state.get_offered_tlc_balance().rpc_err()?;
            let received_tlc_balance = state.get_received_tlc_balance().rpc_err()?;
            // Enrich with failure_detail from ChannelOpenRecord when available
            let failure_detail = self
                .store
                .get_channel_open_record(&channel_id)
                .and_then(|r| r.failure_detail);
            channels.push(Channel {
                channel_id: channel_id.into(),
                is_public: state.is_public(),
                is_acceptor: state.is_acceptor,
                is_one_way: state.is_one_way,
                channel_outpoint: state.get_funding_transaction_outpoint(),
                pubkey: state.remote_pubkey.into(),
                funding_udt_type_script: state.funding_udt_type_script.clone().map(Into::into),
                channel_features: JsonChannelFeatures(state.channel_features.bits()),
                state: rpc_state,
                local_balance: state.get_local_balance(),
                remote_balance: state.get_remote_balance(),
                offered_tlc_balance,
                received_tlc_balance,
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
                    .latest_commitment_transaction()
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
            });
        }

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
                    channel_features: JsonChannelFeatures(record.channel_features.bits()),
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
                FiberActorMessage::new_command(FiberActorCommand::GetPendingAcceptChannels(
                    rpc_reply,
                ))
            };
            let pending_accept = match self.actor_opt() {
                Some(actor) => match call!(actor, pending_accept_msg) {
                    Ok(Ok(list)) => list,
                    _ => vec![],
                },
                None => vec![],
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
            ));
        }

        let channel_id = params.channel_id.into();
        let close_script = params.close_script.clone().map(|s| s.into());
        let fee_rate = params.fee_rate.map(FeeRate::from_u64);

        let message = |rpc_reply| -> FiberActorMessage {
            FiberActorMessage::new_command(FiberActorCommand::ControlFiberChannel(
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
        handle_actor_call!(self.actor()?, message, params)
    }

    pub async fn update_channel(
        &self,
        params: UpdateChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let channel_id = params.channel_id.into();
        let message = |rpc_reply| -> FiberActorMessage {
            FiberActorMessage::new_command(FiberActorCommand::ControlFiberChannel(
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
        handle_actor_call!(self.actor()?, message, params)
    }

    /// Opens a channel with external funding.
    pub async fn open_channel_with_external_funding(
        &self,
        params: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, ErrorObjectOwned> {
        let command = external_funding_command(&params)?;
        let message = |rpc_reply| {
            FiberActorMessage::new_command(FiberActorCommand::OpenChannelWithExternalFunding(
                command, rpc_reply,
            ))
        };
        let response = handle_actor_call!(self.actor()?, message, params)?;
        Ok(OpenChannelWithExternalFundingResult {
            channel_id: response.channel_id.into(),
            unsigned_funding_tx: try_into_rpc_transaction(response.unsigned_funding_tx)
                .rpc_err()?,
            opening_context: None,
        })
    }

    /// Submits a signed funding transaction for an externally funded channel.
    pub async fn submit_signed_funding_tx(
        &self,
        params: SubmitSignedFundingTxParams,
    ) -> Result<SubmitSignedFundingTxResult, ErrorObjectOwned> {
        let channel_id: fiber_types::Hash256 = params.channel_id.into();
        let signed_tx: packed::Transaction = params.signed_funding_tx.clone().into();
        let message = |rpc_reply| {
            FiberActorMessage::new_command(FiberActorCommand::SubmitSignedFundingTx {
                channel_id,
                signed_tx: signed_tx.clone(),
                reply: rpc_reply,
            })
        };
        handle_actor_call!(self.actor()?, message, params).map(|tx_hash| {
            SubmitSignedFundingTxResult {
                channel_id: channel_id.into(),
                funding_tx_hash: tx_hash.into(),
            }
        })
    }

    /// Reads persisted signer state for one channel.
    pub async fn get_channel_signing_status(
        &self,
        params: GetChannelSigningStatusParams,
    ) -> Result<GetChannelSigningStatusResult, ErrorObjectOwned> {
        let channel_id: fiber_types::Hash256 = params.channel_id.into();
        let Some(state) = self.store.get_channel_actor_state(&channel_id) else {
            return Err(rpc_error(format!("channel {channel_id:?} not found")));
        };
        Ok(GetChannelSigningStatusResult {
            channel_id: channel_id.into(),
            status: to_rpc_channel_signing_status(&state).rpc_err()?,
        })
    }

    /// Submits a verified external signature and resumes the channel actor.
    pub async fn submit_channel_signature(
        &self,
        params: SubmitChannelSignatureParams,
    ) -> Result<SubmitChannelSignatureResult, ErrorObjectOwned> {
        let channel_id: fiber_types::Hash256 = params.channel_id.into();
        let request_id = SignatureRequestId(params.request_id.into());
        let partial_signature =
            PartialSignature::from_slice(&params.partial_signature).rpc_err()?;
        let next_material = params
            .next_material
            .clone()
            .map(try_into_next_channel_signer_material)
            .transpose()
            .rpc_err()?;
        let message = |rpc_reply| {
            FiberActorMessage::new_command(FiberActorCommand::SubmitChannelSignature(
                SubmitChannelSignatureCommand {
                    channel_id,
                    request_id,
                    partial_signature,
                    next_material: next_material.clone(),
                },
                rpc_reply,
            ))
        };
        handle_actor_call!(self.actor()?, message, params).map(|outcome| match outcome {
            fiber_types::SubmitSignatureOutcome::Applied => SubmitChannelSignatureResult::Applied,
            fiber_types::SubmitSignatureOutcome::AlreadyApplied => {
                SubmitChannelSignatureResult::AlreadyApplied
            }
        })
    }
}

pub(crate) fn to_rpc_channel_signing_status(
    state: &crate::fiber::channel::ChannelActorState,
) -> Result<ChannelSigningStatus, String> {
    Ok(match state.channel_signing_status() {
        InternalChannelSigningStatus::NoSignatureRequired => {
            ChannelSigningStatus::NoSignatureRequired
        }
        InternalChannelSigningStatus::SignatureRequired {
            request_id,
            transition,
            content,
            settlement_data,
        } => {
            let for_remote = matches!(
                transition,
                fiber_types::ChannelSigningTransition::SendCommitmentSigned
            );
            ChannelSigningStatus::SignatureRequired {
                request_id: request_id.0.into(),
                transition: to_rpc_signing_transition(transition),
                session_evidence: {
                    let (nonce, signature) = state.external_signing_peer_evidence()?;
                    fiber_json_types::SigningSessionEvidence {
                        peer_public_nonce: nonce.serialize().to_vec(),
                        peer_partial_signature: signature.map(|s| s.serialize().to_vec()),
                    }
                },
                content: to_rpc_musig2_signing_content(content)?,
                settlement: settlement_data
                    .map(|settlement| {
                        let remote = state
                            .remote_channel_public_keys
                            .as_ref()
                            .ok_or("missing remote channel public keys")?;
                        to_rpc_signing_settlement(
                            settlement,
                            state.local_channel_public_keys.tlc_base_key,
                            remote.tlc_base_key,
                            for_remote,
                        )
                    })
                    .transpose()?,
            }
        }
    })
}

fn to_rpc_signing_settlement(
    settlement: fiber_types::SettlementData,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
    for_remote: bool,
) -> Result<fiber_json_types::SigningSettlement, String> {
    Ok(fiber_json_types::SigningSettlement {
        local_amount: settlement.local_amount,
        remote_amount: settlement.remote_amount,
        local_settlement_pubkey: local_settlement_key.into(),
        remote_settlement_pubkey: remote_settlement_key.into(),
        for_remote,
        tlcs: settlement
            .tlcs
            .into_iter()
            .map(|tlc| {
                Ok(fiber_json_types::SigningSettlementTlc {
                    tlc_id: u64::from(tlc.tlc_id),
                    local_key_commitment_number: tlc
                        .local_key_commitment_number
                        .ok_or("missing TLC key derivation index")?,
                    inbound: matches!(tlc.tlc_id, TLCId::Received(_)) == for_remote,
                    payment_hash: tlc.payment_hash.into(),
                    payment_amount: tlc.payment_amount,
                    hash_algorithm: tlc.hash_algorithm.into(),
                    expiry: tlc.expiry,
                    local_key_pubkey: tlc.local_pubkey().into(),
                    remote_key: tlc.remote_key.into(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
    })
}

fn to_rpc_signing_transition(
    transition: fiber_types::ChannelSigningTransition,
) -> fiber_json_types::ChannelSigningTransition {
    match transition {
        fiber_types::ChannelSigningTransition::SendCommitmentSigned => {
            fiber_json_types::ChannelSigningTransition::SendCommitmentSigned
        }
        fiber_types::ChannelSigningTransition::CompleteReceivedCommitment => {
            fiber_json_types::ChannelSigningTransition::CompleteReceivedCommitment
        }
        fiber_types::ChannelSigningTransition::SendRevokeAndAck => {
            fiber_json_types::ChannelSigningTransition::SendRevokeAndAck
        }
        fiber_types::ChannelSigningTransition::CompleteReceivedRevokeAndAck => {
            fiber_json_types::ChannelSigningTransition::CompleteReceivedRevokeAndAck
        }
        fiber_types::ChannelSigningTransition::SendClosingSigned => {
            fiber_json_types::ChannelSigningTransition::SendClosingSigned
        }
        fiber_types::ChannelSigningTransition::SignChannelAnnouncement => {
            fiber_json_types::ChannelSigningTransition::SignChannelAnnouncement
        }
    }
}

fn to_rpc_musig2_signing_content(
    content: fiber_types::Musig2SigningContent,
) -> Result<fiber_json_types::Musig2SigningContent, String> {
    Ok(fiber_json_types::Musig2SigningContent {
        v2: Some({
            let binding = content.v2.ok_or("missing V2 external nonce allocation")?;
            fiber_json_types::V2NonceBinding {
                channel_id: binding.channel_id.into(),
                owner: binding.owner.into(),
                public_nonce: binding.public_nonce.serialize().to_vec(),
            }
        }),
        slot: fiber_json_types::NonceSlot {
            purpose: to_rpc_nonce_purpose(content.slot.purpose),
            commitment_number: content.slot.commitment_number,
        },
        commitment_counter: content.commitment_counter.map(to_rpc_commitment_counter),
        key_agg_ctx: content.key_agg_ctx.serialize(),
        agg_nonce: content.agg_nonce.serialize().to_vec(),
        content: to_rpc_musig2_signable_content(content.content)?,
    })
}

fn to_rpc_nonce_purpose(purpose: fiber_types::NoncePurpose) -> fiber_json_types::NoncePurpose {
    match purpose {
        fiber_types::NoncePurpose::Commitment => fiber_json_types::NoncePurpose::Commitment,
        fiber_types::NoncePurpose::Revocation => fiber_json_types::NoncePurpose::Revocation,
        fiber_types::NoncePurpose::Closing => fiber_json_types::NoncePurpose::Closing,
        fiber_types::NoncePurpose::ChannelAnnouncement => {
            fiber_json_types::NoncePurpose::ChannelAnnouncement
        }
    }
}

fn to_rpc_commitment_counter(
    counter: fiber_types::CommitmentCounter,
) -> fiber_json_types::CommitmentCounter {
    match counter {
        fiber_types::CommitmentCounter::Local => fiber_json_types::CommitmentCounter::Local,
        fiber_types::CommitmentCounter::Remote => fiber_json_types::CommitmentCounter::Remote,
    }
}

/// Validates that a packed `Script` has a valid `ScriptHashType` before JSON-RPC conversion.
pub(crate) fn validate_packed_script_hash_type(script: &packed::Script) -> Result<(), String> {
    ScriptHashType::try_from(script.hash_type())
        .map(|_| ())
        .map_err(|err| format!("invalid script hash_type: {err}"))
}

/// Converts a packed `Script` into `ckb_jsonrpc_types::Script`, rejecting invalid `hash_type` bytes.
pub(crate) fn try_into_rpc_script(
    script: packed::Script,
) -> Result<ckb_jsonrpc_types::Script, String> {
    validate_packed_script_hash_type(&script)?;
    Ok(script.into())
}

/// Converts a packed `Transaction` into `ckb_jsonrpc_types::Transaction` without panicking on
/// invalid `dep_type` or `hash_type` bytes.
pub(crate) fn try_into_rpc_transaction(
    tx: packed::Transaction,
) -> Result<ckb_jsonrpc_types::Transaction, String> {
    let raw = tx.raw();
    for cell_dep in raw.cell_deps() {
        DepType::try_from(cell_dep.dep_type())
            .map_err(|err| format!("invalid cell_dep dep_type: {err}"))?;
    }
    for output in raw.outputs() {
        validate_packed_script_hash_type(&output.lock())?;
        if let Some(type_script) = output.type_().to_opt() {
            validate_packed_script_hash_type(&type_script)?;
        }
    }
    Ok(tx.into())
}

fn to_rpc_musig2_signable_content(
    content: fiber_types::Musig2SignableContent,
) -> Result<fiber_json_types::Musig2SignableContent, String> {
    if matches!(
        content,
        fiber_types::Musig2SignableContent::ChannelAnnouncement(_)
    ) {
        return Ok(
            fiber_json_types::Musig2SignableContent::ChannelAnnouncement {
                unsigned_announcement: content.canonical_bytes(),
            },
        );
    }
    match content {
        fiber_types::Musig2SignableContent::CommitmentTransaction(transaction) => Ok(
            fiber_json_types::Musig2SignableContent::CommitmentTransaction {
                transaction: try_into_rpc_transaction(transaction)?,
            },
        ),
        fiber_types::Musig2SignableContent::CooperativeCloseTransaction(transaction) => Ok(
            fiber_json_types::Musig2SignableContent::CooperativeCloseTransaction {
                transaction: try_into_rpc_transaction(transaction)?,
            },
        ),
        fiber_types::Musig2SignableContent::Revocation {
            output,
            output_data,
            commitment_lock_script_args,
        } => Ok(fiber_json_types::Musig2SignableContent::Revocation {
            output,
            output_data,
            commitment_lock_script_args,
        }),
        fiber_types::Musig2SignableContent::ChannelAnnouncement(_) => {
            unreachable!("ChannelAnnouncement is converted from canonical bytes before this match")
        }
    }
}

fn try_into_pub_nonce(bytes: Vec<u8>) -> Result<PubNonce, String> {
    PubNonce::from_bytes(&bytes).map_err(|error| error.to_string())
}

fn try_into_channel_open_signer_material(
    material: fiber_json_types::ChannelOpenSignerMaterial,
) -> Result<ChannelOpenSignerMaterial, String> {
    if material.v2_nonces.len() > fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH {
        return Err(format!(
            "v2_nonces batch size {} exceeds maximum ({})",
            material.v2_nonces.len(),
            fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH
        ));
    }
    Ok(ChannelOpenSignerMaterial {
        v2_nonces: material
            .v2_nonces
            .into_iter()
            .map(try_into_pub_nonce)
            .collect::<Result<_, _>>()?,
        base_public_keys: ChannelBasePublicKeys {
            funding_pubkey: Pubkey::try_from(material.base_public_keys.funding_pubkey)?,
            tlc_base_key: Pubkey::try_from(material.base_public_keys.tlc_base_key)?,
        },
        first_commitment_point: Pubkey::try_from(material.first_commitment_point)?,
        second_commitment_point: Pubkey::try_from(material.second_commitment_point)?,
        commitment_nonce: try_into_pub_nonce(material.commitment_nonce)?,
        next_commitment_nonce: try_into_pub_nonce(material.next_commitment_nonce)?,
        revocation_nonce: try_into_pub_nonce(material.revocation_nonce)?,
        channel_announcement_nonce: material
            .channel_announcement_nonce
            .map(try_into_pub_nonce)
            .transpose()?,
    })
}

fn try_into_next_channel_signer_material(
    material: fiber_json_types::NextChannelSignerMaterial,
) -> Result<NextChannelSignerMaterial, String> {
    if material.v2_nonces.len() > fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH {
        return Err(format!(
            "v2_nonces batch size {} exceeds maximum ({})",
            material.v2_nonces.len(),
            fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH
        ));
    }
    if material.commitment_points.len()
        > fiber_types::channel_v2_validation::MAX_V2_COMMITMENT_POINT_BATCH
    {
        return Err(format!(
            "commitment_points batch size {} exceeds maximum ({})",
            material.commitment_points.len(),
            fiber_types::channel_v2_validation::MAX_V2_COMMITMENT_POINT_BATCH
        ));
    }
    Ok(NextChannelSignerMaterial {
        v2_nonces: material
            .v2_nonces
            .into_iter()
            .map(try_into_pub_nonce)
            .collect::<Result<_, _>>()?,
        commitment_points: material
            .commitment_points
            .into_iter()
            .map(|(number, point)| Ok((number, Pubkey::try_from(point)?)))
            .collect::<Result<_, String>>()?,
        next_commitment_point: material
            .next_commitment_point
            .map(Pubkey::try_from)
            .transpose()?,
        next_commitment_nonce: material
            .next_commitment_nonce
            .map(try_into_pub_nonce)
            .transpose()?,
        next_revocation_nonce: material
            .next_revocation_nonce
            .map(try_into_pub_nonce)
            .transpose()?,
    })
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[cfg(feature = "lsp")]
pub(crate) fn to_rpc_channel_open_signer_material(
    material: &ChannelOpenSignerMaterial,
) -> fiber_json_types::ChannelOpenSignerMaterial {
    fiber_json_types::ChannelOpenSignerMaterial {
        v2_nonces: material
            .v2_nonces
            .iter()
            .map(|nonce| nonce.serialize().to_vec())
            .collect(),
        base_public_keys: fiber_json_types::ChannelBasePublicKeys {
            funding_pubkey: material.base_public_keys.funding_pubkey.into(),
            tlc_base_key: material.base_public_keys.tlc_base_key.into(),
        },
        first_commitment_point: material.first_commitment_point.into(),
        second_commitment_point: material.second_commitment_point.into(),
        commitment_nonce: material.commitment_nonce.serialize().to_vec(),
        next_commitment_nonce: material.next_commitment_nonce.serialize().to_vec(),
        revocation_nonce: material.revocation_nonce.serialize().to_vec(),
        channel_announcement_nonce: material
            .channel_announcement_nonce
            .as_ref()
            .map(|nonce| nonce.serialize().to_vec()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_tenant_opening_params(
    params: &OpenChannelWithExternalFundingParams,
    public_node_id: fiber_json_types::Pubkey,
) -> Result<(), ErrorObjectOwned> {
    if params.pubkey != public_node_id {
        return Err(rpc_error("tenant channel peer must be the LSP public node"));
    }
    if params.public != Some(false) {
        return Err(rpc_error("tenant channel requires public: false"));
    }
    if params.external_channel_signer.is_none() {
        return Err(rpc_error("tenant channel requires external_channel_signer"));
    }
    Ok(())
}

pub(crate) fn external_funding_command(
    params: &OpenChannelWithExternalFundingParams,
) -> Result<OpenChannelWithExternalFundingCommand, ErrorObjectOwned> {
    let pubkey = Pubkey::try_from(params.pubkey).rpc_err()?;
    let funding_lock_script_cell_deps: Vec<packed::CellDep> = params
        .funding_lock_script_cell_deps
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect();
    let external_channel_signer = params
        .external_channel_signer
        .clone()
        .map(try_into_channel_open_signer_material)
        .transpose()
        .rpc_err()?;
    Ok(OpenChannelWithExternalFundingCommand {
        pubkey,
        funding_amount: params.funding_amount,
        public: params.public.unwrap_or(true),
        shutdown_script: params.shutdown_script.clone().into(),
        funding_lock_script: params.funding_lock_script.clone().into(),
        funding_lock_script_cell_deps: funding_lock_script_cell_deps.clone(),
        commitment_delay_epoch: params
            .commitment_delay_epoch
            .map(|e| EpochNumberWithFractionCore::from_full_value(e.value())),
        funding_udt_type_script: params.funding_udt_type_script.clone().map(|s| s.into()),
        commitment_fee_rate: params.commitment_fee_rate,
        funding_fee_rate: params.funding_fee_rate,
        tlc_expiry_delta: params.tlc_expiry_delta,
        tlc_min_value: params.tlc_min_value,
        tlc_fee_proportional_millionths: params.tlc_fee_proportional_millionths,
        max_tlc_value_in_flight: params.max_tlc_value_in_flight,
        max_tlc_number_in_flight: params.max_tlc_number_in_flight,
        external_channel_signer: external_channel_signer.clone(),
    })
}

#[cfg(test)]
mod signing_snapshot_tests {
    use fiber_types::{Privkey, SettlementData, SettlementTlc, TLCId};

    use super::{
        to_rpc_musig2_signing_content, to_rpc_signing_settlement,
        try_into_next_channel_signer_material,
    };

    #[test]
    fn v2_signing_projection_preserves_closing_allocation_and_rejects_missing_binding() {
        let local = Privkey::from(&[1; 32]).pubkey();
        let remote = Privkey::from(&[2; 32]).pubkey();
        let public_nonce = musig2::SecNonce::build([3; 32]).build().public_nonce();
        let binding = fiber_types::V2NonceBinding {
            channel_id: [4; 32].into(),
            owner: local,
            public_nonce: public_nonce.clone(),
        };
        let mut content = fiber_types::Musig2SigningContent {
            v2: Some(binding.clone()),
            slot: fiber_types::NonceSlot {
                purpose: fiber_types::NoncePurpose::Closing,
                commitment_number: 0,
            },
            commitment_counter: None,
            key_agg_ctx: musig2::KeyAggContext::new([local, remote]).unwrap(),
            agg_nonce: musig2::AggNonce::sum([public_nonce]),
            content: fiber_types::Musig2SignableContent::CooperativeCloseTransaction(
                ckb_types::packed::Transaction::default(),
            ),
        };
        let json = to_rpc_musig2_signing_content(content.clone()).unwrap();
        let json_binding = json.v2.unwrap();
        assert_eq!(json_binding.channel_id, binding.channel_id.into());
        assert_eq!(json_binding.owner, local.into());
        assert_eq!(json_binding.public_nonce, binding.public_nonce.serialize());
        assert_eq!(json.slot.purpose, fiber_json_types::NoncePurpose::Closing);
        content.v2 = None;
        assert_eq!(
            to_rpc_musig2_signing_content(content).unwrap_err(),
            "missing V2 external nonce allocation"
        );
    }

    #[test]
    fn v2_followup_material_validates_public_nonce_encodings() {
        let local = Privkey::from(&[1; 32]).pubkey();
        let nonce = musig2::SecNonce::build([3; 32]).build().public_nonce();
        let mut material = fiber_json_types::NextChannelSignerMaterial {
            v2_nonces: vec![nonce.serialize().to_vec()],
            commitment_points: vec![(9, local.into())],
            ..Default::default()
        };
        let internal = try_into_next_channel_signer_material(material.clone()).unwrap();
        assert_eq!(internal.v2_nonces, vec![nonce]);
        assert_eq!(internal.commitment_points, vec![(9, local)]);
        material.v2_nonces.push(vec![0; 1]);
        assert!(try_into_next_channel_signer_material(material).is_err());
    }

    #[test]
    fn signing_snapshot_preserves_ids_keys_and_wallet_relative_direction() {
        let local = Privkey::from(&[1; 32]).pubkey();
        let remote = Privkey::from(&[2; 32]).pubkey();
        for for_remote in [false, true] {
            for inbound in [false, true] {
                let data = SettlementData {
                    local_amount: 100,
                    remote_amount: 200,
                    tlcs: vec![SettlementTlc {
                        tlc_id: if inbound == for_remote {
                            TLCId::Received(42)
                        } else {
                            TLCId::Offered(42)
                        },
                        hash_algorithm: Default::default(),
                        payment_amount: 10,
                        payment_hash: [3; 32].into(),
                        expiry: 10_000,
                        local_key: None,
                        local_key_pubkey: Some(local),
                        local_key_commitment_number: Some(7),
                        remote_key: remote,
                    }],
                };
                let json =
                    to_rpc_signing_settlement(data.clone(), local, remote, for_remote).unwrap();
                assert_eq!(json.tlcs[0].inbound, inbound);
                assert_eq!(json.tlcs[0].tlc_id, 42);
                assert_eq!(json.tlcs[0].local_key_commitment_number, 7);
                let (restored, _, _, _) = fiber_lsp_sdk::json::settlement_from_rpc(&json).unwrap();
                assert_eq!(restored, data);
                let mut missing_index = data;
                missing_index.tlcs[0].local_key_commitment_number = None;
                assert_eq!(
                    to_rpc_signing_settlement(missing_index, local, remote, for_remote)
                        .unwrap_err(),
                    "missing TLC key derivation index"
                );
            }
        }
    }

    #[test]
    fn try_into_rpc_script_and_tx_reject_invalid_hash_type_and_dep_type() {
        use ckb_types::{
            packed,
            prelude::{Builder, Entity, Pack},
        };

        let invalid_script = packed::Script::new_builder()
            .hash_type(packed::Byte::new(0xff))
            .build();
        assert!(super::validate_packed_script_hash_type(&invalid_script).is_err());
        assert!(super::try_into_rpc_script(invalid_script.clone()).is_err());

        let invalid_lock_tx = packed::Transaction::new_builder()
            .raw(
                packed::RawTransaction::new_builder()
                    .outputs(
                        packed::CellOutputVec::new_builder()
                            .push(
                                packed::CellOutput::new_builder()
                                    .lock(invalid_script.clone())
                                    .build(),
                            )
                            .build(),
                    )
                    .build(),
            )
            .build();
        assert!(super::try_into_rpc_transaction(invalid_lock_tx).is_err());

        let invalid_type_tx = packed::Transaction::new_builder()
            .raw(
                packed::RawTransaction::new_builder()
                    .outputs(
                        packed::CellOutputVec::new_builder()
                            .push(
                                packed::CellOutput::new_builder()
                                    .type_(Some(invalid_script).pack())
                                    .build(),
                            )
                            .build(),
                    )
                    .build(),
            )
            .build();
        assert!(super::try_into_rpc_transaction(invalid_type_tx).is_err());

        let invalid_dep_tx = packed::Transaction::new_builder()
            .raw(
                packed::RawTransaction::new_builder()
                    .cell_deps(
                        packed::CellDepVec::new_builder()
                            .push(
                                packed::CellDep::new_builder()
                                    .dep_type(packed::Byte::new(0xff))
                                    .build(),
                            )
                            .build(),
                    )
                    .build(),
            )
            .build();
        assert!(super::try_into_rpc_transaction(invalid_dep_tx.clone()).is_err());

        let local = Privkey::from(&[1; 32]).pubkey();
        let remote = Privkey::from(&[2; 32]).pubkey();
        let public_nonce = musig2::SecNonce::build([3; 32]).build().public_nonce();
        let content = fiber_types::Musig2SigningContent {
            v2: Some(fiber_types::V2NonceBinding {
                channel_id: [4; 32].into(),
                owner: local,
                public_nonce: public_nonce.clone(),
            }),
            slot: fiber_types::NonceSlot {
                purpose: fiber_types::NoncePurpose::Closing,
                commitment_number: 0,
            },
            commitment_counter: None,
            key_agg_ctx: musig2::KeyAggContext::new([local, remote]).expect("valid key agg"),
            agg_nonce: musig2::AggNonce::sum([public_nonce]),
            content: fiber_types::Musig2SignableContent::CooperativeCloseTransaction(
                invalid_dep_tx,
            ),
        };
        let err = to_rpc_musig2_signing_content(content).unwrap_err();
        assert!(err.contains("invalid cell_dep dep_type"), "{err}");
    }

    #[test]
    fn v2_signer_material_rpc_enforces_batch_bounds() {
        let pk = Privkey::from(&[1; 32]).pubkey();
        let nonce_bytes = musig2::SecNonce::build([2; 32])
            .build()
            .public_nonce()
            .serialize()
            .to_vec();

        let open_too_many_nonces = fiber_json_types::ChannelOpenSignerMaterial {
            v2_nonces: vec![
                nonce_bytes.clone();
                fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH + 1
            ],
            base_public_keys: fiber_json_types::ChannelBasePublicKeys {
                funding_pubkey: pk.into(),
                tlc_base_key: pk.into(),
            },
            first_commitment_point: pk.into(),
            second_commitment_point: pk.into(),
            commitment_nonce: nonce_bytes.clone(),
            next_commitment_nonce: nonce_bytes.clone(),
            revocation_nonce: nonce_bytes.clone(),
            channel_announcement_nonce: None,
        };
        let err = super::try_into_channel_open_signer_material(open_too_many_nonces).unwrap_err();
        assert!(err.contains("v2_nonces batch size"), "{err}");

        let next_too_many_nonces = fiber_json_types::NextChannelSignerMaterial {
            v2_nonces: vec![
                nonce_bytes;
                fiber_types::channel_v2_validation::MAX_V2_NONCE_BATCH + 1
            ],
            commitment_points: vec![],
            next_commitment_point: None,
            next_commitment_nonce: None,
            next_revocation_nonce: None,
        };
        let err = try_into_next_channel_signer_material(next_too_many_nonces).unwrap_err();
        assert!(err.contains("v2_nonces batch size"), "{err}");

        let next_too_many_points = fiber_json_types::NextChannelSignerMaterial {
            v2_nonces: vec![],
            commitment_points: (0
                ..=fiber_types::channel_v2_validation::MAX_V2_COMMITMENT_POINT_BATCH as u64)
                .map(|n| (n, pk.into()))
                .collect(),
            next_commitment_point: None,
            next_commitment_nonce: None,
            next_revocation_nonce: None,
        };
        let err = try_into_next_channel_signer_material(next_too_many_points).unwrap_err();
        assert!(err.contains("commitment_points batch size"), "{err}");
    }
}
