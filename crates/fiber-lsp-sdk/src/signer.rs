use std::fmt;

use fiber_types::{
    blake2b_hash_with_salt, ChannelOpenSignerMaterial, InMemorySigner, NextChannelSignerMaterial,
    Pubkey,
};
use musig2::{sign_partial, PubNonce, SecNonce, SecNonceBuilder};
use secp256k1::{Message, PublicKey, SECP256K1};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    ChannelBinding, ChannelKeyId, ChannelPublicMaterial, ChannelSignature, ChannelSigningContent,
    MemoryStore, Musig2Nonce, Musig2SignableContent, Musig2Signature, NoncePurpose, NonceSlot,
    OnchainKeyPurpose, OnchainSignature, PreparedSigning, RootKey, RootKeyBackup, SignerError,
    SignerStore, SigningIntent, SigningReview,
};

use crate::protocol::{PreparedValidation, SigningData};

const STORE_FORMAT_VERSION: u16 = 1;
const DERIVATION_VERSION: u16 = 1;
const METADATA_KEY: &[u8] = b"fiber-lsp-sdk/signer/metadata";
const CHANNEL_KEY_PREFIX: &[u8] = b"fiber-lsp-sdk/signer/channel/";
const SIGNING_STATE_PREFIX: &[u8] = b"fiber-lsp-sdk/signer/signing-state/";
const MAX_ALLOCATION_ATTEMPTS: usize = 16;

#[derive(Debug, Deserialize, Serialize)]
struct SignerMetadata {
    format_version: u16,
    derivation_version: u16,
    root_public_key: Vec<u8>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredChannel {
    format_version: u16,
    derivation_version: u16,
    channel_key_id: ChannelKeyId,
    allocation_entropy: [u8; 32],
    #[serde(default)]
    binding: Option<ChannelBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredSignedRequest {
    intent: SigningIntent,
    content_hash: [u8; 32],
    signing_message: [u8; 32],
}

#[derive(Clone, Deserialize, Serialize)]
struct StoredNonce {
    seed: [u8; 32],
    public_nonce: PubNonce,
    bound_content: Option<[u8; 32]>,
}

impl Drop for StoredNonce {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.seed);
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct StoredSigningState {
    format_version: u16,
    opening_binding: Option<ChannelBinding>,
    commitment: Option<crate::commitment::CommitmentState>,
    revision: u64,
    nonces: Vec<StoredNonce>,
    signed_requests: Vec<StoredSignedRequest>,
}

impl StoredSigningState {
    fn validate_revision_and_nonce(&self, prepared: &SigningData) -> Result<(), SignerError> {
        if self.revision != prepared.state_revision {
            return Err(SignerError::SigningStateChanged);
        }
        if let ChannelSigningContent::Musig2(content) = &prepared.content {
            let binding = content
                .v2
                .as_ref()
                .ok_or_else(|| crate::commitment::invalid("missing V2 nonce allocation"))?;
            let nonce = self
                .nonces
                .iter()
                .find(|n| n.public_nonce == binding.public_nonce)
                .ok_or_else(|| {
                    crate::commitment::invalid("V2 nonce was not published by this signer")
                })?;
            if nonce
                .bound_content
                .is_some_and(|hash| hash != prepared.review.content_hash)
            {
                return Err(crate::commitment::invalid(
                    "V2 nonce already bound to another exact signing context",
                ));
            }
        }
        Ok(())
    }
}

impl Default for StoredSigningState {
    fn default() -> Self {
        Self {
            opening_binding: None,
            commitment: None,
            format_version: 6,
            revision: 0,
            nonces: Vec::new(),
            signed_requests: Vec::new(),
        }
    }
}

/// Result of securely generating and initializing a new root signer.
pub struct CreatedRootSigner<S> {
    /// Initialized root signer SDK.
    pub root_signer: RootSigner<S>,
    /// Explicit backup required to reopen the signer after restart.
    pub root_key_backup: RootKeyBackup,
}

/// User identity and factory for signer-owned channel key bundles.
pub struct RootSigner<S> {
    root_key: RootKey,
    store: S,
}

impl<S> fmt::Debug for RootSigner<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootSigner")
            .field("identity_public_key", &self.root_key.public_key())
            .field("store", &std::any::type_name::<S>())
            .finish_non_exhaustive()
    }
}

impl<S: SignerStore> RootSigner<S> {
    /// Generate a root identity key and initialize an empty store.
    pub async fn create_random(store: S) -> Result<CreatedRootSigner<S>, SignerError> {
        let root_key = RootKey::generate()?;
        let root_key_backup = RootKeyBackup::new(*root_key.secret_bytes());
        let root_signer = Self::create(root_key, store).await?;
        Ok(CreatedRootSigner {
            root_signer,
            root_key_backup,
        })
    }

    /// Initialize an empty store with an externally supplied root identity key.
    pub async fn create(root_key: RootKey, store: S) -> Result<Self, SignerError> {
        let metadata = SignerMetadata {
            format_version: STORE_FORMAT_VERSION,
            derivation_version: DERIVATION_VERSION,
            root_public_key: root_key.public_key().serialize().to_vec(),
        };
        let inserted = store
            .insert_if_absent(METADATA_KEY, &encode(&metadata)?)
            .await
            .map_err(store_error)?;
        if !inserted {
            return Err(SignerError::AlreadyInitialized);
        }
        Ok(Self { root_key, store })
    }

    /// Open an initialized store using its root identity key.
    pub async fn open(root_key: RootKey, store: S) -> Result<Self, SignerError> {
        let encoded = store
            .get(METADATA_KEY)
            .await
            .map_err(store_error)?
            .ok_or(SignerError::NotInitialized)?;
        let metadata: SignerMetadata = decode(&encoded)?;
        validate_versions(metadata.format_version, metadata.derivation_version)?;
        if metadata.root_public_key.as_slice() != root_key.public_key().serialize() {
            return Err(SignerError::RootKeyMismatch);
        }
        Ok(Self { root_key, store })
    }

    /// Public key representing this signer identity.
    pub fn identity_public_key(&self) -> PublicKey {
        self.root_key.public_key()
    }

    /// Sign the narrow, canonical proof used for one tenant registration.
    pub fn sign_tenant_registry_payload(
        &self,
        payload: &fiber_types::TenantRegistryPayload,
    ) -> Result<fiber_types::TenantRegistrySignature, SignerError> {
        if PublicKey::from(payload.root_signer_pubkey) != self.identity_public_key() {
            return Err(SignerError::InvalidContent(
                "tenant registration payload names another RootSigner".to_string(),
            ));
        }
        Ok(self.root_key.sign_tenant_registry_payload(payload))
    }

    /// Create and durably record a fresh signer-owned channel key bundle.
    pub async fn create_channel(&self) -> Result<ChannelSigner<S>, SignerError> {
        for _ in 0..MAX_ALLOCATION_ATTEMPTS {
            let mut allocation_entropy = [0u8; 32];
            getrandom::fill(&mut allocation_entropy)
                .map_err(|error| SignerError::Random(error.to_string()))?;
            let (channel_key_id, key_material) = self.derive_channel(&allocation_entropy);
            let record = StoredChannel {
                format_version: STORE_FORMAT_VERSION,
                derivation_version: DERIVATION_VERSION,
                channel_key_id,
                allocation_entropy,
                binding: None,
            };
            let inserted = self
                .store
                .insert_if_absent(&channel_store_key(channel_key_id), &encode(&record)?)
                .await
                .map_err(store_error)?;
            if inserted {
                return Ok(ChannelSigner {
                    channel_key_id,
                    key_material,
                    store: self.store.clone(),
                });
            }
        }
        Err(SignerError::Random(
            "failed to allocate a unique channel key id".to_string(),
        ))
    }

