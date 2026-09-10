use std::{collections::BTreeSet, fmt};

use fiber_types::{
    blake2b_hash_with_salt, ChannelOpenSignerMaterial, InMemorySigner, Musig2Context,
    NextChannelSignerMaterial, Pubkey,
};
use musig2::{sign_partial, SecNonce};
use secp256k1::{Message, PublicKey, SECP256K1};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    ChannelBinding, ChannelKeyId, ChannelPublicMaterial, ChannelSignature, ChannelSigningContent,
    CommitmentCounter, MemoryStore, Musig2Nonce, Musig2SignableContent, Musig2Signature,
    NoncePurpose, NonceSlot, OnchainKeyPurpose, OnchainSignature, PreparedSigning, RootKey,
    RootKeyBackup, SignerError, SignerStore, SigningIntent, SigningReview, SigningWarning,
};

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
    commitment_counter: Option<CommitmentCounter>,
    nonce_slot: Option<NonceSlot>,
    content_hash: [u8; 32],
    signing_message: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredSigningState {
    format_version: u16,
    revision: u64,
    local_highest_signed: Option<u64>,
    remote_highest_signed: Option<u64>,
    published_nonces: BTreeSet<NonceSlot>,
    signed_requests: Vec<StoredSignedRequest>,
}

impl Default for StoredSigningState {
    fn default() -> Self {
        Self {
            format_version: STORE_FORMAT_VERSION,
            revision: 0,
            local_highest_signed: None,
            remote_highest_signed: None,
            published_nonces: BTreeSet::new(),
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
        Ok(ChannelOpenSignerMaterial {
            base_public_keys: self.public_material().base_public_keys,
            first_commitment_point: self.get_commitment_point(1),
            second_commitment_point: self.get_commitment_point(2),
            commitment_nonce: commitment_nonce.public_nonce,
            next_commitment_nonce: next_commitment_nonce.public_nonce,
            revocation_nonce: revocation_nonce.public_nonce,
            channel_announcement_nonce: channel_announcement_nonce.map(|nonce| nonce.public_nonce),
        })
    }

    /// Public material for the next commitment number after `slot`.
    pub async fn next_material(
        &self,
        slot: NonceSlot,
    ) -> Result<NextChannelSignerMaterial, SignerError> {
        let commitment_number = slot.commitment_number.saturating_add(1);
        let next_commitment_nonce = self
            .get_musig2_nonce(NonceSlot {
                purpose: NoncePurpose::Commitment,
                commitment_number,
            })
            .await?;
        let next_revocation_nonce = self
            .get_musig2_nonce(NonceSlot {
                purpose: NoncePurpose::Revocation,
                commitment_number,
            })
            .await?;
        Ok(NextChannelSignerMaterial {
            next_commitment_point: Some(self.get_commitment_point(commitment_number)),
            next_commitment_nonce: Some(next_commitment_nonce.public_nonce),
            next_revocation_nonce: Some(next_revocation_nonce.public_nonce),
        })
    }
}

impl<S: SignerStore> ChannelSigner<S> {
    /// Obtain and record a Fiber-compatible public nonce for a typed nonce slot.
    pub async fn get_musig2_nonce(&self, slot: NonceSlot) -> Result<Musig2Nonce, SignerError> {
        validate_nonce_slot(slot)?;
        loop {
            let (encoded, mut state) = self.load_signing_state().await?;
            if state.published_nonces.contains(&slot) {
                return Ok(Musig2Nonce {
                    public_nonce: derive_nonce(&self.key_material, slot).public_nonce(),
                });
            }
            state.published_nonces.insert(slot);
            state.revision = state.revision.checked_add(1).ok_or_else(|| {
                SignerError::CorruptStore("signing-state revision overflow".to_string())
            })?;
            if self.store_signing_state(encoded.as_deref(), &state).await? {
                return Ok(Musig2Nonce {
                    public_nonce: derive_nonce(&self.key_material, slot).public_nonce(),
                });
            }
        }
    }

