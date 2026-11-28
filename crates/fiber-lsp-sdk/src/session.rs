//! Hosted LSP session: registration, channel directory, and one-shot status handling.
//!
//! Opening uses caller-provided transport, wallet verification and persistence.
//! For signing, the caller fetches RPC results and submits the returned parameters.

use std::collections::HashMap;

use fiber_json_types::{
    ChannelSigningStatus, CreateWatchChannelParams, GetWatchtowerSigningStatusResult,
    RegisterLspTenantParams, RegisterLspTenantResult, SubmitChannelSignatureParams,
    SubmitWatchtowerSignatureParams, WatchtowerSigningStatus,
};
use fiber_types::{Hash256, TenantId, TenantRegistryPayload};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    evaluate_watchtower_status, handle_watchtower_rpc_error,
    json::{musig2_from_rpc, next_material_to_rpc, onchain_from_rpc, settlement_from_rpc},
    ChannelKeyId, ChannelOpenSignerMaterial, ChannelSignature, ChannelSigningContent,
    OwnedSettlementBinding, PaymentRegistry, PreparedSigning, RootSigner, SignerError, SignerStore,
    SigningDecision, SigningPolicy, WatchtowerEventHandler, WatchtowerHealthStatus,
    WatchtowerLimitPolicy, WatchtowerQuotaAction,
};

#[cfg(test)]
#[path = "token_tests.rs"]
mod token_tests;

/// Persistable hosted-session map. The caller owns file or IndexedDB I/O.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostedSessionState {
    /// Tenant Biscuit after a successful registration.
    pub tenant_token: Option<String>,
    /// Exclusive expiry of `tenant_token`, in Unix milliseconds.
    #[serde(default)]
    pub token_expires_at_ms: Option<u64>,
    /// Fiber `channel_id` → local signer key id.
    pub bindings: HashMap<Hash256, ChannelKeyId>,
    /// At most one unallocated channel key waiting for `open_tenant_channel`.
    pub pending: Option<ChannelKeyId>,
    /// Original opening request, retained after completion to validate retries.
    pub opening_request: Option<Vec<u8>>,
}

/// Errors from [`HostedSession`] orchestration.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SessionError {
    /// Underlying signer failure.
    #[error(transparent)]
    Signer(#[from] SignerError),
    /// RPC payload or registration proof failed validation.
    #[error("{0}")]
    Invalid(String),
    /// [`HostedSession::finish_registration`] has not stored a tenant token.
    #[error("hosted session is not registered")]
    NotRegistered,
    /// [`HostedSession::allocate_pending_channel`] has not created a key bundle.
    #[error("hosted session has no pending channel signer")]
    NoPendingChannel,
    /// `channel_id` is not in [`HostedSessionState::bindings`].
    #[error("channel is not bound in this session")]
    ChannelNotInDirectory,
}

/// A prepared request the caller can review, confirm, or submit.
#[derive(Clone, Debug)]
pub struct PendingRequest {
    /// Fiber channel this request belongs to.
    pub channel_id: Hash256,
    /// Node-issued request id echoed in the submit call.
    pub request_id: Hash256,
    /// Independently hashed plaintext.
    pub prepared: PreparedSigning,
    /// Settlement snapshot when this request assigns balances.
    pub settlement: Option<OwnedSettlementBinding>,
    kind: RequestKind,
}

impl PendingRequest {
    /// Whether this pending request is an externally funded watchtower settlement request
    /// confirmed via [`HostedSession::confirm_external_funding_onchain`].
    pub fn is_external_funding(&self) -> bool {
        matches!(self.kind, RequestKind::WatchtowerExternalFunding { .. })
    }

    /// Optional absolute timestamp `since` required on a funding input for an externally funded
    /// timeout TLC unlock.
    pub fn required_expiry_since(&self) -> Option<u64> {
        match self.kind {
            RequestKind::WatchtowerExternalFunding {
                required_expiry_since,
                ..
            } => required_expiry_since,
            RequestKind::Channel => None,
        }
    }