    /// Reopen a previously created channel signer.
    pub async fn open_channel(
        &self,
        channel_key_id: ChannelKeyId,
    ) -> Result<ChannelSigner<S>, SignerError> {
        let encoded = self
            .store
            .get(&channel_store_key(channel_key_id))
            .await
            .map_err(store_error)?
            .ok_or(SignerError::UnknownChannelKey(channel_key_id))?;
        let record: StoredChannel = decode(&encoded)?;
        validate_versions(record.format_version, record.derivation_version)?;
        if record.channel_key_id != channel_key_id {
            return Err(SignerError::CorruptStore(
                "channel key id does not match its store key".to_string(),
            ));
        }
        let (derived_key_id, key_material) = self.derive_channel(&record.allocation_entropy);
        if derived_key_id != record.channel_key_id {
            return Err(SignerError::CorruptStore(
                "channel key id does not match its derivation".to_string(),
            ));
        }
        Ok(ChannelSigner {
            channel_key_id,
            key_material,
            store: self.store.clone(),
        })
    }

    fn derive_channel(&self, allocation_entropy: &[u8; 32]) -> (ChannelKeyId, InMemorySigner) {
        let channel_root = Zeroizing::new(blake2b_hash_with_salt(
            self.root_key.secret_bytes(),
            b"FIBER_SIGNER_CHANNEL_ROOT_KEY",
        ));
        let mut allocation_input = Zeroizing::new(Vec::with_capacity(64));
        allocation_input.extend_from_slice(&channel_root[..]);
        allocation_input.extend_from_slice(allocation_entropy);
        let signer_seed = Zeroizing::new(blake2b_hash_with_salt(
            &allocation_input[..],
            b"FIBER_SIGNER_CHANNEL_KEY",
        ));
        let channel_key_id = ChannelKeyId(
            blake2b_hash_with_salt(&signer_seed[..], b"FIBER_SIGNER_CHANNEL_KEY_ID").into(),
        );
        (
            channel_key_id,
            InMemorySigner::generate_from_seed(&signer_seed[..]),
        )
    }
}

impl RootSigner<MemoryStore> {
    /// Create an ephemeral root signer using the built-in in-memory store.
    pub async fn in_memory() -> Result<CreatedRootSigner<MemoryStore>, SignerError> {
        Self::create_random(MemoryStore::default()).await
    }
}

/// Signer scoped to exactly one channel key bundle.
///
/// The Fiber `InMemorySigner` is deliberately kept as a private implementation
/// detail so no private channel key crosses the SDK boundary.
pub struct ChannelSigner<S> {
    channel_key_id: ChannelKeyId,
    key_material: InMemorySigner,
    store: S,
}

impl<S> fmt::Debug for ChannelSigner<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelSigner")
            .field("channel_key_id", &self.channel_key_id)
            .field("public_material", &self.public_material())
            .field("store", &std::any::type_name::<S>())
            .finish()
    }
}

impl<S> ChannelSigner<S> {
    /// Opaque identifier used to reopen this channel signer.
    pub fn channel_key_id(&self) -> ChannelKeyId {
        self.channel_key_id
    }

    /// Public material for this channel key bundle.
    pub fn public_material(&self) -> ChannelPublicMaterial {
        ChannelPublicMaterial {
            channel_key_id: self.channel_key_id,
            base_public_keys: self.key_material.get_base_public_keys(),
        }
    }

    /// Derive one public commitment point without exposing secret material.
    pub fn get_commitment_point(&self, commitment_number: u64) -> Pubkey {
        self.key_material.get_commitment_point(commitment_number)
    }
}

impl<S: SignerStore> ChannelSigner<S> {
    /// Public material Fiber needs to send `OpenChannel` without holding channel keys.
    pub async fn channel_open_material(
        &self,
        public: bool,
    ) -> Result<ChannelOpenSignerMaterial, SignerError> {
        let mut opening_key = channel_store_key(self.channel_key_id);
        opening_key.extend_from_slice(if public {
            b"/opening/public"
        } else {
            b"/opening/private"
        });
        if let Some(encoded) = self.store.get(&opening_key).await.map_err(store_error)? {
            return decode(&encoded);
        }
        let commitment_nonce = self
            .get_musig2_nonce(NonceSlot {
                purpose: NoncePurpose::Commitment,
                commitment_number: 0,
            })
            .await?;
        let next_commitment_nonce = self
            .get_musig2_nonce(NonceSlot {
                purpose: NoncePurpose::Commitment,
                commitment_number: 1,
            })
            .await?;
        let revocation_nonce = self
            .get_musig2_nonce(NonceSlot {
                purpose: NoncePurpose::Revocation,
                commitment_number: 2,
            })
            .await?;
        let channel_announcement_nonce = if public {
            Some(
                self.get_musig2_nonce(NonceSlot {
                    purpose: NoncePurpose::ChannelAnnouncement,
                    commitment_number: 0,
                })
                .await?,
            )
        } else {
            None
        };
        let mut v2_nonces = Vec::new();
        for _ in 0..64 {
            v2_nonces.push(
                self.get_musig2_nonce(NonceSlot {
                    purpose: NoncePurpose::Commitment,
                    commitment_number: 1,
                })
                .await?
                .public_nonce,
            );
        }
        let material = ChannelOpenSignerMaterial {
            v2_nonces,
            base_public_keys: self.public_material().base_public_keys,
            first_commitment_point: self.get_commitment_point(1),
            second_commitment_point: self.get_commitment_point(2),
            commitment_nonce: commitment_nonce.public_nonce,
            next_commitment_nonce: next_commitment_nonce.public_nonce,
            revocation_nonce: revocation_nonce.public_nonce,
            channel_announcement_nonce: channel_announcement_nonce.map(|nonce| nonce.public_nonce),
        };
        // Opening retries must advertise the same already-persisted fresh nonce pool.
        if self
            .store
            .insert_if_absent(&opening_key, &encode(&material)?)
            .await
            .map_err(store_error)?
        {
            return Ok(material);
        }
        let encoded = self
            .store
            .get(&opening_key)
            .await
            .map_err(store_error)?
            .ok_or(SignerError::SigningStateChanged)?;
        decode(&encoded)
    }

    pub(crate) async fn next_material_for_request(
        &self,
        slot: NonceSlot,
        content_hash: [u8; 32],
    ) -> Result<NextChannelSignerMaterial, SignerError> {
        let mut key = channel_store_key(self.channel_key_id);
        key.extend_from_slice(b"/response/");
        key.extend_from_slice(&content_hash);
        if let Some(encoded) = self.store.get(&key).await.map_err(store_error)? {
            return decode(&encoded);
        }
        let material = self.next_material(slot).await?;
        // A lost RPC response must retry the exact public refill, not mint another one.
        if self
            .store
            .insert_if_absent(&key, &encode(&material)?)
            .await
            .map_err(store_error)?
        {
            return Ok(material);
        }
        let encoded = self
            .store
            .get(&key)
            .await
            .map_err(store_error)?
            .ok_or(SignerError::SigningStateChanged)?;
        decode(&encoded)
    }

