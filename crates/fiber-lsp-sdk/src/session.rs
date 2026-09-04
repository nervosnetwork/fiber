//! Hosted LSP session: registration, channel directory, and one-shot status handling.
//!
//! This module does not perform network I/O. The caller fetches RPC results and
//! submits the returned parameters.

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
    SigningDecision, SigningPolicy, SigningPolicyInput,
};

/// Persistable hosted-session map. The caller owns file or IndexedDB I/O.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostedSessionState {
    /// Tenant Biscuit after a successful registration.
    pub tenant_token: Option<String>,
    /// Fiber `channel_id` → local signer key id.
    pub bindings: HashMap<Hash256, ChannelKeyId>,
    /// At most one unallocated channel key waiting for `open_channel_with_external_funding`.
    pub pending: Option<ChannelKeyId>,
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

/// Hosted tenant session. Does not talk to the network.
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

    /// Bind the pending signer to a user-approved unsigned funding transaction.
    pub async fn bind_approved_funding(
        &mut self,
        channel_id: Hash256,
        unsigned_funding_tx: &ckb_types::packed::Transaction,
        local_shutdown_script: ckb_types::packed::Script,
        funding_output_index: u32,
    ) -> Result<(), SessionError> {
        if self.state.bindings.contains_key(&channel_id) {
            return Ok(());
        }
        let pending = self.state.pending.ok_or(SessionError::NoPendingChannel)?;
        let expected_inputs: Vec<_> = unsigned_funding_tx
            .raw()
            .inputs()
            .into_iter()
            .map(|input| input.previous_output())
            .collect();
        let signer = self.root.open_channel(pending).await?;
        signer
            .bind_from_approved_funding(
                unsigned_funding_tx,
                funding_output_index,
                local_shutdown_script,
                &expected_inputs,
            )
            .await?;
        self.state.pending = None;
        self.state.bindings.insert(channel_id, pending);
        Ok(())
    }

    /// Handle one `get_channel_signing_status` result. Does not submit.
    pub async fn handle_channel_status(
        &mut self,
        channel_id: Hash256,
        status: ChannelSigningStatus,
    ) -> Result<ProcessOutcome, SessionError> {
        let Some(pending) = self.pending_from_channel_status(channel_id, status).await? else {
            return Ok(ProcessOutcome::Idle);
        };
        self.decide(pending).await
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
        self.decide(pending).await
    }

    /// Sign a request the user has already confirmed. Skips policy.
    pub async fn confirm(&mut self, pending: PendingRequest) -> Result<SubmitParams, SessionError> {
        self.sign_pending(pending).await
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
    ) -> Result<Option<PendingRequest>, SessionError> {
        let ChannelSigningStatus::SignatureRequired {
            request_id,
            content,
            settlement,
            ..
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
        let prepared = signer
            .prepare(ChannelSigningContent::Musig2(content))
            .await?;
        Ok(Some(PendingRequest {
            channel_id,
            request_id: request_id.into(),
            prepared,
            settlement,
            kind: RequestKind::Channel,
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

    async fn decide(&self, pending: PendingRequest) -> Result<ProcessOutcome, SessionError> {
        let settlement = pending
            .settlement
            .as_ref()
            .map(OwnedSettlementBinding::as_binding);
        match self.policy.decide(SigningPolicyInput {
            review: pending.prepared.review(),
            content: pending.prepared.content(),
            settlement,
            registry: &self.registry,
        }) {
            SigningDecision::Allow => Ok(ProcessOutcome::ReadyToSubmit(
                self.sign_pending(pending).await?,
            )),
            SigningDecision::RequireConfirmation => Ok(ProcessOutcome::NeedConfirmation(pending)),
            SigningDecision::Deny => Ok(ProcessOutcome::Denied),
        }
    }

    async fn sign_pending(&self, pending: PendingRequest) -> Result<SubmitParams, SessionError> {
        let key_id = self
            .state
            .bindings
            .get(&pending.channel_id)
            .copied()
            .ok_or(SessionError::ChannelNotInDirectory)?;
        let signer = self.root.open_channel(key_id).await?;
        let slot = pending.prepared.content().nonce_slot();
        let signature = signer.sign(pending.prepared).await?;
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
    use fiber_types::{settlement_witness_hash, Privkey, SettlementData, SettlementTlc, TLCId};
    use musig2::{AggNonce, KeyAggContext, SecNonce};

    use super::*;
    use crate::{
        json::musig2_to_rpc, CommitmentCounter, MemoryStore, Musig2SignableContent, NoncePurpose,
        NonceSlot, RootKey,
    };

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
    async fn bind_consumes_the_pending_channel() {
        let mut session = session().await;
        session.allocate_pending_channel().await.expect("allocate");
        let pending = session.pending_channel_key_id().expect("pending");
        let signer = session.root.open_channel(pending).await.expect("open");
        let remote = Privkey::from(&[9; 32]).pubkey();
        let input = ckb_types::packed::OutPoint::new_builder()
            .tx_hash([7u8; 32].pack())
            .index(0u32)
            .build();
        let ctx = KeyAggContext::new([
            signer.public_material().base_public_keys.funding_pubkey,
            remote,
        ])
        .expect("agg");
        let point: musig2::secp::Point = ctx.aggregated_pubkey();
        let digest = fiber_types::blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
        let lock = ckb_types::packed::Script::new_builder()
            .args(digest[..20].to_vec().pack())
            .build();
        let shutdown = ckb_types::packed::Script::new_builder()
            .args([1u8, 2, 3].pack())
            .build();
        let tx = ckb_types::core::TransactionBuilder::default()
            .input(
                ckb_types::packed::CellInput::new_builder()
                    .previous_output(input.clone())
                    .build(),
            )
            .output(
                ckb_types::packed::CellOutput::new_builder()
                    .lock(lock)
                    .capacity(1000u64)
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        let channel_id = Hash256::from([0x11; 32]);
        session
            .bind_approved_funding(channel_id, &tx, shutdown, 0)
            .await
            .expect("bind");
        assert!(session.pending_channel_key_id().is_none());
        assert_eq!(session.binding(channel_id), Some(pending));
    }

    async fn bound_session_with_inbound() -> (
        HostedSession<MemoryStore>,
        Hash256,
        ChannelSigningStatus,
        Hash256,
    ) {
        let mut session = session().await;
        session.allocate_pending_channel().await.expect("allocate");
        let pending = session.pending_channel_key_id().expect("pending");
        let signer = session.root.open_channel(pending).await.expect("open");
        let remote_secret = secp256k1::SecretKey::from_byte_array(&[3u8; 32]).unwrap();
        let remote_pubkey =
            secp256k1::PublicKey::from_secret_key(secp256k1::SECP256K1, &remote_secret);
        let remote = fiber_types::Pubkey::from(remote_pubkey);
        let funding = ckb_types::packed::OutPoint::new_builder()
            .tx_hash([7u8; 32].pack())
            .index(0u32)
            .build();
        let ctx = KeyAggContext::new([
            signer.public_material().base_public_keys.funding_pubkey,
            remote,
        ])
        .expect("agg");
        let point: musig2::secp::Point = ctx.aggregated_pubkey();
        let digest = fiber_types::blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
        let lock = ckb_types::packed::Script::new_builder()
            .args(digest[..20].to_vec().pack())
            .build();
        let shutdown = ckb_types::packed::Script::new_builder()
            .args([1u8, 2, 3].pack())
            .build();
        let funding_tx = ckb_types::core::TransactionBuilder::default()
            .input(
                ckb_types::packed::CellInput::new_builder()
                    .previous_output(funding.clone())
                    .build(),
            )
            .output(
                ckb_types::packed::CellOutput::new_builder()
                    .lock(lock)
                    .capacity(1000u64)
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        let channel_id = Hash256::from([0x22; 32]);
        session
            .bind_approved_funding(channel_id, &funding_tx, shutdown, 0)
            .await
            .expect("bind");

        let payment_hash = Hash256::from([9; 32]);
        let local_key = Privkey::from(&[1; 32]).pubkey();
        let remote_settle = Privkey::from(&[2; 32]).pubkey();
        let settlement = SettlementData {
            local_amount: 15,
            remote_amount: 1,
            tlcs: vec![SettlementTlc {
                tlc_id: TLCId::Received(0),
                hash_algorithm: Default::default(),
                payment_amount: 5,
                payment_hash,
                expiry: 1_000,
                local_key: None,
                local_key_pubkey: Some(Privkey::from(&[3; 32]).pubkey()),
                local_key_commitment_number: None,
                remote_key: Privkey::from(&[7; 32]).pubkey(),
            }],
        };
        let hash = settlement_witness_hash(&settlement, true, local_key, remote_settle);
        let mut args = vec![0u8; 36];
        args.extend_from_slice(&hash);
        args.push(0x00);
        let commitment = ckb_types::core::TransactionBuilder::default()
            .input(
                ckb_types::packed::CellInput::new_builder()
                    .previous_output(ckb_types::packed::OutPoint::new(
                        funding_tx.calc_tx_hash(),
                        0,
                    ))
                    .build(),
            )
            .output(
                ckb_types::packed::CellOutput::new_builder()
                    .lock(
                        ckb_types::packed::Script::new_builder()
                            .args(args.pack())
                            .build(),
                    )
                    .capacity(1000u64)
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        let slot = NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: 1,
        };
        let local_nonce = signer
            .get_musig2_nonce(slot)
            .await
            .expect("nonce")
            .public_nonce;
        let remote_nonce = SecNonce::build([7u8; 32]).build().public_nonce();
        let content = crate::Musig2SigningContent {
            slot,
            commitment_counter: Some(CommitmentCounter::Local),
            key_agg_ctx: ctx,
            agg_nonce: AggNonce::sum([local_nonce, remote_nonce]),
            content: Musig2SignableContent::CommitmentTransaction(commitment),
        };
        let (data, local_s, remote_s, for_remote) =
            (settlement.clone(), local_key, remote_settle, true);
        let json_settlement = {
            let owned = OwnedSettlementBinding {
                data,
                local_settlement_key: local_s,
                remote_settlement_key: remote_s,
                for_remote: Some(for_remote),
            };
            fiber_json_types::SigningSettlement {
                local_amount: owned.data.local_amount,
                remote_amount: owned.data.remote_amount,
                local_settlement_pubkey: owned.local_settlement_key.into(),
                remote_settlement_pubkey: owned.remote_settlement_key.into(),
                for_remote,
                tlcs: owned
                    .data
                    .tlcs
                    .iter()
                    .map(|tlc| fiber_json_types::SigningSettlementTlc {
                        inbound: matches!(tlc.tlc_id, TLCId::Received(_)),
                        payment_hash: tlc.payment_hash.into(),
                        payment_amount: tlc.payment_amount,
                        hash_algorithm: tlc.hash_algorithm.into(),
                        expiry: tlc.expiry,
                        local_key_pubkey: tlc.local_pubkey().into(),
                        remote_key: tlc.remote_key.into(),
                    })
                    .collect(),
            }
        };
        let status = ChannelSigningStatus::SignatureRequired {
            request_id: Hash256::from([0x33; 32]).into(),
            transition: ChannelSigningTransition::SendCommitmentSigned,
            content: musig2_to_rpc(&content),
            settlement: Some(json_settlement),
        };
        (session, channel_id, status, payment_hash)
    }

    #[tokio::test]
    async fn auto_allows_an_issued_inbound_commitment() {
        let (mut session, channel_id, status, payment_hash) = bound_session_with_inbound().await;
        session.registry_mut().record_issued_invoice(payment_hash);
        session.registry_mut().note_signed_balance(10);
        let outcome = session
            .handle_channel_status(channel_id, status)
            .await
            .expect("handle");
        assert!(matches!(outcome, ProcessOutcome::ReadyToSubmit(_)));
    }

    #[tokio::test]
    async fn auto_denies_an_unissued_inbound_commitment() {
        let (mut session, channel_id, status, _) = bound_session_with_inbound().await;
        session.registry_mut().note_signed_balance(10);
        let outcome = session
            .handle_channel_status(channel_id, status)
            .await
            .expect("handle");
        assert!(matches!(outcome, ProcessOutcome::Denied));
    }

    #[tokio::test]
    async fn manual_asks_before_signing() {
        let (session, channel_id, status, payment_hash) = bound_session_with_inbound().await;
        let mut session = session.with_policy(SigningPolicy::Manual);
        session.registry_mut().record_issued_invoice(payment_hash);
        let outcome = session
            .handle_channel_status(channel_id, status)
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
    async fn auto_asks_before_a_watchtower_settlement() {
        let (mut session, channel_id, _, _) = bound_session_with_inbound().await;
        let outcome = session
            .handle_watchtower_status(channel_id, watchtower_required_status())
            .await
            .expect("handle");
        assert!(matches!(outcome, ProcessOutcome::NeedConfirmation(_)));
    }

    #[tokio::test]
    async fn confirm_watchtower_request_builds_submit_params() {
        let (mut session, channel_id, _, _) = bound_session_with_inbound().await;
        let ProcessOutcome::NeedConfirmation(pending) = session
            .handle_watchtower_status(channel_id, watchtower_required_status())
            .await
            .expect("handle")
        else {
            panic!("expected confirmation");
        };
        let SubmitParams::Watchtower(params) = session.confirm(pending).await.expect("confirm")
        else {
            panic!("expected watchtower submit");
        };
        assert_eq!(params.channel_id, channel_id.into());
        assert_eq!(params.request_id, Hash256::from([0x44; 32]).into());
        assert_eq!(params.signature.len(), 65);
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