    /// Bind this signer to the funding identity the user already approved.
    ///
    /// `unsigned_funding_tx` must be the frozen transaction returned by
    /// `open_channel_with_external_funding`. The caller supplies the cells it
    /// agreed to spend and the shutdown script it sent in that open request.
    /// Fiber puts the funding cell at output index 0; pass another index only
    /// when reviewing a transaction that does not follow that layout.
    pub async fn bind_from_approved_funding(
        &self,
        unsigned_funding_tx: &ckb_types::packed::Transaction,
        funding_output_index: u32,
        local_shutdown_script: ckb_types::packed::Script,
        expected_input_outpoints: &[ckb_types::packed::OutPoint],
    ) -> Result<ChannelBinding, SignerError> {
        let binding = approved_funding_identity(
            unsigned_funding_tx,
            funding_output_index,
            local_shutdown_script,
            expected_input_outpoints,
        )?;
        self.persist_binding(binding.clone()).await?;
        Ok(binding)
    }

    /// Record a previously derived funding identity. Test-only shortcut.
    #[cfg(test)]
    pub async fn bind_channel(&self, binding: ChannelBinding) -> Result<(), SignerError> {
        self.persist_binding(binding).await
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

    /// Independently hash typed plaintext after checking the bound funding identity.
    ///
    /// Requires [`Self::bind_from_approved_funding`]. This is an identity check,
    /// not a balance policy: it does not reconstruct TLC state or output amounts.
    /// Call [`crate::SigningPolicy::decide`] on the returned review before [`sign`].
    pub async fn prepare(
        &self,
        content: ChannelSigningContent,
    ) -> Result<PreparedSigning, SignerError> {
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
        let warnings = signing_warnings(&state, &content, signing_message);
        let review = SigningReview {
            intent: content.intent(),
            commitment_counter: content.commitment_counter(),
            commitment_number: content.nonce_slot().map(|slot| slot.commitment_number),
            signing_message,
            content_hash,
            canonical_content,
            warnings,
        };
        Ok(PreparedSigning {
            channel_key_id: self.channel_key_id,
            state_revision: state.revision,
            content,
            review,
        })
    }

    /// Sign an exact request after its [`SigningReview`] has been approved.
    pub async fn sign(&self, prepared: PreparedSigning) -> Result<ChannelSignature, SignerError> {
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
                && request.commitment_counter == prepared.content.commitment_counter()
                && request.nonce_slot == prepared.content.nonce_slot()
                && request.content_hash == content_hash
                && request.signing_message == signing_message
        });
        if state.revision != prepared.state_revision && !already_signed {
            return Err(SignerError::SigningStateChanged);
        }

        let signature = self.sign_content(&prepared.content, signing_message)?;
        if already_signed {
            return Ok(signature);
        }