    /// Public material for the next commitment number after `slot`.
    pub async fn next_material(
        &self,
        slot: NonceSlot,
    ) -> Result<NextChannelSignerMaterial, SignerError> {
        let commitment_number = slot.commitment_number.saturating_add(1);

        let mut v2_nonces = Vec::new();
        for _ in 0..1 {
            v2_nonces.push(
                self.get_musig2_nonce(NonceSlot {
                    purpose: NoncePurpose::Commitment,
                    commitment_number,
                })
                .await?
                .public_nonce,
            );
        }
        let commitment_points = (commitment_number..commitment_number.saturating_add(4))
            .map(|n| (n, self.get_commitment_point(n)))
            .collect();
        Ok(NextChannelSignerMaterial {
            v2_nonces,
            commitment_points,
            next_commitment_point: Some(self.get_commitment_point(commitment_number)),
            next_commitment_nonce: None,
            next_revocation_nonce: None,
        })
    }
}

impl<S: SignerStore> ChannelSigner<S> {
    /// Obtain and record a Fiber-compatible public nonce for a typed nonce slot.
    pub async fn get_musig2_nonce(&self, slot: NonceSlot) -> Result<Musig2Nonce, SignerError> {
        validate_nonce_slot(slot)?;
        let mut seed = [0; 32];
        getrandom::fill(&mut seed).map_err(|e| SignerError::Random(e.to_string()))?;
        let public_nonce = SecNonceBuilder::new(seed)
            .with_seckey(&self.key_material.funding_key)
            .build()
            .public_nonce();
        loop {
            let (encoded, mut state) = self.load_signing_state().await?;
            if state.nonces.iter().any(|n| n.public_nonce == public_nonce) {
                return Err(crate::commitment::invalid("duplicate random V2 nonce"));
            }
            state.nonces.push(StoredNonce {
                seed,
                public_nonce: public_nonce.clone(),
                bound_content: None,
            });
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or_else(|| crate::commitment::invalid("revision overflow"))?;
            if self.store_signing_state(encoded.as_deref(), &state).await? {
                return Ok(Musig2Nonce { public_nonce });
            }
        }
    }