    /// Exclusive request expiry in Unix milliseconds when carried by an externally funded
    /// watchtower request (`None` if unset/0 or not an external-funding watchtower request).
    pub fn expires_at_ms(&self) -> Option<u64> {
        match self.kind {
            RequestKind::WatchtowerExternalFunding { expires_at_ms, .. } => {
                (expires_at_ms != 0).then_some(expires_at_ms)
            }
            RequestKind::Channel => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
    Channel,
    WatchtowerExternalFunding {
        required_expiry_since: Option<u64>,
        expires_at_ms: u64,
    },
}

/// What the caller should do with a handled status.
#[derive(Clone, Debug)]
pub enum ProcessOutcome {
    /// No signature is required.
    Idle,
    /// Policy allowed signing; the caller should POST these params.
    ReadyToSubmit(SubmitParams),
    /// Policy requires an explicit [`HostedSession::confirm`].
    NeedConfirmation(PendingRequest),
    /// Policy refused this request.
    Denied,
}

/// Submit payload for either channel or watchtower RPC.
#[derive(Clone, Debug)]
pub enum SubmitParams {
    /// `submit_channel_signature` body.
    Channel(SubmitChannelSignatureParams),
    /// `submit_watchtower_signature` body.
    Watchtower(SubmitWatchtowerSignatureParams),
}

/// Hosted tenant session with caller-provided I/O adapters.
pub struct HostedSession<S> {
    root: RootSigner<S>,
    state: HostedSessionState,
    policy: SigningPolicy,
    watchtower_limit_policy: WatchtowerLimitPolicy,
    registry: PaymentRegistry,
}

impl<S: SignerStore> HostedSession<S> {
    /// Create a session with [`SigningPolicy::Auto`] and [`WatchtowerLimitPolicy::FailClosed`].
    pub fn new(root: RootSigner<S>) -> Self {
        Self {
            root,
            state: HostedSessionState::default(),
            policy: SigningPolicy::Auto,
            watchtower_limit_policy: WatchtowerLimitPolicy::FailClosed,
            registry: PaymentRegistry::default(),
        }
    }

    /// Replace the signing policy. Production builds only have `Auto` and `Manual`.
    pub fn with_policy(mut self, policy: SigningPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replace the watchtower quota limit policy (defaults to [`WatchtowerLimitPolicy::FailClosed`]).
    pub fn with_watchtower_limit_policy(mut self, policy: WatchtowerLimitPolicy) -> Self {
        self.watchtower_limit_policy = policy;
        self
    }

    /// Active watchtower quota limit policy.
    pub fn watchtower_limit_policy(&self) -> WatchtowerLimitPolicy {
        self.watchtower_limit_policy
    }

    /// Restore a previously serialized session map.
    pub fn with_state(mut self, state: HostedSessionState) -> Self {
        self.state = state;
        self
    }

    /// Tenant id derived from the RootSigner identity.
    pub fn tenant_id(&self) -> TenantId {
        TenantId::from_root_signer_pubkey(&self.root.identity_public_key().into())
    }

    /// RootSigner identity public key.
    pub fn identity_public_key(&self) -> secp256k1::PublicKey {
        self.root.identity_public_key()
    }

    /// Borrow the persistable session map.
    pub fn state(&self) -> &HostedSessionState {
        &self.state
    }

    /// Invoices and outbound payments this client created.
    pub fn registry(&self) -> &PaymentRegistry {
        &self.registry
    }

    /// Mutate the payment registry (record invoices / outbound pays).
    pub fn registry_mut(&mut self) -> &mut PaymentRegistry {
        &mut self.registry
    }

    /// Tenant token after [`Self::finish_registration`].
    pub fn tenant_token(&self) -> Option<&str> {
        self.state.tenant_token.as_deref()
    }

    /// Pending unallocated channel key, if any.
    pub fn pending_channel_key_id(&self) -> Option<ChannelKeyId> {
        self.state.pending
    }

    /// Local key id bound to a Fiber channel.
    pub fn binding(&self, channel_id: Hash256) -> Option<ChannelKeyId> {
        self.state.bindings.get(&channel_id).copied()
    }

    /// Reopen a channel signer owned by this session.
    pub async fn open_channel(
        &self,
        key_id: ChannelKeyId,
    ) -> Result<crate::ChannelSigner<S>, SessionError> {
        Ok(self.root.open_channel(key_id).await?)
    }

    /// Build a registration or credential refresh request for a client-generated request.
    /// The application sends this proof to its hosting provider, which calls
    /// `lsp_register_tenant` with operator credentials for registration or renewal.
    pub fn begin_registration(
        &self,
        trusted_lsp_node_id: fiber_types::Pubkey,
        request_id: Hash256,
        signed_at: u64,
    ) -> Result<RegisterLspTenantParams, SessionError> {
        let root_signer_pubkey: fiber_types::Pubkey = self.root.identity_public_key().into();
        let payload = TenantRegistryPayload::new(
            trusted_lsp_node_id,
            root_signer_pubkey,
            request_id.into(),
            signed_at,
        );
        let signature = self.root.sign_tenant_registry_payload(&payload)?;
        Ok(RegisterLspTenantParams {
            root_signer_pubkey: root_signer_pubkey.into(),
            request_id: request_id.into(),
            signed_at,
            signature: hex::encode(signature.serialize()),
        })
    }

    /// Store the tenant token after the caller posted [`Self::begin_registration`].
    pub fn finish_registration(
        &mut self,
        result: RegisterLspTenantResult,
    ) -> Result<(), SessionError> {
        if result.tenant.tenant_id != self.tenant_id().as_str() {
            return Err(SessionError::Invalid(
                "LSP returned an unexpected tenant id".to_string(),
            ));
        }
        self.replace_token(result.access_token, result.expires_at_ms)
    }

    /// Replace credentials received from the hosting provider, preserving all signer
    /// and channel state. Invalid input leaves the previous credentials intact.
    /// The caller persists `state()` using its existing storage adapter and updates
    /// credential-bound transports after a successful replacement.
    pub fn replace_token(&mut self, token: String, expires_at_ms: u64) -> Result<(), SessionError> {
        if token.is_empty() || expires_at_ms == 0 {
            return Err(SessionError::Invalid(
                "invalid tenant token or expiry".to_string(),
            ));
        }
        self.state.tenant_token = Some(token);
        self.state.token_expires_at_ms = Some(expires_at_ms);
        Ok(())
    }

    /// Check whether credentials expire within the caller's advance window, in milliseconds.
    /// Expired, missing or incomplete credentials also return true. The caller supplies
    /// Unix milliseconds; this method performs no I/O and does not request a new token.
    pub fn is_token_near_expiry(&self, now_ms: u64, advance_ms: u64) -> bool {
        match (
            self.state.tenant_token.as_deref(),
            self.state.token_expires_at_ms,
        ) {
            (Some(token), Some(expires)) if !token.is_empty() => {
                expires <= now_ms.saturating_add(advance_ms)
            }
            _ => true,
        }
    }

    /// Allocate or reopen the single pending channel key bundle.
    pub async fn allocate_pending_channel(
        &mut self,
    ) -> Result<ChannelOpenSignerMaterial, SessionError> {
        let key_id = match self.state.pending {
            Some(key_id) => key_id,
            None => {
                let channel = self.root.create_channel().await?;
                let key_id = channel.channel_key_id();
                self.state.pending = Some(key_id);
                key_id
            }
        };
        let signer = self.root.open_channel(key_id).await?;
        Ok(signer.channel_open_material(false).await?)
    }

    /// Abandon an unbound pending channel opening and retire its pending signer.
    ///
    /// Call this only after the LSP has definitively rejected an opening attempt
    /// and the caller needs to retry with modified parameters. Dropping the pending
    /// key ensures the next [`Self::allocate_pending_channel`] call creates a fresh
    /// channel signer instead of reusing the retired signer's keys or nonces.
    /// Already-bound channels in [`HostedSessionState::bindings`] are untouched,
    /// and a signer that already recorded an opening binding cannot be abandoned.
    /// The caller persists `state()` using its existing storage adapter after this
    /// call succeeds.
    pub async fn abandon_pending_channel_opening(&mut self) -> Result<ChannelKeyId, SessionError> {
        let key_id = self.state.pending.ok_or(SessionError::NoPendingChannel)?;
        if self.state.bindings.values().any(|bound| *bound == key_id) {
            return Err(SessionError::Invalid(
                "cannot abandon a channel signer that is already bound".into(),
            ));
        }
        let signer = self.root.open_channel(key_id).await?;
        if signer.load_binding().await?.is_some() {
            return Err(SessionError::Invalid(
                "cannot abandon a pending signer that already has a recorded channel binding"
                    .into(),
            ));
        }
        let pending_tlc_base_key: fiber_json_types::Pubkey = signer
            .public_material()
            .base_public_keys
            .tlc_base_key
            .into();
        let matches_pending_intent = self
            .state
            .opening_request
            .as_deref()
            .and_then(|bytes| {
                serde_json::from_slice::<fiber_json_types::OpenChannelWithExternalFundingParams>(
                    bytes,
                )
                .ok()
            })
            .and_then(|request| request.external_channel_signer)
            .is_some_and(|material| material.base_public_keys.tlc_base_key == pending_tlc_base_key);
        self.state.pending = None;
        if matches_pending_intent {
            self.state.opening_request = None;
        }
        Ok(key_id)
    }

    /// Open a tenant channel and return only after wallet/context verification and persistence.
    /// The transport call is the generic `open_channel_with_external_funding` RPC, and the
    /// response must include `opening_context`. The original intent is persisted before the
    /// RPC. Retry the identical request after interruption; the existing pending signer and
    /// intent are reused. The returned transaction is an unsigned proposal, not permission
    /// to release wallet signatures: first drive both initial channel-signing lanes and verify
    /// a complete local recovery commitment.
    pub async fn open_tenant_channel<R, V, P>(
        &mut self,
        request: fiber_json_types::OpenChannelWithExternalFundingParams,
        network: &crate::OpeningNetwork,
        rpc: &R,
        wallet: &V,
        persistence: &P,
    ) -> Result<fiber_json_types::OpenChannelWithExternalFundingResult, SessionError>
    where
        R: crate::TenantOpeningRpc,
        V: crate::FundingVerifier,
        P: crate::OpeningPersistence,
    {
        let token = self
            .state
            .tenant_token
            .clone()
            .ok_or(SessionError::NotRegistered)?;
        let request = self.begin_channel_opening(request).await?;
        persistence.save_session(&self.state).await?;
        let result = rpc
            .open_channel_with_external_funding(&token, request.clone())
            .await?;
        let inputs = wallet.verify_funding(&request, &result).await?;
        self.finish_channel_opening(&result, network, &inputs)
            .await?;
        persistence.save_session(&self.state).await?;
        Ok(result)
    }

    /// Persist the original client request before sending
    /// `open_channel_with_external_funding`. The caller must persist `state()`
    /// before performing network I/O.
    pub(crate) async fn begin_channel_opening(
        &mut self,
        request: fiber_json_types::OpenChannelWithExternalFundingParams,
    ) -> Result<fiber_json_types::OpenChannelWithExternalFundingParams, SessionError> {
        if request.public != Some(false) {
            return Err(SessionError::Invalid(
                "tenant opening requires public: false".into(),
            ));
        }
        let encoded =
            serde_json::to_vec(&request).map_err(|e| SessionError::Invalid(e.to_string()))?;
        // Repeating the last opening intent is a retry and must not allocate a
        // second signer key. Once that opening has completed, a different
        // intent is a new channel for this tenant.
        if self.state.opening_request.as_ref() == Some(&encoded) {
            return Ok(request);
        }
        if let Some(previous) = self.state.opening_request.as_deref() {
            let previous: fiber_json_types::OpenChannelWithExternalFundingParams =
                serde_json::from_slice(previous)
                    .map_err(|e| SessionError::Invalid(e.to_string()))?;
            // The retained intent can belong to a completed channel. A fresh
            // signer may start another opening, but a signer whose intent was
            // already sent must not replace it, even after an RPC timeout.
            if previous
                .external_channel_signer
                .as_ref()
                .zip(request.external_channel_signer.as_ref())
                .is_some_and(|(previous, current)| {
                    previous.base_public_keys.tlc_base_key == current.base_public_keys.tlc_base_key
                })
            {
                return Err(SessionError::Invalid(
                    "cannot replace opening intent for the same signer".into(),
                ));
            }
        }
        let (material, allocated_new) = if let Some(key_id) = self.state.pending {
            let signer = self.root.open_channel(key_id).await?;
            (signer.channel_open_material(false).await?, false)
        } else {
            (self.allocate_pending_channel().await?, true)
        };
        let expected = crate::json::open_material_to_rpc(&material);
        if serde_json::to_value(Some(expected)).map_err(|e| SessionError::Invalid(e.to_string()))?
            != serde_json::to_value(&request.external_channel_signer)
                .map_err(|e| SessionError::Invalid(e.to_string()))?
        {
            // We allocated a key speculatively for this request. If the caller
            // supplied material for a different key, discard it so the previous
            // completed opening can still be retried.
            if allocated_new {
                self.state.pending = None;
            }
            return Err(SessionError::Invalid(
                "opening material is not owned by the pending signer".into(),
            ));
        }
        if request.commitment_delay_epoch.is_none() || request.commitment_fee_rate.is_none() {
            return Err(SessionError::Invalid(
                "choose commitment delay and fee rate before opening".into(),
            ));
        }
        self.state.opening_request = Some(encoded);
        Ok(request)
    }

    /// Validate a frozen proposal against local intent, then persist its baseline and binding.
    /// The funding wallet must independently approve the complete transaction, its net
    /// debit and fee, and its selected inputs. Network constants come from trusted config.
    pub(crate) async fn finish_channel_opening(
        &mut self,
        result: &fiber_json_types::OpenChannelWithExternalFundingResult,
        network: &crate::OpeningNetwork,
        expected_inputs: &[ckb_types::packed::OutPoint],
    ) -> Result<(), SessionError> {
        let request: fiber_json_types::OpenChannelWithExternalFundingParams =
            serde_json::from_slice(
                self.state
                    .opening_request
                    .as_deref()
                    .ok_or(SessionError::NoPendingChannel)?,
            )
            .map_err(|e| SessionError::Invalid(e.to_string()))?;
        let channel_id = result.channel_id.into();
        let key = self
            .state
            .bindings
            .get(&channel_id)
            .copied()
            .or(self.state.pending)
            .ok_or(SessionError::NoPendingChannel)?;
        let signer = self.root.open_channel(key).await?;
        signer
            .verify_and_record_opening(&request, result, network, expected_inputs)
            .await?;
        self.state.bindings.insert(channel_id, key);
        if self.state.pending == Some(key) {
            self.state.pending = None;
        }
        Ok(())
    }

    /// Open this session's signer to approve opening terms and persist payment authorizations.
    pub async fn channel_signer(
        &self,
        channel_id: Hash256,
    ) -> Result<crate::ChannelSigner<S>, SessionError> {
        let key = self
            .state
            .bindings
            .get(&channel_id)
            .ok_or(SessionError::ChannelNotInDirectory)?;
        Ok(self.root.open_channel(*key).await?)
    }

    /// Handle one `get_channel_signing_status` result. Does not submit.
    /// `now_ms` must come from the wallet's own clock, never the LSP response.
    pub async fn handle_channel_status(
        &mut self,
        channel_id: Hash256,
        status: ChannelSigningStatus,
        now_ms: u64,
    ) -> Result<ProcessOutcome, SessionError> {
        let Some(pending) = self
            .pending_from_channel_status(channel_id, status, now_ms)
            .await?
        else {
            return Ok(ProcessOutcome::Idle);
        };
        self.decide(pending, now_ms).await
    }

    /// Handle a full `get_watchtower_signing_status` (or `watchtower_signing` subscription)
    /// payload with trusted chain verification and wallet-funded transaction + spending terms,
    /// reporting structured [`WatchtowerHealthStatus`] (TTL / backoff) to `handler` alongside
    /// the verified signing [`ProcessOutcome`]. Does not submit.
    pub async fn handle_watchtower_status_result<C: crate::ChainVerifier>(
        &self,
        result: GetWatchtowerSigningStatusResult,
        now_ms: u64,
        funded_spend: Option<(
            ckb_jsonrpc_types::Transaction,
            crate::OnchainSpendAuthorization,
        )>,
        chain: &C,
        handler: &mut impl WatchtowerEventHandler,
    ) -> Result<(WatchtowerHealthStatus, ProcessOutcome), SessionError> {
        let health = evaluate_watchtower_status(&result, now_ms);
        handler.on_status_update(&health);
        let outcome = match result.status {
            WatchtowerSigningStatus::ExternalFundingRequired { .. } => {
                let (funded_tx, auth) = funded_spend.ok_or_else(|| {
                    SessionError::Invalid(
                        "on-chain watchtower signing requires a funded transaction and OnchainSpendAuthorization".into(),
                    )
                })?;
                self.handle_external_funding_watchtower_status(
                    result.channel_id.into(),
                    result.status,
                    funded_tx,
                    auth,
                    chain,
                    now_ms,
                )
                .await?
            }
            WatchtowerSigningStatus::NoSignatureRequired
            | WatchtowerSigningStatus::Backoff { .. } => ProcessOutcome::Idle,
        };
        Ok((health, outcome))
    }

    /// Classify a watchtower RPC error using this session's [`WatchtowerLimitPolicy`]
    /// and notify `handler` if a quota limit was hit.
    pub fn handle_watchtower_rpc_error(
        &self,
        message: &str,
        handler: &mut impl WatchtowerEventHandler,
    ) -> Option<WatchtowerQuotaAction> {
        handle_watchtower_rpc_error(self.watchtower_limit_policy, handler, message)
    }

    /// Sign a request the user has already confirmed. Skips the interaction policy,
    /// but rechecks mandatory commitment validation with fresh wallet time.
    pub async fn confirm(
        &mut self,
        pending: PendingRequest,
        now_ms: u64,
    ) -> Result<SubmitParams, SessionError> {
        self.sign_pending(pending, now_ms).await
    }

    /// Build `create_watch_channel` params with pubkeys only.
    pub async fn watch_channel_params(
        &self,
        channel_id: Hash256,
        remote_funding_pubkey: fiber_types::Pubkey,
        remote_settlement_key: fiber_types::Pubkey,
        funding_udt_type_script: Option<ckb_jsonrpc_types::Script>,
        settlement_data: fiber_json_types::SettlementData,
        commitment_contract_features: fiber_types::ChannelFeatures,
    ) -> Result<CreateWatchChannelParams, SessionError> {
        let key_id = self
            .state
            .bindings
            .get(&channel_id)
            .copied()
            .ok_or(SessionError::ChannelNotInDirectory)?;
        let signer = self.root.open_channel(key_id).await?;
        let keys = signer.public_material().base_public_keys;
        Ok(CreateWatchChannelParams {
            channel_id: channel_id.into(),
            funding_udt_type_script,
            local_settlement_key: None,
            local_settlement_key_pubkey: Some(keys.tlc_base_key.into()),
            remote_settlement_key: remote_settlement_key.into(),
            local_funding_pubkey: keys.funding_pubkey.into(),
            remote_funding_pubkey: remote_funding_pubkey.into(),
            settlement_data,
            channel_features: commitment_contract_features.into(),
        })
    }

    async fn pending_from_channel_status(
        &self,
        channel_id: Hash256,
        status: ChannelSigningStatus,
        now_ms: u64,
    ) -> Result<Option<PendingRequest>, SessionError> {
        let ChannelSigningStatus::SignatureRequired {
            request_id,
            content,
            settlement,
            transition,
            session_evidence,
        } = status
        else {
            return Ok(None);
        };
        let key_id = self
            .state
            .bindings
            .get(&channel_id)
            .copied()
            .ok_or(SessionError::ChannelNotInDirectory)?;
        let signer = self.root.open_channel(key_id).await?;
        let content = musig2_from_rpc(content).map_err(SessionError::Invalid)?;
        let session = crate::json::session_evidence_from_rpc(session_evidence)
            .map_err(SessionError::Invalid)?;
        let settlement = settlement
            .as_ref()
            .map(settlement_from_rpc)
            .transpose()
            .map_err(SessionError::Invalid)?
            .map(
                |(data, local_settlement_key, remote_settlement_key, for_remote)| {
                    OwnedSettlementBinding {
                        data,
                        local_settlement_key,
                        remote_settlement_key,
                        for_remote: Some(for_remote),
                    }
                },
            );
        let content = ChannelSigningContent::Musig2(content);
        let prepared = if content.intent() == crate::SigningIntent::CommitmentTransaction {
            let transition = match transition {
                fiber_json_types::ChannelSigningTransition::SendCommitmentSigned => {
                    fiber_types::ChannelSigningTransition::SendCommitmentSigned
                }
                fiber_json_types::ChannelSigningTransition::CompleteReceivedCommitment => {
                    fiber_types::ChannelSigningTransition::CompleteReceivedCommitment
                }
                _ => {
                    return Err(SessionError::Invalid(
                        "incorrect commitment transition".to_string(),
                    ))
                }
            };
            signer
                .prepare_commitment(
                    content,
                    crate::CommitmentContext {
                        transition,
                        session,
                        settlement: settlement.clone().ok_or_else(|| {
                            SessionError::Invalid("missing commitment settlement".to_string())
                        })?,
                    },
                    now_ms,
                )
                .await?
        } else if content.intent() == crate::SigningIntent::Revocation {
            let transition = match transition {
                fiber_json_types::ChannelSigningTransition::SendRevokeAndAck => {
                    fiber_types::ChannelSigningTransition::SendRevokeAndAck
                }
                fiber_json_types::ChannelSigningTransition::CompleteReceivedRevokeAndAck => {
                    fiber_types::ChannelSigningTransition::CompleteReceivedRevokeAndAck
                }
                _ => {
                    return Err(SessionError::Invalid(
                        "incorrect revocation transition".into(),
                    ))
                }
            };
            let receiving =
                transition == fiber_types::ChannelSigningTransition::CompleteReceivedRevokeAndAck;
            let version = content
                .nonce_slot()
                .ok_or_else(|| SessionError::Invalid("missing nonce".into()))?
                .commitment_number;
            let old_version = version
                .checked_sub(1)
                .ok_or_else(|| SessionError::Invalid("revocation version underflow".into()))?;
            let records = signer.recovery_records().await?;
            let find = |v| {
                records
                    .iter()
                    .find(|r| r.reference.for_remote == receiving && r.reference.version == v)
                    .map(|r| r.reference.clone())
                    .ok_or_else(|| {
                        SessionError::Invalid("missing commitment history for revocation".into())
                    })
            };
            let context = crate::RevocationContext {
                transition,
                session,
                revoked: find(old_version)?,
                replacement: find(version)?,
            };
            signer.prepare_revocation(content, context, now_ms).await?
        } else if content.intent() == crate::SigningIntent::CooperativeCloseTransaction {
            if transition != fiber_json_types::ChannelSigningTransition::SendClosingSigned {
                return Err(SessionError::Invalid("incorrect close transition".into()));
            }
            signer.prepare_close(content, session).await?
        } else if content.intent() == crate::SigningIntent::ChannelAnnouncement {
            if transition != fiber_json_types::ChannelSigningTransition::SignChannelAnnouncement {
                return Err(SessionError::Invalid(
                    "incorrect announcement transition".into(),
                ));
            }
            signer.prepare_announcement(content, session).await?
        } else {
            signer.prepare(content).await?
        };
        Ok(Some(PendingRequest {
            channel_id,
            request_id: request_id.into(),
            prepared,
            settlement,
            kind: RequestKind::Channel,
        }))
    }

    /// Prepare an externally funded watchtower request
    /// ([`WatchtowerSigningStatus::ExternalFundingRequired`]) after the wallet has funded
    /// transaction fees (and any required CKB shortfall, plus `cell_deps`, `inputs[1..]`,
    /// payout/change outputs, and `witness[1..]` when needed) on the watchtower-issued template
    /// transaction. On-chain signatures always require confirmation, regardless of policy.
    ///
    /// Validates `funded_transaction` against both the watchtower template (preserving `inputs[0]`,
    /// any residual commitment `outputs[0]`, `witness[0]`, and any `required_expiry_since`) and the
    /// wallet's trusted [`crate::ChainVerifier`] + [`crate::OnchainSpendAuthorization`].
    pub async fn handle_external_funding_watchtower_status<C: crate::ChainVerifier>(
        &self,
        channel_id: Hash256,
        status: WatchtowerSigningStatus,
        funded_transaction: ckb_jsonrpc_types::Transaction,
        authorization: crate::OnchainSpendAuthorization,
        chain: &C,
        now_ms: u64,
    ) -> Result<ProcessOutcome, SessionError> {
        let (request_id, template, expires_at_ms, required_expiry_since) = match status {
            WatchtowerSigningStatus::ExternalFundingRequired {
                request_id,
                content,
                expires_at_ms,
                required_expiry_since,
            } => (request_id, content, expires_at_ms, required_expiry_since),
            WatchtowerSigningStatus::NoSignatureRequired
            | WatchtowerSigningStatus::Backoff { .. } => {
                return Ok(ProcessOutcome::Idle);
            }
        };
        if expires_at_ms != 0 && now_ms >= expires_at_ms {
            return Err(SessionError::Invalid(
                "watchtower signing request has expired".into(),
            ));
        }
        let template_content = onchain_from_rpc(template);
        let funded_packed: ckb_types::packed::Transaction = funded_transaction.into();
        let normalized_funded = fiber_types::validate_and_normalize_external_funding_watchtower_tx(
            &template_content.transaction,
            &funded_packed,
            required_expiry_since,
            None,
        )
        .map_err(SessionError::Invalid)?;
        let funded_content = crate::OnchainSigningContent {
            key_purpose: template_content.key_purpose,
            transaction: normalized_funded,
        };
        let signer = self.channel_signer(channel_id).await?;
        let prepared = signer
            .prepare_onchain(funded_content, authorization, chain)
            .await?;
        Ok(ProcessOutcome::NeedConfirmation(PendingRequest {
            channel_id,
            request_id: request_id.into(),
            prepared,
            settlement: None,
            kind: RequestKind::WatchtowerExternalFunding {
                required_expiry_since,
                expires_at_ms,
            },
        }))
    }

    /// Confirm an externally funded on-chain watchtower request (`pending.is_external_funding() == true`),
    /// rechecking request expiry, live chain inputs, and maturity before signing `inputs[0]` and
    /// returning [`SubmitWatchtowerSignatureParams`] carrying both `signature` and
    /// `signed_transaction` (with all client funding inputs, cell_deps, outputs, and `witness[1..]`
    /// intact).
    pub async fn confirm_external_funding_onchain<C: crate::ChainVerifier>(
        &self,
        pending: PendingRequest,
        chain: &C,
        now_ms: u64,
    ) -> Result<SubmitParams, SessionError> {
        let expires_at_ms = match pending.kind {
            RequestKind::WatchtowerExternalFunding { expires_at_ms, .. } => expires_at_ms,
            RequestKind::Channel => {
                return Err(SessionError::Invalid("expected watchtower request".into()));
            }
        };
        if expires_at_ms != 0 && now_ms >= expires_at_ms {
            return Err(SessionError::Invalid(
                "watchtower signing request has expired".into(),
            ));
        }
        let crate::ChannelSigningContent::Onchain(onchain) = pending.prepared.content() else {
            return Err(SessionError::Invalid("expected on-chain content".into()));
        };
        let funded_tx: ckb_jsonrpc_types::Transaction = onchain.transaction.clone().into();
        let signer = self.channel_signer(pending.channel_id).await?;
        let ChannelSignature::Onchain(signature) =
            signer.sign_onchain(pending.prepared, chain).await?
        else {
            return Err(SessionError::Invalid("expected on-chain signature".into()));
        };
        Ok(SubmitParams::Watchtower(SubmitWatchtowerSignatureParams {
            channel_id: pending.channel_id.into(),
            request_id: pending.request_id.into(),
            signature: signature.signature.to_vec(),
            signed_transaction: funded_tx,
        }))
    }

    async fn decide(
        &self,
        pending: PendingRequest,
        now_ms: u64,
    ) -> Result<ProcessOutcome, SessionError> {
        let decision = self.policy.decide_prepared(&pending.prepared);
        match decision {
            SigningDecision::Allow => Ok(ProcessOutcome::ReadyToSubmit(
                self.sign_pending(pending, now_ms).await?,
            )),
            SigningDecision::RequireConfirmation => Ok(ProcessOutcome::NeedConfirmation(pending)),
            SigningDecision::Deny => Ok(ProcessOutcome::Denied),
        }
    }

    async fn sign_pending(
        &self,
        pending: PendingRequest,
        now_ms: u64,
    ) -> Result<SubmitParams, SessionError> {
        let key_id = self
            .state
            .bindings
            .get(&pending.channel_id)
            .copied()
            .ok_or(SessionError::ChannelNotInDirectory)?;
        let signer = self.root.open_channel(key_id).await?;
        let slot = pending.prepared.content().nonce_slot();
        let content_hash = pending.prepared.review().content_hash;
        let include_commitment_points = matches!(
            &pending.prepared.validation,
            crate::protocol::PreparedValidation::Commitment { context, .. }
                if context.transition
                    == fiber_types::ChannelSigningTransition::CompleteReceivedCommitment
        ) || matches!(
            &pending.prepared.validation,
            crate::protocol::PreparedValidation::Revocation { context, .. }
                if context.transition == fiber_types::ChannelSigningTransition::SendRevokeAndAck
        );
        let signature = match &pending.prepared.validation {
            crate::protocol::PreparedValidation::Commitment { .. } => {
                signer.sign_commitment(pending.prepared, now_ms).await?
            }
            crate::protocol::PreparedValidation::Revocation { .. } => {
                signer.sign_revocation(pending.prepared, now_ms).await?
            }
            crate::protocol::PreparedValidation::Close { .. }
            | crate::protocol::PreparedValidation::Announcement { .. } => {
                signer.sign(pending.prepared).await?
            }
            crate::protocol::PreparedValidation::Onchain { .. } => {
                return Err(SessionError::Invalid(
                    "on-chain confirmation requires a ChainVerifier".into(),
                ));
            }
        };
        match (pending.kind, signature) {
            (RequestKind::Channel, ChannelSignature::Musig2(signature)) => {
                let next_material = match slot {
                    Some(slot) => Some(next_material_to_rpc(
                        &signer
                            .next_material_for_request(
                                slot,
                                content_hash,
                                include_commitment_points,
                            )
                            .await?,
                    )),
                    None => None,
                };
                Ok(SubmitParams::Channel(SubmitChannelSignatureParams {
                    channel_id: pending.channel_id.into(),
                    request_id: pending.request_id.into(),
                    partial_signature: signature.partial_signature.serialize(),
                    next_material,
                }))
            }
            _ => Err(SessionError::Invalid(
                "signature type does not match the signing request".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests;
