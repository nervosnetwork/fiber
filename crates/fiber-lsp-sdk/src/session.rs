//! Hosted LSP session: registration, channel directory, and one-shot status handling.
//!
//! Opening uses caller-provided transport, wallet verification and persistence.
//! For signing, the caller fetches RPC results and submits the returned parameters.

use std::collections::HashMap;

use fiber_json_types::{
    ChannelSigningStatus, CreateWatchChannelParams, GetLspTenantRegistryNonceResult,
    RegisterLspTenantParams, RegisterLspTenantResult, SubmitChannelSignatureParams,
    SubmitWatchtowerSignatureParams, WatchtowerSigningStatus,
};
use fiber_types::{Hash256, TenantId, TenantRegistryPayload};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    json::{musig2_from_rpc, next_material_to_rpc, onchain_from_rpc, settlement_from_rpc},
    ChannelKeyId, ChannelOpenSignerMaterial, ChannelSignature, ChannelSigningContent,
    OwnedSettlementBinding, PaymentRegistry, PreparedSigning, RootSigner, SignerError, SignerStore,
    SigningDecision, SigningPolicy,
};

/// Persistable hosted-session map. The caller owns file or IndexedDB I/O.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostedSessionState {
    /// Tenant Biscuit after a successful registration.
    pub tenant_token: Option<String>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
    Channel,
    Watchtower,
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
    registry: PaymentRegistry,
}

impl<S: SignerStore> HostedSession<S> {
    /// Create a session with [`SigningPolicy::Auto`].
    pub fn new(root: RootSigner<S>) -> Self {
        Self {
            root,
            state: HostedSessionState::default(),
            policy: SigningPolicy::Auto,
            registry: PaymentRegistry::default(),
        }
    }