    async fn persist_binding(&self, binding: ChannelBinding) -> Result<(), SignerError> {
        loop {
            let encoded = self
                .store
                .get(&channel_store_key(self.channel_key_id))
                .await
                .map_err(store_error)?
                .ok_or(SignerError::UnknownChannelKey(self.channel_key_id))?;
            let mut record: StoredChannel = decode(&encoded)?;
            validate_versions(record.format_version, record.derivation_version)?;
            match &record.binding {
                Some(existing) if existing == &binding => return Ok(()),
                Some(_) => return Err(SignerError::ChannelAlreadyBound),
                None => {
                    record.binding = Some(binding.clone());
                    if self
                        .store
                        .compare_and_swap(
                            &channel_store_key(self.channel_key_id),
                            Some(encoded.as_slice()),
                            &encode(&record)?,
                        )
                        .await
                        .map_err(store_error)?
                    {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Reject context-free signing requests. Use the intent-specific preparation method
    /// (`prepare_commitment`, `prepare_revocation`, `prepare_close`,
    /// `prepare_announcement` or `prepare_onchain`) with its verification evidence.
    pub async fn prepare(
        &self,
        content: ChannelSigningContent,
    ) -> Result<PreparedSigning, SignerError> {
        let _ = content;
        Err(crate::commitment::invalid(
            "signing requires the intent-specific verifier and complete context",
        ))
    }

    async fn prepare_inner(
        &self,
        content: ChannelSigningContent,
    ) -> Result<SigningData, SignerError> {
        let binding = self
            .load_binding()
            .await?
            .ok_or(SignerError::ChannelNotBound)?;
        validate_content_against_binding(&content, &binding)?;
        validate_signing_content(&content)?;
        let signing_message = content.signing_message();
        let canonical_content = content.canonical_bytes();
        let content_hash = content
            .content_hash(&canonical_content)
            .map_err(SignerError::InvalidContent)?;
        let (_, state) = self.load_signing_state().await?;
        // V2 nonce reuse is a hard error enforced by the persisted allocation
        // ledger, never a permissive review warning. Owner lanes are independent.
        let warnings = Vec::new();
        let review = SigningReview {
            intent: content.intent(),
            commitment_counter: content.commitment_counter(),
            commitment_number: content.nonce_slot().map(|slot| slot.commitment_number),
            signing_message,
            content_hash,
            canonical_content,
            warnings,
        };
        Ok(SigningData {
            channel_key_id: self.channel_key_id,
            state_revision: state.revision,
            content,
            review,
        })
    }

    /// Validate wallet-approved opening terms and install their baseline and funding identity.
    /// The funding wallet must approve the complete transaction and selected inputs.
    /// Contract, keys, fee and balances come from the wallet's opening approval.
    /// JSON-RPC clients should use `HostedSession::open_tenant_channel` (which calls the generic
    /// `open_channel_with_external_funding` RPC) to require and validate `opening_context`
    /// against their original request and trusted network configuration.
    pub async fn approve_opening(
        &self,
        funding: &ckb_types::packed::Transaction,
        funding_output_index: u32,
        local_shutdown_script: ckb_types::packed::Script,
        expected_input_outpoints: &[ckb_types::packed::OutPoint],
        parameters: crate::CommitmentParameters,
    ) -> Result<(), SignerError> {
        use ckb_types::prelude::*;
        let binding = approved_funding_identity(
            funding,
            funding_output_index,
            local_shutdown_script,
            expected_input_outpoints,
        )?;
        if self
            .load_binding()
            .await?
            .is_some_and(|existing| existing != binding)
        {
            return Err(SignerError::ChannelAlreadyBound);
        }
        if funding.calc_tx_hash() != binding.funding_outpoint.tx_hash() {
            return Err(crate::commitment::invalid(
                "opening transaction differs from bound funding",
            ));
        }
        let index: u32 = binding.funding_outpoint.index().unpack();
        let output = funding
            .raw()
            .outputs()
            .get(index as usize)
            .ok_or_else(|| crate::commitment::invalid("missing funding output"))?;
        let data = funding
            .raw()
            .outputs_data()
            .get(index as usize)
            .ok_or_else(|| crate::commitment::invalid("missing funding output data"))?
            .raw_data()
            .to_vec();
        let ctx = musig2::KeyAggContext::new([
            self.key_material.funding_key.pubkey(),
            parameters.remote_funding_key,
        ])
        .map_err(|e| crate::commitment::invalid(&e.to_string()))?;
        // Funding aggregation may use either ordering depending on which peer opened.
        let reversed = musig2::KeyAggContext::new([
            parameters.remote_funding_key,
            self.key_material.funding_key.pubkey(),
        ])
        .map_err(|e| crate::commitment::invalid(&e.to_string()))?;
        if !musig2_matches_approved_lock(&ctx, &binding.funding_lock_script)
            && !musig2_matches_approved_lock(&reversed, &binding.funding_lock_script)
        {
            return Err(crate::commitment::invalid(
                "approved remote funding key does not match funding lock",
            ));
        }
        let commitment = crate::commitment::CommitmentState::new(parameters, output, data)?;
        loop {
            let (encoded, mut state) = self.load_signing_state().await?;
            if let Some(existing) = &state.commitment {
                return if existing.parameters == commitment.parameters
                    && state.opening_binding.as_ref() == Some(&binding)
                {
                    self.persist_binding(binding).await
                } else {
                    Err(crate::commitment::invalid(
                        "approved opening terms cannot be replaced",
                    ))
                };
            }
            if state
                .signed_requests
                .iter()
                .any(|r| r.intent == SigningIntent::CommitmentTransaction)
            {
                return Err(crate::commitment::invalid(
                    "legacy signed channel requires trusted state recovery",
                ));
            }
            state.commitment = Some(commitment.clone());
            state.opening_binding = Some(binding.clone());
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or_else(|| crate::commitment::invalid("state revision overflow"))?;
            if self.store_signing_state(encoded.as_deref(), &state).await? {
                return self.persist_binding(binding).await;
            }
        }
    }

    /// Persist a wallet-approved outgoing payment in this channel's asset.
    /// For invoices compare the returned invoice against the original local creation terms.
    pub async fn authorize_payment(
        &self,
        authorization: crate::PaymentAuthorization,
    ) -> Result<(), SignerError> {
        if authorization.inbound {
            return Err(crate::commitment::invalid(
                "inbound authorization requires authorize_invoice and a matching preimage",
            ));
        }
        self.update_commitment(|state| state.authorize(authorization.clone()))
            .await
    }

    /// Register a locally created invoice only after proving possession of its preimage.
    /// The preimage is checked but is neither persisted by the SDK nor sent to the LSP.
    /// This is not authorization to fulfill or release it; call `authorize_fulfillment` later.
    pub async fn authorize_invoice(
        &self,
        authorization: crate::PaymentAuthorization,
        preimage: fiber_types::Hash256,
    ) -> Result<(), SignerError> {
        if !authorization.inbound
            || authorization.hash_algorithm.hash(preimage.as_ref())
                != *authorization.payment_hash.as_ref()
        {
            return Err(crate::commitment::invalid(
                "invoice direction or preimage does not match authorization",
            ));
        }
        self.update_commitment(|state| state.authorize(authorization.clone()))
            .await
    }

    /// Record a verified preimage before allowing a TLC to settle into its recipient's balance.
    /// This records intent only: it does not prove a commitment is enforceable or release the
    /// preimage to the LSP. The application must separately enforce safe preimage release.
    pub async fn authorize_fulfillment(
        &self,
        payment_hash: fiber_types::Hash256,
        preimage: fiber_types::Hash256,
    ) -> Result<(), SignerError> {
        self.update_commitment(|state| state.resolve(payment_hash, Some(preimage)))
            .await
    }

    /// Approve refunding an unresolved TLC. Cannot override an approved fulfillment.
    pub async fn authorize_cancellation(
        &self,
        payment_hash: fiber_types::Hash256,
    ) -> Result<(), SignerError> {
        self.update_commitment(|state| state.resolve(payment_hash, None))
            .await
    }

    async fn update_commitment(
        &self,
        update: impl Fn(&mut crate::commitment::CommitmentState) -> Result<(), SignerError>,
    ) -> Result<(), SignerError> {
        loop {
            let (encoded, mut state) = self.load_signing_state().await?;
            update(
                state
                    .commitment
                    .as_mut()
                    .ok_or_else(|| crate::commitment::invalid("missing approved opening terms"))?,
            )?;
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or_else(|| crate::commitment::invalid("state revision overflow"))?;
            if self.store_signing_state(encoded.as_deref(), &state).await? {
                return Ok(());
            }
        }
    }

    /// Validate every commitment against locally approved terms, keys, payment authorizations
    /// and both persisted commitment lanes. `now_ms` must come from the wallet's trusted clock.
    /// The returned immutable preparation binds validation and signing to one store revision.
    pub async fn prepare_commitment(
        &self,
        content: ChannelSigningContent,
        context: crate::CommitmentContext,
        now_ms: u64,
    ) -> Result<PreparedSigning, SignerError> {
        let prepared = self.prepare_inner(content).await?;
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared)?;
        let mut commitment = state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing approved opening terms"))?;
        let binding = self
            .load_binding()
            .await?
            .ok_or(SignerError::ChannelNotBound)?;
        crate::lifecycle::validate_session(
            &prepared.content,
            &context.session,
            context.transition,
            &commitment.parameters,
            &self.key_material,
        )?;
        let review = commitment.validate(
            &prepared.content,
            &context,
            &binding,
            &self.key_material,
            now_ms,
        )?;
        commitment
            .lifecycle
            .record_commitment(&prepared.content, &context, &review)?;
        Ok(PreparedSigning {
            data: prepared,
            validation: PreparedValidation::Commitment {
                state: commitment,
                review,
                context,
                validated_at_ms: now_ms,
            },
        })
    }

    /// Validate a revocation against the old state and a safely persisted successor.
    pub async fn prepare_revocation(
        &self,
        content: ChannelSigningContent,
        context: crate::RevocationContext,
        now_ms: u64,
    ) -> Result<PreparedSigning, SignerError> {
        let prepared = self.prepare_inner(content).await?;
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared)?;
        let mut commitment = state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?;
        let binding = self
            .load_binding()
            .await?
            .ok_or(SignerError::ChannelNotBound)?;
        commitment.validate_revocation(
            &prepared.content,
            &context,
            &binding,
            &self.key_material,
            now_ms,
        )?;
        Ok(PreparedSigning {
            data: prepared,
            validation: PreparedValidation::Revocation {
                state: commitment,
                context,
                validated_at_ms: now_ms,
            },
        })
    }

    /// Revalidate revocation time and state immediately before signing.
    pub async fn sign_revocation(
        &self,
        prepared: PreparedSigning,
        now_ms: u64,
    ) -> Result<ChannelSignature, SignerError> {
        if prepared.data.channel_key_id != self.channel_key_id {
            return Err(SignerError::PreparedForAnotherChannel);
        }
        let PreparedValidation::Revocation {
            context,
            validated_at_ms,
            ..
        } = prepared.validation
        else {
            return Err(crate::commitment::invalid("expected verified revocation"));
        };
        if now_ms < validated_at_ms {
            return Err(crate::commitment::invalid("wallet clock moved backwards"));
        }
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared.data)?;
        let fresh = self
            .prepare_revocation(prepared.data.content, context, now_ms)
            .await?;
        if fresh.data.state_revision != state.revision {
            return Err(SignerError::SigningStateChanged);
        }
        self.sign_verified(fresh).await
    }

    /// Retrieve durable signed commitment evidence for independent recovery.
    pub async fn recovery_records(&self) -> Result<Vec<crate::RecoveryRecord>, SignerError> {
        let (_, state) = self.load_signing_state().await?;
        Ok(state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?
            .lifecycle
            .commitments)
    }

    /// Retrieve verified revocation records and complete punishment signatures for recovery.
    pub async fn revocation_records(&self) -> Result<Vec<crate::RevocationRecord>, SignerError> {
        let (_, state) = self.load_signing_state().await?;
        Ok(state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?
            .lifecycle
            .revocations)
    }

    /// Return whether the channel has a locally verified, completely signed opening
    /// commitment transaction on the local lane. Wallets MUST verify this returns `true`
    /// before signing or broadcasting the external funding transaction.
    pub async fn has_verified_opening_exit(&self) -> Result<bool, SignerError> {
        let (_, state) = self.load_signing_state().await?;
        let Some(commitment) = state.commitment.as_ref() else {
            return Ok(false);
        };
        Ok(commitment.lifecycle.commitments.iter().any(|record| {
            !record.reference.for_remote
                && record.reference.version == 1
                && record.complete_signature.is_some()
        }))
    }

    /// Return the channel's on-chain recovery safety margin (2/3 of the commitment delay
    /// epoch in milliseconds), or `None` if opening terms have not been persisted yet.
    pub async fn safety_margin_ms(&self) -> Result<Option<u64>, SignerError> {
        let (_, state) = self.load_signing_state().await?;
        state
            .commitment
            .as_ref()
            .map(|commitment| commitment.parameters.safety_margin_ms())
            .transpose()
    }

    /// Check and durably record permission to release a preimage at the current wallet time.
    /// The application must release immediately; the trusted chain source verifies live funding
    /// and on-chain median time so a lagging local clock cannot weaken the TLC safety window.
    /// This API neither transmits nor persists the preimage.
    pub async fn authorize_preimage_release<C: crate::ChainVerifier>(
        &self,
        hash: fiber_types::Hash256,
        preimage: fiber_types::Hash256,
        now_ms: u64,
        chain: &C,
    ) -> Result<crate::CommitmentReference, SignerError> {
        let (encoded, mut state) = self.load_signing_state().await?;
        let binding = self
            .load_binding()
            .await?
            .ok_or(SignerError::ChannelNotBound)?;
        let live = chain.live_cell(&binding.funding_outpoint).await?;
        let chain_time_ms = chain.median_time_ms().await?;
        let effective_now_ms = now_ms.max(chain_time_ms);
        let commitment = state
            .commitment
            .as_mut()
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?;
        if live.output != commitment.funding_output || live.data != commitment.funding_data {
            return Err(crate::commitment::invalid(
                "funding is not live with approved asset and capacity",
            ));
        }
        let reference = commitment.release_preimage(hash, preimage, effective_now_ms)?;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| crate::commitment::invalid("revision overflow"))?;
        if !self.store_signing_state(encoded.as_deref(), &state).await? {
            return Err(SignerError::SigningStateChanged);
        }
        Ok(reference)
    }

    /// Approve an exact cooperative-close fee allocation locally.
    pub async fn authorize_close(
        &self,
        terms: crate::CloseAuthorization,
    ) -> Result<(), SignerError> {
        if terms.local_fee_share.checked_add(terms.remote_fee_share) != Some(terms.fee) {
            return Err(crate::commitment::invalid("invalid close fee allocation"));
        }
        self.update_commitment(|state| {
            state.lifecycle.close = Some(terms.clone());
            Ok(())
        })
        .await
    }

    /// Validate cooperative-close outputs and nonce session against both tracked balances.
    pub async fn prepare_close(
        &self,
        content: ChannelSigningContent,
        session: crate::SigningSessionEvidence,
    ) -> Result<PreparedSigning, SignerError> {
        let prepared = self.prepare_inner(content).await?;
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared)?;
        let commitment = state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?;
        let binding = self
            .load_binding()
            .await?
            .ok_or(SignerError::ChannelNotBound)?;
        crate::lifecycle::validate_session(
            &prepared.content,
            &session,
            fiber_types::ChannelSigningTransition::SendClosingSigned,
            &commitment.parameters,
            &self.key_material,
        )?;
        commitment.validate_close(&prepared.content, &binding)?;
        Ok(PreparedSigning {
            data: prepared,
            validation: PreparedValidation::Close { state: commitment },
        })
    }