        state.signed_requests.push(StoredSignedRequest {
            intent: prepared.content.intent(),
            commitment_counter: prepared.content.commitment_counter(),
            nonce_slot: prepared.content.nonce_slot(),
            content_hash,
            signing_message,
        });
        update_highest_signed(&mut state, &prepared.content);
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
    ) -> Result<ChannelSignature, SignerError> {
        match content {
            ChannelSigningContent::Musig2(content) => {
                let partial_signature = sign_partial(
                    &content.key_agg_ctx,
                    &self.key_material.funding_key,
                    derive_nonce(&self.key_material, content.slot),
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
                let state: StoredSigningState = decode(bytes)?;
                if state.format_version != STORE_FORMAT_VERSION {
                    return Err(SignerError::UnsupportedStoreVersion(state.format_version));
                }
                state
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

    async fn load_binding(&self) -> Result<Option<ChannelBinding>, SignerError> {
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
    match expected {
        NoncePurpose::ChannelAnnouncement if content.commitment_counter.is_some() => {
            Err(SignerError::InvalidContent(
                "channel announcement must not select a commitment counter".to_string(),
            ))
        }
        NoncePurpose::ChannelAnnouncement => Ok(()),
        _ if content.commitment_counter.is_none() => Err(SignerError::InvalidContent(
            "commitment and revocation signing must select a commitment counter".to_string(),
        )),
        _ => Ok(()),
    }
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

fn signing_warnings(
    state: &StoredSigningState,
    content: &ChannelSigningContent,
    signing_message: [u8; 32],
) -> Vec<SigningWarning> {
    let mut warnings = Vec::new();
    if let Some(slot) = content.nonce_slot() {
        for previous in state
            .signed_requests
            .iter()
            .filter(|request| request.nonce_slot == Some(slot))
        {
            if previous.signing_message != signing_message {
                warnings.push(SigningWarning::NoncePreviouslyUsedForDifferentMessage {
                    previous_message: previous.signing_message,
                });
            }
        }
        if let Some(counter) = content.commitment_counter() {
            let highest = match counter {
                CommitmentCounter::Local => state.local_highest_signed,
                CommitmentCounter::Remote => state.remote_highest_signed,
            };
            if let Some(highest_signed) = highest {
                if slot.commitment_number < highest_signed {
                    warnings.push(SigningWarning::CommitmentNumberRollback {
                        highest_signed,
                        requested: slot.commitment_number,
                    });
                } else if slot.commitment_number > highest_signed.saturating_add(1) {
                    warnings.push(SigningWarning::CommitmentNumberJump {
                        highest_signed,
                        requested: slot.commitment_number,
                    });
                }
            }
        }
    }
    warnings
}

fn update_highest_signed(state: &mut StoredSigningState, content: &ChannelSigningContent) {
    let (Some(counter), Some(slot)) = (content.commitment_counter(), content.nonce_slot()) else {
        return;
    };
    let highest = match counter {
        CommitmentCounter::Local => &mut state.local_highest_signed,
        CommitmentCounter::Remote => &mut state.remote_highest_signed,
    };
    if highest.is_none_or(|current| slot.commitment_number > current) {
        *highest = Some(slot.commitment_number);
    }
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

fn derive_nonce(signer: &InMemorySigner, slot: NonceSlot) -> SecNonce {
    match slot.purpose {
        NoncePurpose::Commitment => {
            signer.derive_musig2_nonce(slot.commitment_number, Musig2Context::Commitment)
        }
        NoncePurpose::Revocation => {
            signer.derive_musig2_nonce(slot.commitment_number, Musig2Context::Revoke)
        }
        NoncePurpose::ChannelAnnouncement => {
            let seed =
                blake2b_hash_with_salt(signer.musig2_base_nonce.as_ref(), b"channel_announcement");
            SecNonce::build(seed).build()
        }
    }
}

#[cfg(test)]
mod tests {
    use ckb_types::core::TransactionBuilder;
    use ckb_types::prelude::*;
    use fiber_types::{Hash256, InMemorySigner, Musig2Context};
    use musig2::{verify_partial, AggNonce, KeyAggContext};
    use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};

    use super::*;
    use crate::{
        MemoryStoreError, Musig2SignableContent, Musig2SigningContent, OnchainSigningContent,
    };

    fn root_key() -> RootKey {
        RootKey::import([42; 32]).expect("valid fixed root key")
    }

    fn transaction(version: u32) -> ckb_types::core::TransactionView {
        TransactionBuilder::default().version(version).build()
    }

    fn funding_outpoint() -> ckb_types::packed::OutPoint {
        ckb_types::packed::OutPoint::new_builder()
            .tx_hash([7u8; 32].pack())
            .index(0u32)
            .build()
    }

    fn shutdown_script() -> ckb_types::packed::Script {
        ckb_types::packed::Script::new_builder()
            .args([1u8, 2, 3].pack())
            .build()
    }

    fn lock_script_for(local: Pubkey, remote: Pubkey) -> ckb_types::packed::Script {
        let ctx = KeyAggContext::new([local, remote]).expect("aggregate keys");
        ckb_types::packed::Script::new_builder()
            .args(aggregated_funding_lock_args(&ctx).to_vec().pack())
            .build()
    }

    fn binding(channel: &ChannelSigner<MemoryStore>, remote: &InMemorySigner) -> ChannelBinding {
        ChannelBinding {
            funding_outpoint: funding_outpoint(),
            funding_lock_script: lock_script_for(
                channel.public_material().base_public_keys.funding_pubkey,
                remote.funding_key.pubkey(),
            ),
            local_shutdown_script: shutdown_script(),
        }
    }

    async fn bind_remote(channel: &ChannelSigner<MemoryStore>, remote: &InMemorySigner) {
        channel
            .bind_channel(binding(channel, remote))
            .await
            .expect("bind test channel");
    }

    fn commitment_tx(version: u32) -> ckb_types::packed::Transaction {
        use ckb_types::packed::CellInput;
        TransactionBuilder::default()
            .version(version)
            .input(
                CellInput::new_builder()
                    .previous_output(funding_outpoint())
                    .build(),
            )
            .build()
            .data()
    }

    async fn musig_content(
        channel: &ChannelSigner<MemoryStore>,
        remote_signer: &InMemorySigner,
        commitment_number: u64,
        version: u32,
    ) -> (
        ChannelSigningContent,
        musig2::PubNonce,
        KeyAggContext,
        AggNonce,
    ) {
        let slot = NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number,
        };
        let local_nonce = channel
            .get_musig2_nonce(slot)
            .await
            .expect("local nonce")
            .public_nonce;
        let remote_nonce = remote_signer
            .derive_musig2_nonce(commitment_number, Musig2Context::Commitment)
            .public_nonce();
        let key_agg_ctx = KeyAggContext::new([
            channel.public_material().base_public_keys.funding_pubkey,
            remote_signer.funding_key.pubkey(),
        ])
        .expect("aggregate keys");
        let agg_nonce = AggNonce::sum([local_nonce.clone(), remote_nonce]);
        (
            ChannelSigningContent::Musig2(Musig2SigningContent {
                slot,
                commitment_counter: Some(CommitmentCounter::Local),
                key_agg_ctx: key_agg_ctx.clone(),
                agg_nonce: agg_nonce.clone(),
                content: Musig2SignableContent::CommitmentTransaction(commitment_tx(version)),
            }),
            local_nonce,
            key_agg_ctx,
            agg_nonce,
        )
    }

    #[tokio::test]
    async fn channel_open_material_matches_fiber_open_channel_slots() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let private = channel
            .channel_open_material(false)
            .await
            .expect("private open material");
        assert_eq!(
            private.base_public_keys,
            channel.public_material().base_public_keys
        );
        assert_eq!(
            private.first_commitment_point,
            channel.get_commitment_point(1)
        );
        assert_eq!(
            private.second_commitment_point,
            channel.get_commitment_point(2)
        );
        assert!(private.channel_announcement_nonce.is_none());
        let public = channel
            .channel_open_material(true)
            .await
            .expect("public open material");
        assert!(public.channel_announcement_nonce.is_some());
        assert_eq!(
            public.commitment_nonce,
            channel
                .get_musig2_nonce(NonceSlot {
                    purpose: NoncePurpose::Commitment,
                    commitment_number: 0,
                })
                .await
                .expect("commitment nonce")
                .public_nonce
        );
        assert_eq!(
            public.next_commitment_nonce,
            channel
                .get_musig2_nonce(NonceSlot {
                    purpose: NoncePurpose::Commitment,
                    commitment_number: 1,
                })
                .await
                .expect("tx-complete commitment nonce")
                .public_nonce
        );
    }

    #[tokio::test]
    async fn restored_channel_signer_preserves_signed_requests() {
        let store = MemoryStore::default();
        let root = RootSigner::create(root_key(), store.clone())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel signer");
        let channel_key_id = channel.channel_key_id();
        let material = channel.public_material();
        let remote_signer = InMemorySigner::generate_from_seed(b"restore test remote signer");
        bind_remote(&channel, &remote_signer).await;
        let (content, nonce_before, _, _) = musig_content(&channel, &remote_signer, 42, 1).await;
        let prepared = channel.prepare(content.clone()).await.expect("prepare");
        let review_before = prepared.review().clone();
        let partial_before = channel.sign(prepared).await.expect("sign");
        let onchain = ChannelSigningContent::Onchain(OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Settlement,
            transaction: transaction(2).data(),
        });
        let settlement_before = channel
            .sign(channel.prepare(onchain.clone()).await.expect("prepare"))
            .await
            .expect("sign settlement");

        let snapshot = store.snapshot().expect("serialize store");
        let restored_store = MemoryStore::from_snapshot(&snapshot).expect("restore store");
        let restored_root = RootSigner::open(root_key(), restored_store)
            .await
            .expect("open root signer");
        let restored = restored_root
            .open_channel(channel_key_id)
            .await
            .expect("open channel signer");
        assert_eq!(restored.public_material(), material);
        let slot = NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: 42,
        };
        assert_eq!(
            restored
                .get_musig2_nonce(slot)
                .await
                .expect("restored nonce")
                .public_nonce,
            nonce_before
        );
        let prepared = restored.prepare(content).await.expect("prepare restored");
        assert_eq!(prepared.review(), &review_before);
        assert_eq!(
            restored.sign(prepared).await.expect("sign restored"),
            partial_before
        );
        let prepared = restored.prepare(onchain).await.expect("prepare settlement");
        assert_eq!(
            restored.sign(prepared).await.expect("sign settlement"),
            settlement_before
        );
    }

    #[tokio::test]
    async fn root_signer_lifecycle_rejects_misuse() {
        let store = MemoryStore::default();
        let root = RootSigner::create(root_key(), store.clone())
            .await
            .expect("create root signer");
        assert_eq!(root.identity_public_key(), root_key().public_key());
        assert_eq!(
            RootSigner::create(root_key(), store.clone())
                .await
                .expect_err("create must not overwrite a store"),
            SignerError::AlreadyInitialized
        );
        assert_eq!(
            RootSigner::open(
                RootKey::import([43; 32]).expect("other valid root key"),
                store,
            )
            .await
            .expect_err("wrong key must not open the store"),
            SignerError::RootKeyMismatch
        );
    }

    #[tokio::test]
    async fn root_signer_signs_only_its_tenant_registry_identity() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let lsp_node_id = RootKey::import([7; 32])
            .expect("valid LSP key")
            .public_key()
            .into();
        let payload = fiber_types::TenantRegistryPayload::new(
            lsp_node_id,
            root.identity_public_key().into(),
            [3; 32],
        );
        let signature = root
            .sign_tenant_registry_payload(&payload)
            .expect("sign registration payload");
        payload
            .verify_signature(&signature)
            .expect("verify registration proof");

        let wrong_payload = fiber_types::TenantRegistryPayload::new(
            lsp_node_id,
            RootKey::import([8; 32])
                .expect("valid other key")
                .public_key()
                .into(),
            [3; 32],
        );
        assert!(matches!(
            root.sign_tenant_registry_payload(&wrong_payload),
            Err(SignerError::InvalidContent(_))
        ));
    }

    #[tokio::test]
    async fn generated_root_key_backup_reopens_the_serialized_store() {
        let store = MemoryStore::default();
        let created = RootSigner::create_random(store.clone())
            .await
            .expect("create random root signer");
        let expected_public_key = created.root_signer.identity_public_key();
        let backup = created.root_key_backup.expose_secret();
        let snapshot = store.snapshot().expect("serialize store");
        let reopened = RootSigner::open(
            RootKey::import(backup).expect("import generated backup"),
            MemoryStore::from_snapshot(&snapshot).expect("restore store"),
        )
        .await
        .expect("open generated root signer");
        assert_eq!(reopened.identity_public_key(), expected_public_key);
    }

    #[tokio::test]
    async fn channel_creation_is_fresh_and_persisted_before_return() {
        let store = MemoryStore::default();
        let root = RootSigner::create(root_key(), store.clone())
            .await
            .expect("create root signer");
        let first = root.create_channel().await.expect("first channel");
        let second = root.create_channel().await.expect("second channel");
        assert_ne!(first.channel_key_id(), second.channel_key_id());
        assert_ne!(first.public_material(), second.public_material());
        let reopened_root = RootSigner::open(root_key(), store)
            .await
            .expect("reopen root signer");
        assert_eq!(
            reopened_root
                .open_channel(first.channel_key_id())
                .await
                .expect("open persisted channel")
                .public_material(),
            first.public_material()
        );
        let unknown = ChannelKeyId(Hash256::from([99; 32]));
        assert_eq!(
            reopened_root
                .open_channel(unknown)
                .await
                .expect_err("unknown channel must fail"),
            SignerError::UnknownChannelKey(unknown)
        );
    }

    #[tokio::test]
    async fn signer_computes_digest_and_signatures_from_plaintext() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote_signer = InMemorySigner::generate_from_seed(b"remote signer");
        bind_remote(&channel, &remote_signer).await;
        let (content, local_nonce, key_agg_ctx, agg_nonce) =
            musig_content(&channel, &remote_signer, 7, 11).await;
        let expected_message = content.signing_message();
        let prepared = channel.prepare(content).await.expect("prepare");
        assert_eq!(prepared.review().signing_message, expected_message);
        assert_eq!(prepared.content().signing_message(), expected_message);
        let response = match channel.sign(prepared).await.expect("sign") {
            ChannelSignature::Musig2(response) => response,
            ChannelSignature::Onchain(_) => panic!("expected MuSig2 signature"),
        };
        verify_partial(
            &key_agg_ctx,
            response.partial_signature,
            &agg_nonce,
            channel.public_material().base_public_keys.funding_pubkey,
            &local_nonce,
            expected_message,
        )
        .expect("valid partial signature");

        let onchain = ChannelSigningContent::Onchain(OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Settlement,
            transaction: transaction(12).data(),
        });
        let expected_message = crate::compute_tx_message(&transaction(12));
        let prepared = channel.prepare(onchain).await.expect("prepare onchain");
        assert_eq!(prepared.review().signing_message, expected_message);
        let response = match channel.sign(prepared).await.expect("sign onchain") {
            ChannelSignature::Onchain(response) => response,
            ChannelSignature::Musig2(_) => panic!("expected on-chain signature"),
        };
        let recovery_id =
            RecoveryId::try_from(i32::from(response.signature[64])).expect("recovery id");
        let signature = RecoverableSignature::from_compact(&response.signature[..64], recovery_id)
            .expect("recoverable signature");
        let recovered = SECP256K1
            .recover_ecdsa(&Message::from_digest(expected_message), &signature)
            .expect("recover public key");
        assert_eq!(
            Pubkey(recovered.serialize()),
            channel.public_material().base_public_keys.tlc_base_key
        );
    }

    #[tokio::test]
    async fn reviewed_plaintext_cannot_be_replaced_before_signing() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"tamper remote signer");
        bind_remote(&channel, &remote).await;
        let (content, _, _, _) = musig_content(&channel, &remote, 9, 1).await;
        let mut prepared = channel.prepare(content).await.expect("prepare");
        let ChannelSigningContent::Musig2(content) = &mut prepared.content else {
            panic!("expected MuSig2 content");
        };
        content.content = Musig2SignableContent::CommitmentTransaction(transaction(2).data());
        assert!(matches!(
            channel.sign(prepared).await,
            Err(SignerError::InvalidContent(_))
        ));
    }

    #[tokio::test]
    async fn nonce_reuse_and_commitment_counter_anomalies_are_review_warnings() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"warning remote signer");
        bind_remote(&channel, &remote).await;
        let (first, _, _, _) = musig_content(&channel, &remote, 5, 1).await;
        let prepared = channel.prepare(first).await.expect("prepare first");
        channel.sign(prepared).await.expect("sign first");

        let (different, _, _, _) = musig_content(&channel, &remote, 5, 2).await;
        let prepared = channel.prepare(different).await.expect("prepare reuse");
        assert!(prepared.review().warnings.iter().any(|warning| matches!(
            warning,
            SigningWarning::NoncePreviouslyUsedForDifferentMessage { .. }
        )));
        channel
            .sign(prepared)
            .await
            .expect("compatibility mode permits explicitly reviewed reuse");

        let (rollback, _, _, _) = musig_content(&channel, &remote, 4, 3).await;
        assert!(channel
            .prepare(rollback)
            .await
            .expect("prepare rollback")
            .review()
            .warnings
            .iter()
            .any(|warning| matches!(warning, SigningWarning::CommitmentNumberRollback { .. })));
        let (jump, _, _, _) = musig_content(&channel, &remote, 8, 4).await;
        assert!(channel
            .prepare(jump)
            .await
            .expect("prepare jump")
            .review()
            .warnings
            .iter()
            .any(|warning| matches!(warning, SigningWarning::CommitmentNumberJump { .. })));
    }

    #[tokio::test]
    async fn concurrent_approvals_use_store_compare_and_swap() {
        let store = MemoryStore::default();
        let root = RootSigner::create(root_key(), store)
            .await
            .expect("create root");
        let channel = root.create_channel().await.expect("create channel");
        let first = root
            .open_channel(channel.channel_key_id())
            .await
            .expect("open first");
        let second = root
            .open_channel(channel.channel_key_id())
            .await
            .expect("open second");
        let remote = InMemorySigner::generate_from_seed(b"concurrent remote signer");
        bind_remote(&first, &remote).await;
        let (first_content, _, _, _) = musig_content(&first, &remote, 3, 1).await;
        let (second_content, _, _, _) = musig_content(&second, &remote, 3, 2).await;
        let first_prepared = first.prepare(first_content).await.expect("prepare first");
        let second_prepared = second
            .prepare(second_content)
            .await
            .expect("prepare second");
        let (first_result, second_result) =
            tokio::join!(first.sign(first_prepared), second.sign(second_prepared));
        assert_eq!(
            usize::from(first_result.is_ok()) + usize::from(second_result.is_ok()),
            1
        );
        assert!(matches!(
            first_result.as_ref().err().or(second_result.as_ref().err()),
            Some(SignerError::SigningStateChanged)
        ));
    }

    #[tokio::test]
    async fn tampered_channel_derivation_record_is_rejected() {
        let store = MemoryStore::default();
        let root = RootSigner::create(root_key(), store.clone())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let store_key = channel_store_key(channel.channel_key_id());
        let encoded = store
            .get(&store_key)
            .await
            .expect("read store")
            .expect("channel record");
        let mut record: StoredChannel = decode(&encoded).expect("decode channel record");
        record.allocation_entropy[0] ^= 1;
        store
            .put(
                &store_key,
                &encode(&record).expect("encode tampered record"),
            )
            .await
            .expect("write tampered record");
        assert!(matches!(
            root.open_channel(channel.channel_key_id()).await,
            Err(SignerError::CorruptStore(_))
        ));
    }

    #[test]
    fn memory_store_rejects_invalid_snapshots() {
        assert!(matches!(
            MemoryStore::from_snapshot(b"not a store"),
            Err(MemoryStoreError::InvalidSnapshot(_))
        ));
    }

    fn tx_spending_funding_and_paying_shutdown() -> ckb_types::packed::Transaction {
        use ckb_types::{packed::CellInput, prelude::*};
        TransactionBuilder::default()
            .input(
                CellInput::new_builder()
                    .previous_output(funding_outpoint())
                    .build(),
            )
            .output(
                ckb_types::packed::CellOutput::new_builder()
                    .lock(shutdown_script())
                    .capacity(1000u64)
                    .build(),
            )
            .build()
            .data()
    }

    async fn bind_and_prepare_commitment(
        channel: &ChannelSigner<MemoryStore>,
        remote: &InMemorySigner,
        transaction: ckb_types::packed::Transaction,
    ) -> Result<PreparedSigning, SignerError> {
        let (mut content, _, _, _) = musig_content(channel, remote, 1, 1).await;
        let ChannelSigningContent::Musig2(inner) = &mut content else {
            panic!("expected MuSig2 content");
        };
        inner.content = Musig2SignableContent::CommitmentTransaction(transaction);
        channel.prepare(content).await
    }

    #[tokio::test]
    async fn prepare_requires_a_channel_binding() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"unbound remote");
        let (content, _, _, _) = musig_content(&channel, &remote, 1, 1).await;
        assert_eq!(
            channel.prepare(content).await.expect_err("unbound"),
            SignerError::ChannelNotBound
        );
    }

    #[tokio::test]
    async fn bind_channel_is_idempotent_for_the_same_identity() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"bind remote");
        let identity = binding(&channel, &remote);
        channel
            .bind_channel(identity.clone())
            .await
            .expect("bind once");
        channel
            .bind_channel(identity.clone())
            .await
            .expect("bind the same identity again");
        let mut other = identity;
        other.funding_outpoint = ckb_types::packed::OutPoint::new([9u8; 32].pack(), 0);
        assert_eq!(
            channel
                .bind_channel(other)
                .await
                .expect_err("different identity"),
            SignerError::ChannelAlreadyBound
        );
    }

    #[tokio::test]
    async fn prepare_rejects_musig2_keys_that_do_not_match_the_binding() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"bound remote");
        let attacker = InMemorySigner::generate_from_seed(b"attacker remote");
        channel
            .bind_channel(binding(&channel, &remote))
            .await
            .expect("bind channel");
        let (content, _, _, _) = musig_content(&channel, &attacker, 1, 1).await;
        assert!(matches!(
            channel.prepare(content).await,
            Err(SignerError::InvalidContent(_))
        ));
    }

    #[tokio::test]
    async fn prepare_rejects_a_commitment_that_does_not_spend_funding() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"spend remote");
        channel
            .bind_channel(binding(&channel, &remote))
            .await
            .expect("bind channel");
        assert!(matches!(
            bind_and_prepare_commitment(&channel, &remote, transaction(1).data()).await,
            Err(SignerError::InvalidContent(_))
        ));
    }

    #[tokio::test]
    async fn prepare_accepts_onchain_settlement_without_a_shutdown_output() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"onchain remote");
        channel
            .bind_channel(binding(&channel, &remote))
            .await
            .expect("bind channel");
        let onchain = ChannelSigningContent::Onchain(OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Settlement,
            transaction: transaction(1).data(),
        });
        channel
            .prepare(onchain)
            .await
            .expect("settlement txs are not checked against the shutdown script");
    }

    #[tokio::test]
    async fn prepare_accepts_a_commitment_that_matches_the_binding() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"valid remote");
        channel
            .bind_channel(binding(&channel, &remote))
            .await
            .expect("bind channel");
        let prepared = bind_and_prepare_commitment(
            &channel,
            &remote,
            tx_spending_funding_and_paying_shutdown(),
        )
        .await
        .expect("verified prepare");
        channel.sign(prepared).await.expect("sign verified request");
    }

    fn approved_funding_tx(
        inputs: &[ckb_types::packed::OutPoint],
        funding_lock: ckb_types::packed::Script,
    ) -> ckb_types::packed::Transaction {
        use ckb_types::packed::CellInput;
        let mut builder = TransactionBuilder::default();
        for outpoint in inputs {
            builder = builder.input(
                CellInput::new_builder()
                    .previous_output(outpoint.clone())
                    .build(),
            );
        }
        builder
            .output(
                ckb_types::packed::CellOutput::new_builder()
                    .lock(funding_lock)
                    .capacity(1000u64)
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data()
    }

    #[tokio::test]
    async fn bind_from_approved_funding_records_the_funding_outpoint() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"approved remote");
        let input = funding_outpoint();
        let lock = lock_script_for(
            channel.public_material().base_public_keys.funding_pubkey,
            remote.funding_key.pubkey(),
        );
        let tx = approved_funding_tx(std::slice::from_ref(&input), lock.clone());
        let binding = channel
            .bind_from_approved_funding(&tx, 0, shutdown_script(), std::slice::from_ref(&input))
            .await
            .expect("bind from approved funding");
        assert_eq!(binding.funding_lock_script, lock);
        assert_eq!(binding.local_shutdown_script, shutdown_script());
        assert_eq!(binding.funding_outpoint.tx_hash(), tx.calc_tx_hash());
        channel
            .bind_from_approved_funding(&tx, 0, shutdown_script(), &[input])
            .await
            .expect("binding the same approved funding is idempotent");
    }

    #[tokio::test]
    async fn bind_from_approved_funding_rejects_unexpected_inputs() {
        let root = RootSigner::create(root_key(), MemoryStore::default())
            .await
            .expect("create root signer");
        let channel = root.create_channel().await.expect("create channel");
        let remote = InMemorySigner::generate_from_seed(b"wrong inputs");
        let tx = approved_funding_tx(
            &[funding_outpoint()],
            lock_script_for(
                channel.public_material().base_public_keys.funding_pubkey,
                remote.funding_key.pubkey(),
            ),
        );
        assert!(matches!(
            channel
                .bind_from_approved_funding(
                    &tx,
                    0,
                    shutdown_script(),
                    &[ckb_types::packed::OutPoint::new([8u8; 32].pack(), 0)]
                )
                .await,
            Err(SignerError::InvalidContent(_))
        ));
    }
}
