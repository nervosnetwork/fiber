use jsonrpsee::proc_macros::rpc;

#[cfg(feature = "watchtower")]
use jsonrpsee::types::ErrorObjectOwned;

#[cfg(feature = "watchtower")]
use crate::rpc::utils::{rpc_error, RpcResultExt};
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerStore;
#[cfg(feature = "watchtower")]
pub use fiber_json_types::RpcContext;
#[cfg(feature = "watchtower")]
use fiber_types::{NodeId, Pubkey, SettlementKey, WatchChannelKeys};

#[cfg(feature = "watchtower")]
fn validate_channel_features(bits: u8) -> Result<fiber_types::ChannelFeatures, ErrorObjectOwned> {
    fiber_types::ChannelFeatures::from_bits(bits).map_err(rpc_error)
}

pub use fiber_json_types::{
    CreatePreimageParams, CreateWatchChannelParams, GetWatchtowerSigningStatusParams,
    GetWatchtowerSigningStatusResult, RemovePreimageParams, RemoveWatchChannelParams,
    SubmitWatchtowerSignatureParams, SubmitWatchtowerSignatureResult, UpdateLocalSettlementParams,
    UpdatePendingRemoteSettlementParams, UpdateRevocationParams, WatchtowerSigningStatus,
};

/// RPC module for watchtower related operations
#[cfg(feature = "watchtower")]
#[rpc(server)]
trait WatchtowerRpc {
    /// Create a new watched channel.
    ///
    /// Supplying `local_settlement_key` leaves the settlement secret on the
    /// watchtower. Omit the private key and pass `local_settlement_key_pubkey`
    /// for an externally signed channel. Settlement then pauses until the
    /// owner submits a signature through `get_watchtower_signing_status` and
    /// `submit_watchtower_signature`.
    #[method(name = "create_watch_channel")]
    async fn create_watch_channel(
        &self,
        ctx: RpcContext,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Remove a watched channel
    #[method(name = "remove_watch_channel")]
    async fn remove_watch_channel(
        &self,
        ctx: RpcContext,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update revocation
    #[method(name = "update_revocation")]
    async fn update_revocation(
        &self,
        ctx: RpcContext,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update pending remote settlement
    #[method(name = "update_pending_remote_settlement")]
    async fn update_pending_remote_settlement(
        &self,
        ctx: RpcContext,
        params: UpdatePendingRemoteSettlementParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update settlement
    #[method(name = "update_local_settlement")]
    async fn update_local_settlement(
        &self,
        ctx: RpcContext,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Create preimage
    #[method(name = "create_preimage")]
    async fn create_preimage(
        &self,
        ctx: RpcContext,
        params: CreatePreimageParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Remove preimage
    #[method(name = "remove_preimage")]
    async fn remove_preimage(
        &self,
        ctx: RpcContext,
        params: RemovePreimageParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Read the current external watchtower signing status for a watched channel.
    #[method(name = "get_watchtower_signing_status")]
    async fn get_watchtower_signing_status(
        &self,
        ctx: RpcContext,
        params: GetWatchtowerSigningStatusParams,
    ) -> Result<GetWatchtowerSigningStatusResult, ErrorObjectOwned>;

    /// Submit an external watchtower settlement or TLC signature.
    #[method(name = "submit_watchtower_signature")]
    async fn submit_watchtower_signature(
        &self,
        ctx: RpcContext,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult, ErrorObjectOwned>;
}

/// ignore rpc-doc-gen
/// RPC client
#[rpc(client)]
trait WatchtowerRpc {
    /// Create a new watched channel
    #[method(name = "create_watch_channel")]
    async fn create_watch_channel(
        &self,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Remove a watched channel
    #[method(name = "remove_watch_channel")]
    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update revocation
    #[method(name = "update_revocation")]
    async fn update_revocation(
        &self,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update pending remote settlement
    #[method(name = "update_pending_remote_settlement")]
    async fn update_pending_remote_settlement(
        &self,
        params: UpdatePendingRemoteSettlementParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update settlement
    #[method(name = "update_local_settlement")]
    async fn update_local_settlement(
        &self,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Create preimage
    #[method(name = "create_preimage")]
    async fn create_preimage(&self, params: CreatePreimageParams) -> Result<(), ErrorObjectOwned>;

    /// Remove preimage
    #[method(name = "remove_preimage")]
    async fn remove_preimage(&self, params: RemovePreimageParams) -> Result<(), ErrorObjectOwned>;

    /// Read the current external watchtower signing status for a watched channel.
    #[method(name = "get_watchtower_signing_status")]
    async fn get_watchtower_signing_status(
        &self,
        params: GetWatchtowerSigningStatusParams,
    ) -> Result<GetWatchtowerSigningStatusResult, ErrorObjectOwned>;

    /// Submit an external watchtower settlement or TLC signature.
    #[method(name = "submit_watchtower_signature")]
    async fn submit_watchtower_signature(
        &self,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult, ErrorObjectOwned>;
}

#[cfg(feature = "watchtower")]
pub struct WatchtowerRpcServerImpl<S> {
    store: S,
    signing_events: crate::signing::SigningEvents,
    quotas: crate::lsp_config::WatchtowerQuotas,
    lsp_enabled: bool,
    #[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
    lsp_actor: Option<ractor::ActorRef<crate::lsp::LspServiceMessage>>,
}

#[cfg(feature = "watchtower")]
impl<S> WatchtowerRpcServerImpl<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            signing_events: Default::default(),
            quotas: Default::default(),
            lsp_enabled: true,
            #[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
            lsp_actor: None,
        }
    }

    pub(crate) fn with_signing_events(mut self, events: crate::signing::SigningEvents) -> Self {
        self.signing_events = events;
        self
    }

    pub(crate) fn with_quotas(mut self, quotas: crate::lsp_config::WatchtowerQuotas) -> Self {
        self.quotas = quotas;
        self
    }

    pub(crate) fn with_lsp_enabled(mut self, enabled: bool) -> Self {
        self.lsp_enabled = enabled;
        self
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
    pub(crate) fn with_lsp_actor(
        mut self,
        lsp_actor: Option<ractor::ActorRef<crate::lsp::LspServiceMessage>>,
    ) -> Self {
        self.lsp_actor = lsp_actor;
        self
    }

    fn refresh_lsp_tenant_pending_quota(&self, node_id: &NodeId) {
        #[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
        if *node_id != NodeId::local() {
            if let Some(lsp_actor) = &self.lsp_actor {
                let _ = lsp_actor.send_message(
                    crate::lsp::LspServiceMessage::RefreshTenantWatchtowerChannelQuotaByNodeId(
                        node_id.clone(),
                    ),
                );
            }
        }
        #[cfg(not(all(not(target_arch = "wasm32"), feature = "lsp")))]
        let _ = node_id;
    }

    fn authorized_node_id(&self, ctx: &RpcContext) -> Result<NodeId, ErrorObjectOwned> {
        if ctx.tenant_scoped && !self.lsp_enabled {
            return Err(rpc_error("hosted LSP service is not enabled"));
        }
        authorized_watchtower_node_id(ctx)
    }
}

#[cfg(feature = "watchtower")]
fn authorized_watchtower_node_id(ctx: &RpcContext) -> Result<NodeId, ErrorObjectOwned> {
    let node_id = ctx.node_id.parse::<NodeId>().rpc_err()?;
    if ctx.tenant_scoped && node_id == NodeId::local() {
        return Err(rpc_error(
            "tenant token cannot access the host watchtower namespace",
        ));
    }
    Ok(node_id)
}

#[cfg(feature = "watchtower")]
const MAX_WATCHTOWER_SCRIPT_ARGS_BYTES: usize = 1024;
#[cfg(feature = "watchtower")]
const MAX_WATCHTOWER_OUTPUT_DATA_BYTES: usize = 1024;

#[cfg(feature = "watchtower")]
fn validate_script_size(
    script: &ckb_types::packed::Script,
    field: &str,
) -> Result<(), ErrorObjectOwned> {
    if script.args().raw_data().len() > MAX_WATCHTOWER_SCRIPT_ARGS_BYTES {
        return Err(rpc_error(format!(
            "{field} args exceeds maximum size ({MAX_WATCHTOWER_SCRIPT_ARGS_BYTES} bytes)"
        )));
    }
    Ok(())
}

#[cfg(feature = "watchtower")]
fn validate_update_signing_keys<S: WatchtowerStore>(
    store: &S,
    node_id: &NodeId,
    channel_id: &fiber_types::Hash256,
    settlement: &fiber_types::SettlementData,
) -> Result<(), ErrorObjectOwned> {
    if settlement.tlcs.len() > fiber_types::MAX_WITNESS_TLC_COUNT {
        return Err(rpc_error(
            "TLC count exceeds witness encoding limit (max 254)",
        ));
    }
    if let Some(channel) = store.get_watch_channel(node_id, channel_id) {
        settlement
            .validate_signing_keys(channel.local_settlement_key.is_none())
            .map_err(rpc_error)?;
    }
    Ok(())
}

#[cfg(feature = "watchtower")]
#[async_trait::async_trait]
impl<S> WatchtowerRpcServer for WatchtowerRpcServerImpl<S>
where
    S: WatchtowerStore + Send + Sync + 'static,
{
    async fn create_watch_channel(
        &self,
        ctx: RpcContext,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = self.authorized_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let local_settlement_key = params
            .local_settlement_key
            .map(TryInto::try_into)
            .transpose()
            .map_err(rpc_error)?;
        if ctx.tenant_scoped && local_settlement_key.is_some() {
            return Err(rpc_error(
                "tenant token cannot register a local settlement key",
            ));
        }
        let supplied_local_settlement_pubkey = params
            .local_settlement_key_pubkey
            .map(Pubkey::try_from)
            .transpose()
            .rpc_err()?;
        let local_settlement =
            SettlementKey::from_parts(local_settlement_key, supplied_local_settlement_pubkey)
                .map_err(|_| {
                    rpc_error("local_settlement_key_pubkey does not match local_settlement_key")
                })?
                .ok_or_else(|| {
                    rpc_error(
                        "local_settlement_key_pubkey is required when \
                         local_settlement_key is omitted",
                    )
                })?;
        let remote_settlement_key = Pubkey::try_from(params.remote_settlement_key).rpc_err()?;
        let local_funding_pubkey = Pubkey::try_from(params.local_funding_pubkey).rpc_err()?;
        let remote_funding_pubkey = Pubkey::try_from(params.remote_funding_pubkey).rpc_err()?;
        validate_channel_features(params.channel_features.0)?;
        // Move fields out of params last, after all borrows of params are done.
        let funding_udt_type_script: Option<ckb_types::packed::Script> =
            params.funding_udt_type_script.map(Into::into);
        if let Some(ref script) = funding_udt_type_script {
            validate_script_size(script, "funding_udt_type_script")?;
        }
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        settlement_data
            .validate_signing_keys(local_settlement.is_external())
            .map_err(rpc_error)?;
        let signing_scope = self.signing_events.watchtower(&node_id);
        let signing_update = signing_scope.prepare_created(channel_id);
        let keys = WatchChannelKeys {
            local_settlement,
            remote_settlement: remote_settlement_key,
            local_funding: local_funding_pubkey,
            remote_funding: remote_funding_pubkey,
        };
        let created = if ctx.tenant_scoped {
            self.store
                .insert_watch_channel_checked(
                    node_id.clone(),
                    channel_id,
                    funding_udt_type_script,
                    keys,
                    settlement_data,
                    fiber_types::ChannelFeatures::V2,
                    &self.quotas,
                )
                .map_err(rpc_error)?
        } else {
            self.store.insert_watch_channel(
                node_id.clone(),
                channel_id,
                funding_udt_type_script,
                keys,
                settlement_data,
                fiber_types::ChannelFeatures::V2,
            )
        };
        if created {
            signing_update.publish();
            self.refresh_lsp_tenant_pending_quota(&node_id);
        }
        Ok(())
    }

    async fn remove_watch_channel(
        &self,
        ctx: RpcContext,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = self.authorized_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let signing_scope = self.signing_events.watchtower(&node_id);
        let signing_update = signing_scope.prepare_removed(channel_id);
        if self.store.remove_watch_channel(node_id.clone(), channel_id) {
            signing_update.publish();
            self.refresh_lsp_tenant_pending_quota(&node_id);
        }
        Ok(())
    }

    async fn update_revocation(
        &self,
        ctx: RpcContext,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = self.authorized_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let revocation_data: fiber_types::RevocationData = params
            .revocation_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        if revocation_data.output_data.raw_data().len() > MAX_WATCHTOWER_OUTPUT_DATA_BYTES {
            return Err(rpc_error(format!(
                "revocation_data.output_data exceeds maximum size ({MAX_WATCHTOWER_OUTPUT_DATA_BYTES} bytes)"
            )));
        }
        validate_script_size(
            &revocation_data.output.lock(),
            "revocation_data.output.lock",
        )?;
        if let Some(type_script) = revocation_data.output.type_().to_opt() {
            validate_script_size(&type_script, "revocation_data.output.type")?;
        }
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        validate_update_signing_keys(&self.store, &node_id, &channel_id, &settlement_data)?;
        self.store
            .update_revocation(node_id, channel_id, revocation_data, settlement_data);
        Ok(())
    }

    async fn update_pending_remote_settlement(
        &self,
        ctx: RpcContext,
        params: UpdatePendingRemoteSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = self.authorized_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        validate_update_signing_keys(&self.store, &node_id, &channel_id, &settlement_data)?;
        self.store
            .update_pending_remote_settlement(
                node_id,
                channel_id,
                params.commitment_number,
                settlement_data,
            )
            .map_err(rpc_error)
    }

    async fn update_local_settlement(
        &self,
        ctx: RpcContext,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = self.authorized_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        validate_update_signing_keys(&self.store, &node_id, &channel_id, &settlement_data)?;
        self.store
            .update_local_settlement(node_id, channel_id, settlement_data);
        Ok(())
    }

    async fn create_preimage(
        &self,
        ctx: RpcContext,
        params: CreatePreimageParams,
    ) -> Result<(), ErrorObjectOwned> {
        use fiber_types::HashAlgorithm;

        let node_id = self.authorized_node_id(&ctx)?;
        let payment_hash = params.payment_hash.into();
        let preimage = params.preimage.into();

        if HashAlgorithm::supported_algorithms()
            .iter()
            .all(|algorithm| payment_hash != algorithm.hash(preimage).into())
        {
            return Err(rpc_error("Wrong preimage"));
        }
        if ctx.tenant_scoped {
            self.store
                .insert_watch_preimage_checked(node_id, payment_hash, preimage, &self.quotas)
                .map_err(rpc_error)?;
        } else {
            self.store
                .insert_watch_preimage(node_id, payment_hash, preimage);
        }
        Ok(())
    }
    async fn remove_preimage(
        &self,
        ctx: RpcContext,
        params: RemovePreimageParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = self.authorized_node_id(&ctx)?;
        let payment_hash = params.payment_hash.into();
        self.store.remove_watch_preimage(node_id, payment_hash);
        Ok(())
    }

    async fn get_watchtower_signing_status(
        &self,
        ctx: RpcContext,
        params: GetWatchtowerSigningStatusParams,
    ) -> Result<GetWatchtowerSigningStatusResult, ErrorObjectOwned> {
        let node_id = self.authorized_node_id(&ctx)?;
        let channel_id: fiber_types::Hash256 = params.channel_id.into();
        if self
            .store
            .get_watch_channel(&node_id, &channel_id)
            .is_none()
        {
            return Err(rpc_error("watched channel not found"));
        }
        let (_, _, node_retry_after_ms) = self.store.watchtower_external_signing_stats(&node_id);
        let status = to_rpc_watchtower_signing_status_with_backoff(
            self.store.get_watchtower_signer(&node_id, &channel_id),
            node_retry_after_ms,
            crate::now_timestamp_as_millis_u64(),
        );
        Ok(GetWatchtowerSigningStatusResult {
            channel_id: channel_id.into(),
            status,
        })
    }

    async fn submit_watchtower_signature(
        &self,
        ctx: RpcContext,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult, ErrorObjectOwned> {
        use fiber_types::{LastAppliedWatchtowerSignature, WatchtowerSignerState};

        let node_id = self.authorized_node_id(&ctx)?;
        let channel_id: fiber_types::Hash256 = params.channel_id.into();
        let request_id: fiber_types::Hash256 = params.request_id.into();
        let signature: [u8; 65] = params
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| rpc_error("watchtower signature must be 65 bytes"))?;
        let submitted_tx: ckb_types::packed::Transaction = params.signed_transaction.into();
        fiber_types::validate_external_funding_watchtower_tx_bounds(&submitted_tx)
            .map_err(rpc_error)?;
        let submitted_tx_hash = fiber_types::external_funding_watchtower_tx_hash(&submitted_tx);
        let signing_scope = self.signing_events.watchtower(&node_id);
        let mut signing_update = signing_scope.prepare_update();
        let (result, updated_state) =
            self.store
                .update_watchtower_signer(&node_id, &channel_id, |state| {
                    let Some(channel) = self.store.get_watch_channel(&node_id, &channel_id) else {
                        return Err(rpc_error("watched channel not found"));
                    };
                    let WatchtowerSignerState::External(external) = state else {
                        return Err(rpc_error("watched channel does not use an external signer"));
                    };
                    if let Some(existing) = external.signed_signatures.get(&request_id) {
                        if existing.signature == signature
                            && submitted_tx_hash
                                == fiber_types::external_funding_watchtower_tx_hash(
                                    &existing.content.transaction,
                                )
                            && fiber_types::validate_external_funding_retry_witness_zero(
                                &submitted_tx,
                                &signature,
                            )
                            .is_ok()
                        {
                            return Ok((SubmitWatchtowerSignatureResult::AlreadyApplied, None));
                        }
                        return Err(rpc_error(
                            "submitted signature does not match the previously applied result",
                        ));
                    }
                    if let Some(applied) = external
                        .last_applied
                        .as_ref()
                        .filter(|applied| applied.request_id == request_id)
                    {
                        if applied.signature == signature
                            && applied.signed_transaction_hash == submitted_tx_hash
                            && fiber_types::validate_external_funding_retry_witness_zero(
                                &submitted_tx,
                                &signature,
                            )
                            .is_ok()
                        {
                            return Ok((SubmitWatchtowerSignatureResult::AlreadyApplied, None));
                        }
                        return Err(rpc_error(
                            "submitted signature does not match the previously applied result",
                        ));
                    }
                    let Some(pending) = external.pending_requests.get(&request_id) else {
                        return Err(rpc_error(
                            "signature request id not found in pending requests",
                        ));
                    };
                    let now_ms = crate::now_timestamp_as_millis_u64();
                    if pending.created_at_ms > 0
                        && now_ms
                            >= pending
                                .created_at_ms
                                .saturating_add(crate::ckb::held_cells::WATCHTOWER_HOLD_MS)
                    {
                        return Err(rpc_error("watchtower signature request has expired"));
                    }
                    let normalized =
                        fiber_types::validate_and_normalize_external_funding_watchtower_tx(
                            &pending.content.transaction,
                            &submitted_tx,
                            pending.required_expiry_since,
                            Some(&signature),
                        )
                        .map_err(rpc_error)?;
                    let signed_tx_hash =
                        fiber_types::external_funding_watchtower_tx_hash(&normalized);
                    let signed_entry = fiber_types::SignedWatchtowerRequest {
                        content: fiber_types::OnchainSigningContent {
                            key_purpose: pending.content.key_purpose,
                            transaction: normalized,
                        },
                        signature,
                        created_at_ms: now_ms,
                    };
                    let superseded = self
                        .store
                        .get_superseded_remote_settlements(&node_id, &channel_id);
                    let expected_pubkey = channel
                        .expected_onchain_pubkey_with_settlements(
                            &signed_entry.content.key_purpose,
                            &superseded,
                        )
                        .map_err(rpc_error)?;
                    crate::watchtower::WatchtowerSigner::apply_submitted(
                        &expected_pubkey,
                        &signed_entry.content,
                        signature,
                    )
                    .map_err(rpc_error)?;
                    external
                        .pending_requests
                        .remove(&request_id)
                        .expect("pending request checked above");
                    external.last_applied = Some(LastAppliedWatchtowerSignature {
                        request_id,
                        signature,
                        signed_transaction_hash: signed_tx_hash,
                    });
                    external.signed_signatures.insert(request_id, signed_entry);
                    Ok((
                        SubmitWatchtowerSignatureResult::Applied,
                        Some(state.clone()),
                    ))
                })?;
        if let Some(updated_state) = updated_state {
            if signing_update.is_observed() {
                let (_, _, node_retry_after_ms) =
                    self.store.watchtower_external_signing_stats(&node_id);
                signing_update.watchtower_state_with_backoff(
                    channel_id,
                    &updated_state,
                    node_retry_after_ms,
                );
            }
        }
        signing_update.publish();
        Ok(result)
    }
}

#[cfg(all(test, feature = "watchtower"))]
pub(crate) fn to_rpc_watchtower_signing_status(
    state: fiber_types::WatchtowerSignerState,
) -> WatchtowerSigningStatus {
    to_rpc_watchtower_signing_status_with_backoff(state, 0, crate::now_timestamp_as_millis_u64())
}

#[cfg(feature = "watchtower")]
pub(crate) fn to_rpc_watchtower_signing_status_with_backoff(
    state: fiber_types::WatchtowerSignerState,
    node_retry_after_ms: u64,
    now_ms: u64,
) -> WatchtowerSigningStatus {
    use fiber_types::{OnchainKeyPurpose, WatchtowerSignerState};

    match state {
        WatchtowerSignerState::Internal => WatchtowerSigningStatus::NoSignatureRequired,
        WatchtowerSignerState::External(external) => {
            if let Some((request_id, pending)) = external.first_pending() {
                let expires_at_ms = if pending.created_at_ms > 0 {
                    pending
                        .created_at_ms
                        .saturating_add(crate::ckb::held_cells::WATCHTOWER_HOLD_MS)
                } else {
                    0
                };
                WatchtowerSigningStatus::ExternalFundingRequired {
                    request_id: request_id.into(),
                    content: fiber_json_types::OnchainSigningContent {
                        key_purpose: match pending.content.key_purpose {
                            OnchainKeyPurpose::Settlement => {
                                fiber_json_types::OnchainKeyPurpose::Settlement
                            }
                            OnchainKeyPurpose::Tlc { commitment_number } => {
                                fiber_json_types::OnchainKeyPurpose::Tlc { commitment_number }
                            }
                        },
                        transaction: pending.content.transaction.clone().into(),
                    },
                    expires_at_ms,
                    required_expiry_since: pending.required_expiry_since,
                }
            } else {
                let effective_retry_after_ms = external.retry_after_ms.max(node_retry_after_ms);
                if effective_retry_after_ms > now_ms {
                    WatchtowerSigningStatus::Backoff {
                        retry_after_ms: effective_retry_after_ms,
                        consecutive_expired_requests: external.consecutive_expired_requests,
                    }
                } else {
                    WatchtowerSigningStatus::NoSignatureRequired
                }
            }
        }
    }
}

#[cfg(all(test, feature = "watchtower"))]
mod tests;