    /// Approve exact public announcement fields from wallet-owned network/channel configuration.
    /// This is local authorization, never a copy of an unreviewed LSP signing request.
    pub async fn approve_announcement(
        &self,
        announcement: fiber_types::ChannelAnnouncement,
    ) -> Result<(), SignerError> {
        let binding = self
            .load_binding()
            .await?
            .ok_or(SignerError::ChannelNotBound)?;
        let (_, state) = self.load_signing_state().await?;
        let commitment = state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?;
        let mut keys = [
            self.key_material.funding_key.pubkey(),
            commitment.parameters.remote_funding_key,
        ];
        keys.sort();
        let ctx = musig2::KeyAggContext::new(keys)
            .map_err(|e| crate::commitment::invalid(&e.to_string()))?;
        let point: musig2::secp::Point = ctx.aggregated_pubkey();
        // Announcements advertise liquid capacity, excluding the CKB reserves.
        let total = commitment
            .parameters
            .local_amount
            .checked_add(commitment.parameters.remote_amount)
            .ok_or_else(|| crate::commitment::invalid("capacity overflow"))?;
        let capacity = if commitment.funding_output.type_().to_opt().is_none() {
            total
                .checked_sub(u128::from(commitment.parameters.local_reserved_ckb_amount))
                .and_then(|n| {
                    n.checked_sub(u128::from(commitment.parameters.remote_reserved_ckb_amount))
                })
                .ok_or_else(|| crate::commitment::invalid("reserves exceed capacity"))?
        } else {
            total
        };
        if announcement.channel_outpoint != binding.funding_outpoint
            || announcement.ckb_key.serialize() != point.serialize_xonly()
            || announcement.udt_type_script != commitment.funding_output.type_().to_opt()
            || announcement.capacity != capacity
        {
            return Err(crate::commitment::invalid(
                "announcement does not match approved funding",
            ));
        }
        let content = Musig2SignableContent::ChannelAnnouncement(announcement);
        let bytes = content.canonical_bytes();
        self.update_commitment(|state| {
            if state
                .lifecycle
                .announcement
                .as_ref()
                .is_some_and(|old| *old != bytes)
            {
                return Err(crate::commitment::invalid(
                    "announcement terms already approved",
                ));
            }
            state.lifecycle.announcement = Some(bytes.clone());
            Ok(())
        })
        .await
    }