    /// Replace the signing policy. Production builds only have `Auto` and `Manual`.
    pub fn with_policy(mut self, policy: SigningPolicy) -> Self {
        self.policy = policy;
        self
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

    /// Build a registration request from an LSP nonce. The caller sends it.
    pub fn begin_registration(
        &self,
        nonce: GetLspTenantRegistryNonceResult,
    ) -> Result<RegisterLspTenantParams, SessionError> {
        let root_signer_pubkey: fiber_types::Pubkey = self.root.identity_public_key().into();
        let returned_root = fiber_types::Pubkey::try_from(nonce.root_signer_pubkey)
            .map_err(SessionError::Invalid)?;
        if returned_root != root_signer_pubkey {
            return Err(SessionError::Invalid(
                "LSP returned a nonce for another RootSigner".to_string(),
            ));
        }
        let nonce_hash: Hash256 = nonce.nonce.into();
        let payload = TenantRegistryPayload::new(
            fiber_types::Pubkey::try_from(nonce.lsp_node_id).map_err(SessionError::Invalid)?,
            root_signer_pubkey,
            nonce_hash.into(),
        );
        let signature = self.root.sign_tenant_registry_payload(&payload)?;
        Ok(RegisterLspTenantParams {
            root_signer_pubkey: root_signer_pubkey.into(),
            nonce: nonce_hash.into(),
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
        self.state.tenant_token = Some(result.access_token);
        Ok(())
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

    /// Open a tenant channel and return only after wallet/context verification and persistence.
    /// The original intent is persisted before the RPC. Retry the identical request
    /// after interruption; the existing pending signer and intent are reused.
    pub async fn open_tenant_channel<R, V, P>(
        &mut self,
        request: fiber_json_types::OpenChannelWithExternalFundingParams,
        network: &crate::OpeningNetwork,
        rpc: &R,
        wallet: &V,
        persistence: &P,
    ) -> Result<fiber_json_types::OpenTenantChannelResult, SessionError>
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
        let result = rpc.open_tenant_channel(&token, request.clone()).await?;
        let inputs = wallet.verify_funding(&request, &result).await?;
        self.finish_channel_opening(&result, network, &inputs)
            .await?;
        persistence.save_session(&self.state).await?;
        Ok(result)
    }

    /// Persist the original client request before sending `open_tenant_channel`.
    /// The caller must persist `state()` before performing network I/O.
    pub(crate) async fn begin_channel_opening(
        &mut self,
        request: fiber_json_types::OpenChannelWithExternalFundingParams,
    ) -> Result<fiber_json_types::OpenChannelWithExternalFundingParams, SessionError> {
        if request.public != Some(false) {
            return Err(SessionError::Invalid(
                "tenant opening requires public: false".into(),
            ));
        }
        if !self.state.bindings.is_empty() {
            let encoded =
                serde_json::to_vec(&request).map_err(|e| SessionError::Invalid(e.to_string()))?;
            if self.state.opening_request.as_ref() == Some(&encoded) {
                return Ok(request);
            }
            return Err(SessionError::Invalid(
                "tenant already has an approved channel".into(),
            ));
        }
        let material = self.allocate_pending_channel().await?;
        let expected = crate::json::open_material_to_rpc(&material);
        if serde_json::to_value(Some(&expected))
            .map_err(|e| SessionError::Invalid(e.to_string()))?
            != serde_json::to_value(&request.external_channel_signer)
                .map_err(|e| SessionError::Invalid(e.to_string()))?
        {
            return Err(SessionError::Invalid(
                "opening material is not owned by the pending signer".into(),
            ));
        }
        if request.commitment_delay_epoch.is_none() || request.commitment_fee_rate.is_none() {
            return Err(SessionError::Invalid(
                "choose commitment delay and fee rate before opening".into(),
            ));
        }
        let encoded =
            serde_json::to_vec(&request).map_err(|e| SessionError::Invalid(e.to_string()))?;
        if self
            .state
            .opening_request
            .as_ref()
            .is_some_and(|old| old != &encoded)
        {
            return Err(SessionError::Invalid(
                "cannot replace pending opening intent".into(),
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
        result: &fiber_json_types::OpenTenantChannelResult,
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
            .pending
            .or_else(|| self.state.bindings.get(&channel_id).copied())
            .ok_or(SessionError::NoPendingChannel)?;
        let signer = self.root.open_channel(key).await?;
        signer
            .verify_and_record_opening(&request, result, network, expected_inputs)
            .await?;
        self.state.bindings.insert(channel_id, key);
        self.state.pending = None;
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

    /// Handle one `get_watchtower_signing_status` result. Does not submit.
    pub async fn handle_watchtower_status(
        &mut self,
        channel_id: Hash256,
        status: WatchtowerSigningStatus,
    ) -> Result<ProcessOutcome, SessionError> {
        let Some(pending) = self
            .pending_from_watchtower_status(channel_id, status)
            .await?
        else {
            return Ok(ProcessOutcome::Idle);
        };
        self.decide(pending, 0).await
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

    /// Prepare a watchtower request with wallet-approved spending terms and trusted chain data.
    /// On-chain signatures always require confirmation, regardless of policy.
    pub async fn handle_watchtower_status_verified<C: crate::ChainVerifier>(
        &self,
        channel_id: Hash256,
        status: WatchtowerSigningStatus,
        authorization: crate::OnchainSpendAuthorization,
        chain: &C,
    ) -> Result<ProcessOutcome, SessionError> {
        let WatchtowerSigningStatus::SignatureRequired {
            request_id,
            content,
        } = status
        else {
            return Ok(ProcessOutcome::Idle);
        };
        let signer = self.channel_signer(channel_id).await?;
        let prepared = signer
            .prepare_onchain(onchain_from_rpc(content), authorization, chain)
            .await?;
        Ok(ProcessOutcome::NeedConfirmation(PendingRequest {
            channel_id,
            request_id: request_id.into(),
            prepared,
            settlement: None,
            kind: RequestKind::Watchtower,
        }))
    }

    /// Confirm an on-chain request, rechecking live chain inputs and maturity before signing.
    pub async fn confirm_onchain<C: crate::ChainVerifier>(
        &self,
        pending: PendingRequest,
        chain: &C,
    ) -> Result<SubmitParams, SessionError> {
        if !matches!(pending.kind, RequestKind::Watchtower) {
            return Err(SessionError::Invalid("expected watchtower request".into()));
        }
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
        }))
    }

    async fn pending_from_watchtower_status(
        &self,
        channel_id: Hash256,
        status: WatchtowerSigningStatus,
    ) -> Result<Option<PendingRequest>, SessionError> {
        let WatchtowerSigningStatus::SignatureRequired {
            request_id,
            content,
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
        let prepared = signer
            .prepare(ChannelSigningContent::Onchain(onchain_from_rpc(content)))
            .await?;
        Ok(Some(PendingRequest {
            channel_id,
            request_id: request_id.into(),
            prepared,
            settlement: None,
            kind: RequestKind::Watchtower,
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
                    Some(slot) => Some(next_material_to_rpc(&signer.next_material(slot).await?)),
                    None => None,
                };
                Ok(SubmitParams::Channel(SubmitChannelSignatureParams {
                    channel_id: pending.channel_id.into(),
                    request_id: pending.request_id.into(),
                    partial_signature: signature.partial_signature.serialize(),
                    next_material,
                }))
            }
            (RequestKind::Watchtower, ChannelSignature::Onchain(signature)) => {
                Ok(SubmitParams::Watchtower(SubmitWatchtowerSignatureParams {
                    channel_id: pending.channel_id.into(),
                    request_id: pending.request_id.into(),
                    signature: signature.signature.to_vec(),
                }))
            }
            _ => Err(SessionError::Invalid(
                "signature type does not match the signing request".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use ckb_types::prelude::*;
    use fiber_json_types::{
        ChannelSigningTransition, GetLspTenantRegistryNonceResult, LspTenantRuntimeStatus,
        LspTenantStatus, RegisterLspTenantResult,
    };
    use fiber_types::Privkey;

    use super::*;
    use crate::{json::musig2_to_rpc, MemoryStore, RootKey};

    fn root_key() -> RootKey {
        RootKey::import([42; 32]).expect("root key")
    }

    fn lsp_pubkey() -> fiber_types::Pubkey {
        Privkey::from(&[7; 32]).pubkey()
    }

    async fn session() -> HostedSession<MemoryStore> {
        let created = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root");
        HostedSession::new(created)
    }

    fn nonce_for(session: &HostedSession<MemoryStore>) -> GetLspTenantRegistryNonceResult {
        GetLspTenantRegistryNonceResult {
            lsp_node_id: lsp_pubkey().into(),
            root_signer_pubkey: fiber_types::Pubkey::from(session.root.identity_public_key())
                .into(),
            nonce: Hash256::from([3; 32]).into(),
        }
    }

    #[tokio::test]
    async fn new_session_defaults_to_auto() {
        let session = session().await;
        assert_eq!(session.policy, SigningPolicy::Auto);
    }

    #[tokio::test]
    async fn registration_rejects_a_nonce_for_another_root() {
        let session = session().await;
        let mut nonce = nonce_for(&session);
        nonce.root_signer_pubkey = lsp_pubkey().into();
        assert!(matches!(
            session.begin_registration(nonce),
            Err(SessionError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn registration_round_trip_stores_the_token() {
        let mut session = session().await;
        let params = session
            .begin_registration(nonce_for(&session))
            .expect("begin");
        session
            .finish_registration(RegisterLspTenantResult {
                tenant: LspTenantStatus {
                    tenant_id: session.tenant_id().as_str().to_string(),
                    root_signer_pubkey: Some(params.root_signer_pubkey),
                    invoice_pubkey: lsp_pubkey().into(),
                    private_channel_id: None,
                    created_at: 1,
                    runtime_status: LspTenantRuntimeStatus::Cold,
                    channel_online: false,
                },
                access_token: "token".to_string(),
            })
            .expect("finish");
        assert_eq!(session.tenant_token(), Some("token"));
    }

    #[tokio::test]
    async fn opening_consumes_the_pending_channel() {
        let mut session = session().await;
        session.allocate_pending_channel().await.unwrap();
        let pending = session.pending_channel_key_id().unwrap();
        let signer = session.open_channel(pending).await.unwrap();
        let (request, result, network, inputs) = crate::opening::tests::proposal(&signer).await;
        session.begin_channel_opening(request).await.unwrap();
        session
            .finish_channel_opening(&result, &network, &inputs)
            .await
            .unwrap();
        assert!(session.pending_channel_key_id().is_none());
        assert_eq!(session.binding(result.channel_id.into()), Some(pending));
    }

    async fn bound_session_with_inbound() -> (
        HostedSession<MemoryStore>,
        Hash256,
        ChannelSigningStatus,
        Hash256,
    ) {
        let f = crate::commitment_tests::Fixture::new(false).await;
        f.sign(true, 0, f.opening()).await;
        let (content, context) = f.request(true, 1, f.incoming(true)).await;
        let ChannelSigningContent::Musig2(content) = content else {
            panic!()
        };
        let snapshot = context.settlement;
        let settlement = fiber_json_types::SigningSettlement {
            local_amount: snapshot.data.local_amount,
            remote_amount: snapshot.data.remote_amount,
            local_settlement_pubkey: snapshot.local_settlement_key.into(),
            remote_settlement_pubkey: snapshot.remote_settlement_key.into(),
            for_remote: true,
            tlcs: snapshot
                .data
                .tlcs
                .iter()
                .map(|tlc| fiber_json_types::SigningSettlementTlc {
                    tlc_id: u64::from(tlc.tlc_id),
                    local_key_commitment_number: tlc.local_key_commitment_number.unwrap(),
                    inbound: true,
                    payment_hash: tlc.payment_hash.into(),
                    payment_amount: tlc.payment_amount,
                    hash_algorithm: tlc.hash_algorithm.into(),
                    expiry: tlc.expiry,
                    local_key_pubkey: tlc.local_pubkey().into(),
                    remote_key: tlc.remote_key.into(),
                })
                .collect(),
        };
        let channel_id = Hash256::from([0x22; 32]);
        let hash = f.terms(true).payment_hash;
        let mut session = HostedSession::new(f.root);
        session
            .state
            .bindings
            .insert(channel_id, f.signer.channel_key_id());
        let status = ChannelSigningStatus::SignatureRequired {
            session_evidence: crate::json::session_evidence_to_rpc(&context.session),
            request_id: Hash256::from([0x33; 32]).into(),
            transition: ChannelSigningTransition::SendCommitmentSigned,
            content: musig2_to_rpc(&content),
            settlement: Some(settlement),
        };
        (session, channel_id, status, hash)
    }

    async fn authorize_receive(
        session: &HostedSession<MemoryStore>,
        channel_id: Hash256,
        payment_hash: Hash256,
    ) {
        session
            .channel_signer(channel_id)
            .await
            .unwrap()
            .authorize_invoice(
                crate::PaymentAuthorization {
                    payment_hash,
                    hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
                    inbound: true,
                    amount: 50,
                    expires_at_ms: 10_000,
                    min_tlc_expiry_delta_ms: 1000,
                    max_tlc_expiry_ms: 100_000,
                },
                [9; 32].into(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn auto_allows_an_issued_inbound_commitment() {
        let (mut session, channel_id, status, payment_hash) = bound_session_with_inbound().await;
        authorize_receive(&session, channel_id, payment_hash).await;
        session.registry_mut().note_signed_balance(10);
        let outcome = session
            .handle_channel_status(channel_id, status, 0)
            .await
            .expect("handle");
        assert!(matches!(outcome, ProcessOutcome::ReadyToSubmit(_)));
    }

    #[tokio::test]
    async fn auto_denies_an_unissued_inbound_commitment() {
        let (mut session, channel_id, status, _) = bound_session_with_inbound().await;
        session.registry_mut().note_signed_balance(10);
        let error = session
            .handle_channel_status(channel_id, status, 0)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no local payment authorization"));
    }

    #[tokio::test]
    async fn hash_only_registry_cannot_authorize_an_unverified_receipt() {
        let (mut session, channel_id, status, hash) = bound_session_with_inbound().await;
        session.registry_mut().record_issued_invoice(hash);
        session.registry_mut().note_signed_balance(0);
        assert!(session
            .handle_channel_status(channel_id, status, 0)
            .await
            .unwrap_err()
            .to_string()
            .contains("no local payment authorization"));
    }

    #[tokio::test]
    async fn manual_asks_before_signing() {
        let (session, channel_id, status, payment_hash) = bound_session_with_inbound().await;
        let mut session = session.with_policy(SigningPolicy::Manual);
        authorize_receive(&session, channel_id, payment_hash).await;
        let outcome = session
            .handle_channel_status(channel_id, status, 0)
            .await
            .expect("handle");
        assert!(matches!(outcome, ProcessOutcome::NeedConfirmation(_)));
    }

    fn watchtower_required_status() -> WatchtowerSigningStatus {
        let tx = ckb_types::core::TransactionBuilder::default()
            .output(
                ckb_types::packed::CellOutput::new_builder()
                    .capacity(1000u64)
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        WatchtowerSigningStatus::SignatureRequired {
            request_id: Hash256::from([0x44; 32]).into(),
            content: fiber_json_types::OnchainSigningContent {
                key_purpose: fiber_json_types::OnchainKeyPurpose::Settlement,
                transaction: tx.into(),
            },
        }
    }

    #[tokio::test]
    async fn auto_rejects_watchtower_without_verified_chain_context() {
        let (mut session, channel_id, _, _) = bound_session_with_inbound().await;
        assert!(session
            .handle_watchtower_status(channel_id, watchtower_required_status())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn manual_cannot_bypass_missing_chain_context() {
        let (session, channel_id, _, _) = bound_session_with_inbound().await;
        let mut session = session.with_policy(SigningPolicy::Manual);
        assert!(session
            .handle_watchtower_status(channel_id, watchtower_required_status())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn watch_channel_params_omit_the_settlement_secret() {
        let (session, channel_id, _, _) = bound_session_with_inbound().await;
        let params = session
            .watch_channel_params(
                channel_id,
                Privkey::from(&[8; 32]).pubkey(),
                Privkey::from(&[9; 32]).pubkey(),
                None,
                fiber_json_types::SettlementData {
                    local_amount: 1,
                    remote_amount: 1,
                    tlcs: Vec::new(),
                },
            )
            .await
            .expect("watch params");
        assert!(params.local_settlement_key.is_none());
        assert!(params.local_settlement_key_pubkey.is_some());
        assert_eq!(params.channel_id, channel_id.into());
    }
}