    /// Verify announcement fields and nonce against local approval.
    pub async fn prepare_announcement(
        &self,
        content: ChannelSigningContent,
        session: crate::SigningSessionEvidence,
    ) -> Result<PreparedSigning, SignerError> {
        let prepared = self.prepare_inner(content).await?;
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared)?;
        let commitment = state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?;
        crate::lifecycle::validate_session(
            &prepared.content,
            &session,
            fiber_types::ChannelSigningTransition::SignChannelAnnouncement,
            &commitment.parameters,
            &self.key_material,
        )?;
        if commitment.lifecycle.announcement.as_ref() != Some(&prepared.content.canonical_bytes()) {
            return Err(crate::commitment::invalid(
                "announcement differs from approved network/channel fields",
            ));
        }
        Ok(PreparedSigning {
            data: prepared,
            validation: PreparedValidation::Announcement { state: commitment },
        })
    }

    /// Prepare an on-chain claim against local history and an independent trusted chain source.
    pub async fn prepare_onchain<C: crate::ChainVerifier>(
        &self,
        content: crate::OnchainSigningContent,
        authorization: crate::OnchainSpendAuthorization,
        chain: &C,
    ) -> Result<PreparedSigning, SignerError> {
        let prepared = self
            .prepare_inner(ChannelSigningContent::Onchain(content.clone()))
            .await?;
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared)?;
        let commitment = state
            .commitment
            .as_ref()
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?;
        let record = commitment.lifecycle.find(&authorization.source)?;
        crate::onchain::validate(&content, &authorization, record, chain).await?;
        Ok(PreparedSigning {
            data: prepared,
            validation: PreparedValidation::Onchain { authorization },
        })
    }

    /// Recheck live inputs and chain maturity immediately before signing an on-chain claim.
    pub async fn sign_onchain<C: crate::ChainVerifier>(
        &self,
        prepared: PreparedSigning,
        chain: &C,
    ) -> Result<ChannelSignature, SignerError> {
        if prepared.data.channel_key_id != self.channel_key_id {
            return Err(SignerError::PreparedForAnotherChannel);
        }
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared.data)?;
        let ChannelSigningContent::Onchain(content) = &prepared.data.content else {
            return Err(crate::commitment::invalid("expected on-chain claim"));
        };
        let PreparedValidation::Onchain { authorization } = &prepared.validation else {
            return Err(crate::commitment::invalid(
                "expected verified on-chain claim",
            ));
        };
        let commitment = state
            .commitment
            .as_ref()
            .ok_or_else(|| crate::commitment::invalid("missing opening terms"))?;
        crate::onchain::validate(
            content,
            authorization,
            commitment.lifecycle.find(&authorization.source)?,
            chain,
        )
        .await?;
        self.sign_verified(prepared).await
    }

    /// Sign a verified close or announcement after review. Other operations require
    /// their intent-specific signing method to recheck time or chain data.
    pub async fn sign(&self, prepared: PreparedSigning) -> Result<ChannelSignature, SignerError> {
        if !matches!(
            prepared.validation,
            PreparedValidation::Close { .. } | PreparedValidation::Announcement { .. }
        ) {
            return Err(crate::commitment::invalid(
                "use the intent-specific signing method with verified context",
            ));
        }
        self.sign_verified(prepared).await
    }

    /// Recheck commitment context and expiry at approval time, then atomically persist the
    /// validated state with the signature history. Use the wallet's clock, not LSP time.
    pub async fn sign_commitment(
        &self,
        prepared: PreparedSigning,
        now_ms: u64,
    ) -> Result<ChannelSignature, SignerError> {
        let PreparedValidation::Commitment {
            context,
            validated_at_ms,
            ..
        } = &prepared.validation
        else {
            return Err(crate::commitment::invalid("expected verified commitment"));
        };
        if now_ms < *validated_at_ms {
            return Err(crate::commitment::invalid(
                "wallet clock moved backwards since review",
            ));
        }
        let (_, state) = self.load_signing_state().await?;
        state.validate_revision_and_nonce(&prepared.data)?;
        let mut commitment = state
            .commitment
            .ok_or_else(|| crate::commitment::invalid("missing approved opening terms"))?;
        let binding = self
            .load_binding()
            .await?
            .ok_or(SignerError::ChannelNotBound)?;
        let review = commitment.validate(
            &prepared.data.content,
            context,
            &binding,
            &self.key_material,
            now_ms,
        )?;
        crate::lifecycle::validate_session(
            &prepared.data.content,
            &context.session,
            context.transition,
            &commitment.parameters,
            &self.key_material,
        )?;
        commitment
            .lifecycle
            .record_commitment(&prepared.data.content, context, &review)?;
        self.sign_inner(prepared.data, Some(commitment)).await
    }

    async fn sign_verified(
        &self,
        prepared: PreparedSigning,
    ) -> Result<ChannelSignature, SignerError> {
        let state = match prepared.validation {
            PreparedValidation::Commitment { state, .. }
            | PreparedValidation::Revocation { state, .. }
            | PreparedValidation::Close { state }
            | PreparedValidation::Announcement { state } => Some(state),
            PreparedValidation::Onchain { .. } => None,
        };
        self.sign_inner(prepared.data, state).await
    }

    async fn sign_inner(
        &self,
        prepared: SigningData,
        commitment_state: Option<crate::commitment::CommitmentState>,
    ) -> Result<ChannelSignature, SignerError> {
        if prepared.channel_key_id != self.channel_key_id {
            return Err(SignerError::PreparedForAnotherChannel);
        }
        validate_signing_content(&prepared.content)?;
        let signing_message = prepared.content.signing_message();
        let canonical_content = prepared.content.canonical_bytes();
        let content_hash = prepared
            .content
            .content_hash(&canonical_content)
            .map_err(SignerError::InvalidContent)?;
        if signing_message != prepared.review.signing_message
            || content_hash != prepared.review.content_hash
        {
            return Err(SignerError::InvalidContent(
                "prepared plaintext no longer matches its review".to_string(),
            ));
        }

        let (encoded, mut state) = self.load_signing_state().await?;
        let already_signed = state.signed_requests.iter().any(|request| {
            request.intent == prepared.content.intent()
                && request.content_hash == content_hash
                && request.signing_message == signing_message
        });
        if state.revision != prepared.state_revision && !already_signed {
            return Err(SignerError::SigningStateChanged);
        }

        let secret_nonce = if let ChannelSigningContent::Musig2(content) = &prepared.content {
            let binding = content
                .v2
                .as_ref()
                .ok_or_else(|| crate::commitment::invalid("missing V2 nonce binding"))?;
            let record = state
                .nonces
                .iter_mut()
                .find(|n| n.public_nonce == binding.public_nonce)
                .ok_or_else(|| {
                    crate::commitment::invalid("V2 nonce not allocated by this signer")
                })?;
            if record
                .bound_content
                .is_some_and(|hash| hash != content_hash)
            {
                return Err(crate::commitment::invalid(
                    "refusing V2 nonce reuse across signing contexts",
                ));
            }
            record.bound_content = Some(content_hash);
            Some(
                SecNonceBuilder::new(record.seed)
                    .with_seckey(&self.key_material.funding_key)
                    .build(),
            )
        } else {
            None
        };
        let signature = self.sign_content(&prepared.content, signing_message, secret_nonce)?;
        if already_signed {
            return Ok(signature);
        }

        if let Some(mut commitment) = commitment_state {
            if let ChannelSignature::Musig2(ref partial) = signature {
                commitment
                    .lifecycle
                    .complete(&prepared.content, partial.partial_signature)?;
            }
            state.commitment = Some(commitment);
        }
        state.signed_requests.push(StoredSignedRequest {
            intent: prepared.content.intent(),
            content_hash,
            signing_message,
        });
        state.revision = state.revision.checked_add(1).ok_or_else(|| {
            SignerError::CorruptStore("signing-state revision overflow".to_string())
        })?;
        if !self.store_signing_state(encoded.as_deref(), &state).await? {
            return Err(SignerError::SigningStateChanged);
        }
        Ok(signature)
    }

    fn sign_content(
        &self,
        content: &ChannelSigningContent,
        signing_message: [u8; 32],
        secret_nonce: Option<SecNonce>,
    ) -> Result<ChannelSignature, SignerError> {
        match content {
            ChannelSigningContent::Musig2(content) => {
                let partial_signature = sign_partial(
                    &content.key_agg_ctx,
                    &self.key_material.funding_key,
                    secret_nonce.ok_or_else(|| {
                        crate::commitment::invalid("missing fresh V2 secret nonce")
                    })?,
                    &content.agg_nonce,
                    signing_message,
                )
                .map_err(|error| SignerError::Signing(error.to_string()))?;
                Ok(ChannelSignature::Musig2(Musig2Signature {
                    partial_signature,
                }))
            }
            ChannelSigningContent::Onchain(content) => {
                let key = match content.key_purpose {
                    OnchainKeyPurpose::Settlement => self.key_material.tlc_base_key.clone(),
                    OnchainKeyPurpose::Tlc { commitment_number } => {
                        self.key_material.derive_tlc_key(commitment_number)
                    }
                };
                let signature = SECP256K1
                    .sign_ecdsa_recoverable(&Message::from_digest(signing_message), &key.0);
                let (recovery_id, compact) = signature.serialize_compact();
                let mut bytes = [0u8; 65];
                bytes[..64].copy_from_slice(&compact);
                bytes[64] = i32::from(recovery_id) as u8;
                Ok(ChannelSignature::Onchain(OnchainSignature {
                    signature: bytes,
                }))
            }
        }
    }

    async fn load_signing_state(
        &self,
    ) -> Result<(Option<Vec<u8>>, StoredSigningState), SignerError> {
        let encoded = self
            .store
            .get(&signing_state_store_key(self.channel_key_id))
            .await
            .map_err(store_error)?;
        let state = match encoded.as_deref() {
            Some(bytes) => {
                let version = bytes
                    .get(..2)
                    .map(|v| u16::from_le_bytes([v[0], v[1]]))
                    .ok_or_else(|| {
                        SignerError::CorruptStore("missing signing store version".to_string())
                    })?;
                match version {
                    6 => decode(bytes)?,
                    other => return Err(SignerError::UnsupportedStoreVersion(other)),
                }
            }
            None => StoredSigningState::default(),
        };
        Ok((encoded, state))
    }

    async fn store_signing_state(
        &self,
        expected: Option<&[u8]>,
        state: &StoredSigningState,
    ) -> Result<bool, SignerError> {
        self.store
            .compare_and_swap(
                &signing_state_store_key(self.channel_key_id),
                expected,
                &encode(state)?,
            )
            .await
            .map_err(store_error)
    }

    pub(crate) async fn load_binding(&self) -> Result<Option<ChannelBinding>, SignerError> {
        let encoded = self
            .store
            .get(&channel_store_key(self.channel_key_id))
            .await
            .map_err(store_error)?
            .ok_or(SignerError::UnknownChannelKey(self.channel_key_id))?;
        let record: StoredChannel = decode(&encoded)?;
        validate_versions(record.format_version, record.derivation_version)?;
        Ok(record.binding)
    }
}

fn validate_signing_content(content: &ChannelSigningContent) -> Result<(), SignerError> {
    let ChannelSigningContent::Musig2(content) = content else {
        return Ok(());
    };
    validate_nonce_slot(content.slot)?;
    let expected = content.content.expected_nonce_purpose();
    if content.slot.purpose != expected {
        return Err(SignerError::InvalidContent(format!(
            "{:?} plaintext requires {:?} nonce, got {:?}",
            content.content.intent(),
            expected,
            content.slot.purpose
        )));
    }
    if content.v2.is_none() || content.commitment_counter.is_some() {
        return Err(crate::commitment::invalid(
            "V2 requires an explicit fresh allocation, not a relative counter",
        ));
    }
    Ok(())
}

fn approved_funding_identity(
    unsigned_funding_tx: &ckb_types::packed::Transaction,
    funding_output_index: u32,
    local_shutdown_script: ckb_types::packed::Script,
    expected_input_outpoints: &[ckb_types::packed::OutPoint],
) -> Result<ChannelBinding, SignerError> {
    let raw = unsigned_funding_tx.raw();
    let inputs: Vec<ckb_types::packed::OutPoint> = raw
        .inputs()
        .into_iter()
        .map(|input| input.previous_output())
        .collect();
    if inputs != expected_input_outpoints {
        return Err(SignerError::InvalidContent(
            "approved funding transaction does not spend the expected cells".to_string(),
        ));
    }
    let output = raw
        .outputs()
        .get(funding_output_index as usize)
        .ok_or_else(|| {
            SignerError::InvalidContent(format!(
                "approved funding transaction has no output {funding_output_index}"
            ))
        })?;
    Ok(ChannelBinding {
        funding_outpoint: ckb_types::packed::OutPoint::new(
            unsigned_funding_tx.calc_tx_hash(),
            funding_output_index,
        ),
        funding_lock_script: output.lock(),
        local_shutdown_script,
    })
}

fn validate_content_against_binding(
    content: &ChannelSigningContent,
    binding: &ChannelBinding,
) -> Result<(), SignerError> {
    use ckb_types::prelude::Entity;
    match content {
        ChannelSigningContent::Musig2(content) => {
            match &content.content {
                Musig2SignableContent::CommitmentTransaction(transaction) => {
                    if !musig2_matches_approved_lock(
                        &content.key_agg_ctx,
                        &binding.funding_lock_script,
                    ) {
                        return Err(SignerError::InvalidContent(
                            "MuSig2 key aggregation does not match the approved funding lock"
                                .to_string(),
                        ));
                    }
                    if !transaction_spends(transaction, &binding.funding_outpoint) {
                        return Err(SignerError::InvalidContent(
                            "transaction does not spend the bound funding outpoint".to_string(),
                        ));
                    }
                }
                Musig2SignableContent::CooperativeCloseTransaction(transaction) => {
                    if !musig2_matches_approved_lock(
                        &content.key_agg_ctx,
                        &binding.funding_lock_script,
                    ) {
                        return Err(SignerError::InvalidContent(
                            "MuSig2 key aggregation does not match the approved funding lock"
                                .to_string(),
                        ));
                    }
                    if !transaction_spends(transaction, &binding.funding_outpoint) {
                        return Err(SignerError::InvalidContent(
                            "transaction does not spend the bound funding outpoint".to_string(),
                        ));
                    }
                    if !transaction_pays_script(transaction, &binding.local_shutdown_script) {
                        return Err(SignerError::InvalidContent(
                            "close transaction does not pay the bound local shutdown script"
                                .to_string(),
                        ));
                    }
                }
                // Revocation aggregates the same funding pubkeys in a
                // commitment-specific order and does not spend the funding cell.
                Musig2SignableContent::Revocation { .. } => {}
                Musig2SignableContent::ChannelAnnouncement(announcement) => {
                    if !musig2_matches_approved_lock(
                        &content.key_agg_ctx,
                        &binding.funding_lock_script,
                    ) {
                        return Err(SignerError::InvalidContent(
                            "MuSig2 key aggregation does not match the approved funding lock"
                                .to_string(),
                        ));
                    }
                    if announcement.channel_outpoint.as_slice()
                        != binding.funding_outpoint.as_slice()
                    {
                        return Err(SignerError::InvalidContent(
                            "channel announcement outpoint does not match the bound funding outpoint"
                                .to_string(),
                        ));
                    }
                }
            }
            Ok(())
        }
        ChannelSigningContent::Onchain(_) => {
            // Settlement and TLC spends pay commitment-derived locks, not the
            // cooperative-close shutdown script. Bound signing does not claim
            // to reconstruct those outputs.
            Ok(())
        }
    }
}

fn aggregated_funding_lock_args(ctx: &musig2::KeyAggContext) -> [u8; 20] {
    let point: musig2::secp::Point = ctx.aggregated_pubkey();
    let digest = blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
    let mut args = [0u8; 20];
    args.copy_from_slice(&digest[..20]);
    args
}

fn musig2_matches_approved_lock(
    ctx: &musig2::KeyAggContext,
    lock: &ckb_types::packed::Script,
) -> bool {
    lock.args().raw_data().as_ref() == aggregated_funding_lock_args(ctx)
}

fn transaction_spends(
    transaction: &ckb_types::packed::Transaction,
    outpoint: &ckb_types::packed::OutPoint,
) -> bool {
    use ckb_types::prelude::Entity;
    transaction
        .raw()
        .inputs()
        .into_iter()
        .any(|input| input.previous_output().as_slice() == outpoint.as_slice())
}

fn transaction_pays_script(
    transaction: &ckb_types::packed::Transaction,
    script: &ckb_types::packed::Script,
) -> bool {
    use ckb_types::prelude::Entity;
    transaction
        .raw()
        .outputs()
        .into_iter()
        .any(|output| output.lock().as_slice() == script.as_slice())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, SignerError> {
    bincode::serialize(value).map_err(|error| SignerError::CorruptStore(error.to_string()))
}

fn decode<T: DeserializeOwned>(value: &[u8]) -> Result<T, SignerError> {
    bincode::deserialize(value).map_err(|error| SignerError::CorruptStore(error.to_string()))
}

fn validate_versions(format: u16, derivation: u16) -> Result<(), SignerError> {
    if format != STORE_FORMAT_VERSION {
        return Err(SignerError::UnsupportedStoreVersion(format));
    }
    if derivation != DERIVATION_VERSION {
        return Err(SignerError::CorruptStore(format!(
            "unsupported channel derivation version: {derivation}"
        )));
    }
    Ok(())
}

fn channel_store_key(channel_key_id: ChannelKeyId) -> Vec<u8> {
    prefixed_channel_key(CHANNEL_KEY_PREFIX, channel_key_id)
}

fn signing_state_store_key(channel_key_id: ChannelKeyId) -> Vec<u8> {
    prefixed_channel_key(SIGNING_STATE_PREFIX, channel_key_id)
}

fn prefixed_channel_key(prefix: &[u8], channel_key_id: ChannelKeyId) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 32);
    key.extend_from_slice(prefix);
    key.extend_from_slice(channel_key_id.0.as_ref());
    key
}

fn store_error(error: impl std::fmt::Display) -> SignerError {
    SignerError::Store(error.to_string())
}

fn validate_nonce_slot(slot: NonceSlot) -> Result<(), SignerError> {
    if slot.purpose == NoncePurpose::ChannelAnnouncement && slot.commitment_number != 0 {
        return Err(SignerError::InvalidNonceSlot(
            "channel announcement must use commitment number zero".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_types::{Hash256, Musig2SigningContent, V2NonceBinding};
    use musig2::{AggNonce, KeyAggContext};

    fn slot(purpose: NoncePurpose) -> NonceSlot {
        NonceSlot {
            purpose,
            commitment_number: if matches!(
                purpose,
                NoncePurpose::Closing | NoncePurpose::ChannelAnnouncement
            ) {
                0
            } else {
                2
            },
        }
    }

    #[tokio::test]
    async fn opening_material_is_stable_across_concurrent_calls_and_restart() {
        let store = MemoryStore::default();
        let root = RootSigner::create(RootKey::import([23; 32]).unwrap(), store.clone())
            .await
            .unwrap();
        let signer = root.create_channel().await.unwrap();
        let other = root.open_channel(signer.channel_key_id()).await.unwrap();
        let (first, second) = tokio::join!(
            signer.channel_open_material(false),
            other.channel_open_material(false),
        );
        let material = first.unwrap();
        assert_eq!(material, second.unwrap());
        let restored = RootSigner::open(
            RootKey::import([23; 32]).unwrap(),
            MemoryStore::from_snapshot(&store.snapshot().unwrap()).unwrap(),
        )
        .await
        .unwrap();
        let reopened = restored
            .open_channel(signer.channel_key_id())
            .await
            .unwrap();
        assert_eq!(
            material,
            reopened.channel_open_material(false).await.unwrap()
        );
        assert_ne!(material.commitment_nonce, material.v2_nonces[0]);
    }

    #[tokio::test]
    async fn fresh_nonces_survive_restart_and_bind_exact_context() {
        let store = MemoryStore::default();
        let root = RootSigner::create(RootKey::import([19; 32]).unwrap(), store.clone())
            .await
            .unwrap();
        let signer = root.create_channel().await.unwrap();
        let a = signer
            .get_musig2_nonce(slot(NoncePurpose::Commitment))
            .await
            .unwrap()
            .public_nonce;
        let b = signer
            .get_musig2_nonce(slot(NoncePurpose::Commitment))
            .await
            .unwrap()
            .public_nonce;
        let c = signer
            .get_musig2_nonce(slot(NoncePurpose::Closing))
            .await
            .unwrap()
            .public_nonce;
        assert_ne!(a, b);
        assert_ne!(a, c);
        let restored = root.open_channel(signer.channel_key_id()).await.unwrap();
        let (_, state) = restored.load_signing_state().await.unwrap();
        assert_eq!(state.nonces.len(), 3);
        let remote = InMemorySigner::generate_from_seed(b"v2 remote");
        let keys = [
            signer.key_material.funding_key.pubkey(),
            remote.funding_key.pubkey(),
        ];
        let ctx = KeyAggContext::new(keys).unwrap();
        let peer = SecNonceBuilder::new([81; 32])
            .with_seckey(&remote.funding_key)
            .build();
        let content = ChannelSigningContent::Musig2(Musig2SigningContent {
            v2: Some(V2NonceBinding {
                channel_id: Hash256::from([3; 32]),
                owner: keys[1],
                public_nonce: a.clone(),
            }),
            slot: slot(NoncePurpose::Commitment),
            commitment_counter: None,
            key_agg_ctx: ctx.clone(),
            agg_nonce: AggNonce::sum([a.clone(), peer.public_nonce()]),
            content: Musig2SignableContent::CommitmentTransaction(
                ckb_types::packed::Transaction::default(),
            ),
        });
        let data = |content: ChannelSigningContent, revision: u64| SigningData {
            channel_key_id: signer.channel_key_id(),
            state_revision: revision,
            review: SigningReview {
                intent: content.intent(),
                commitment_counter: None,
                commitment_number: Some(2),
                signing_message: content.signing_message(),
                content_hash: content.content_hash(&content.canonical_bytes()).unwrap(),
                canonical_content: content.canonical_bytes(),
                warnings: vec![],
            },
            content,
        };
        let first = restored
            .sign_inner(data(content.clone(), state.revision), None)
            .await
            .unwrap();
        let replay = restored
            .sign_inner(data(content.clone(), state.revision), None)
            .await
            .unwrap();
        assert_eq!(format!("{first:?}"), format!("{replay:?}"));
        let (_, state) = restored.load_signing_state().await.unwrap();
        let ChannelSigningContent::Musig2(mut changed) = content.clone() else {
            unreachable!()
        };
        changed.v2.as_mut().unwrap().owner = keys[0];
        assert!(restored
            .sign_inner(
                data(ChannelSigningContent::Musig2(changed), state.revision),
                None
            )
            .await
            .is_err());
        let ChannelSigningContent::Musig2(mut changed) = content else {
            unreachable!()
        };
        changed.slot.commitment_number += 1;
        assert!(restored
            .sign_inner(
                data(ChannelSigningContent::Musig2(changed), state.revision),
                None
            )
            .await
            .is_err());
    }
}
