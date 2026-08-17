use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use fiber_json_types::{
    ChannelOpenSignerMaterial as JsonChannelOpenSignerMaterial, ChannelSigningStatus,
    RegisterLspTenantParams, SubmitChannelSignatureParams, SubmitChannelSignatureResult,
};
use fiber_lsp_sdk::{
    ChannelKeyId, ChannelSignature, ChannelSigningContent, RootKey, RootSigner, SignerStore,
    TenantId, TenantRegistryPayload,
};
use fiber_types::Hash256;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    convert::{musig2_from_rpc, next_material_to_rpc, open_material_to_rpc},
    rpc::FiberRpc,
};

const ROOT_KEY_FILE: &str = "root.key";
const AGENT_STATE_FILE: &str = "agent.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersistedAgent {
    tenant_token: Option<String>,
    bindings: HashMap<String, String>,
    pending_channel_key_id: Option<String>,
}

/// Fixture information consumed by the external E2E driver.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentStatus {
    pub tenant_id: String,
    pub root_signer_pubkey: fiber_json_types::Pubkey,
    pub tenant_token: String,
    pub channel_open_signer_material: Option<JsonChannelOpenSignerMaterial>,
    pub bound_channel_ids: Vec<String>,
}

pub struct AgentConfig {
    pub store_dir: PathBuf,
    pub status_file: Option<PathBuf>,
}

/// Auto-approving test client backed by the real portable SDK.
pub struct Agent<R, S> {
    rpc: R,
    root: RootSigner<S>,
    config: AgentConfig,
    tenant_id: TenantId,
    tenant_token: Option<String>,
    bindings: HashMap<Hash256, ChannelKeyId>,
    pending: Option<ChannelKeyId>,
    state_file: PathBuf,
}

impl<R: FiberRpc, S: SignerStore> Agent<R, S> {
    fn from_parts(
        rpc: R,
        root: RootSigner<S>,
        config: AgentConfig,
        persisted: PersistedAgent,
    ) -> Result<Self> {
        let tenant_id = TenantId::from_root_signer_pubkey(&root.identity_public_key().into());
        let bindings = persisted
            .bindings
            .into_iter()
            .map(|(channel_id, key_id)| {
                Ok((parse_hash(&channel_id)?, ChannelKeyId(parse_hash(&key_id)?)))
            })
            .collect::<Result<_>>()?;
        let pending = persisted
            .pending_channel_key_id
            .map(|key_id| parse_hash(&key_id).map(ChannelKeyId))
            .transpose()?;
        let state_file = config.store_dir.join(AGENT_STATE_FILE);
        Ok(Self {
            rpc,
            root,
            config,
            tenant_id,
            tenant_token: persisted.tenant_token,
            bindings,
            pending,
            state_file,
        })
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    pub fn pending_channel_key_id(&self) -> Option<ChannelKeyId> {
        self.pending
    }

    pub fn binding(&self, channel_id: Hash256) -> Option<ChannelKeyId> {
        self.bindings.get(&channel_id).copied()
    }

    /// Register the RootSigner tenant once and allocate channel open material.
    pub async fn initialize(&mut self) -> Result<()> {
        self.ensure_registered().await?;
        self.ensure_pending().await?;
        self.write_status().await
    }

    async fn ensure_registered(&mut self) -> Result<()> {
        if self.tenant_token.is_some() {
            return Ok(());
        }
        let root_signer_pubkey: fiber_types::Pubkey = self.root.identity_public_key().into();
        let nonce = self
            .rpc
            .get_tenant_registry_nonce(root_signer_pubkey.into())
            .await
            .context("request tenant registration nonce")?;
        let returned_root = fiber_types::Pubkey::try_from(nonce.root_signer_pubkey)
            .map_err(|error| anyhow!(error))?;
        if returned_root != root_signer_pubkey {
            return Err(anyhow!("LSP returned a nonce for another RootSigner"));
        }
        let nonce_hash: Hash256 = nonce.nonce.into();
        let payload = TenantRegistryPayload::new(
            fiber_types::Pubkey::try_from(nonce.lsp_node_id).map_err(|error| anyhow!(error))?,
            root_signer_pubkey,
            nonce_hash.into(),
        );
        let signature = self
            .root
            .sign_tenant_registry_payload(&payload)
            .map_err(|error| anyhow!("sign tenant registration: {error}"))?;
        let registered = self
            .rpc
            .register_tenant(RegisterLspTenantParams {
                root_signer_pubkey: root_signer_pubkey.into(),
                nonce: nonce_hash.into(),
                signature: hex::encode(signature.serialize()),
            })
            .await
            .context("register RootSigner tenant")?;
        if registered.tenant.tenant_id != self.tenant_id.as_str() {
            return Err(anyhow!("LSP returned an unexpected tenant id"));
        }
        self.tenant_token = Some(
            registered
                .access_token
                .ok_or_else(|| anyhow!("first registration returned no tenant token"))?,
        );
        self.persist_state()?;
        Ok(())
    }

    async fn ensure_pending(&mut self) -> Result<()> {
        if self.pending.is_some() {
            return Ok(());
        }
        let channel = self
            .root
            .create_channel()
            .await
            .map_err(|error| anyhow!("create channel signer: {error}"))?;
        self.pending = Some(channel.channel_key_id());
        self.persist_state()
    }

    /// Bind the pending signer to the funding transaction the user already approved.
    pub async fn bind_approved_funding(
        &mut self,
        channel_id: Hash256,
        unsigned_funding_tx: ckb_types::packed::Transaction,
        local_shutdown_script: ckb_types::packed::Script,
        funding_output_index: u32,
    ) -> Result<()> {
        if self.bindings.contains_key(&channel_id) {
            return Ok(());
        }
        let pending = self
            .pending
            .ok_or_else(|| anyhow!("no pending signer for channel {channel_id:#x}"))?;
        let expected_inputs: Vec<_> = unsigned_funding_tx
            .raw()
            .inputs()
            .into_iter()
            .map(|input| input.previous_output())
            .collect();
        let signer = self
            .root
            .open_channel(pending)
            .await
            .map_err(|error| anyhow!("open pending channel signer: {error}"))?;
        signer
            .bind_from_approved_funding(
                &unsigned_funding_tx,
                funding_output_index,
                local_shutdown_script,
                &expected_inputs,
            )
            .await
            .map_err(|error| anyhow!("bind approved funding: {error}"))?;
        self.pending = None;
        self.bindings.insert(channel_id, pending);
        self.persist_state()?;
        self.write_status().await?;
        info!(channel_id = %format!("{channel_id:#x}"), "bound SDK signer to approved funding");
        Ok(())
    }

    /// Service outstanding signing requests for already-bound channels.
    pub async fn poll_once(&mut self) -> Result<()> {
        let token = self.tenant_token()?.to_string();
        let bound = self
            .bindings
            .iter()
            .map(|(id, key)| (*id, *key))
            .collect::<Vec<_>>();
        for (channel_id, key_id) in bound {
            self.poll_channel(&token, channel_id, key_id).await?;
        }
        Ok(())
    }

    async fn poll_channel(
        &self,
        token: &str,
        channel_id: Hash256,
        key_id: ChannelKeyId,
    ) -> Result<()> {
        let status = self
            .rpc
            .get_channel_signing_status(token, channel_id.into())
            .await?
            .status;
        let ChannelSigningStatus::SignatureRequired {
            request_id,
            content,
            ..
        } = status
        else {
            return Ok(());
        };
        let signer = self
            .root
            .open_channel(key_id)
            .await
            .map_err(|error| anyhow!("open channel signer: {error}"))?;
        let content = musig2_from_rpc(content).map_err(|error| anyhow!(error))?;
        let slot = content.slot;
        let prepared = signer
            .prepare_bound(ChannelSigningContent::Musig2(content))
            .await
            .map_err(|error| anyhow!("prepare bound signature: {error}"))?;
        let signature = signer
            .sign(prepared)
            .await
            .map_err(|error| anyhow!("sign channel request: {error}"))?;
        let ChannelSignature::Musig2(signature) = signature else {
            return Err(anyhow!("channel request produced an on-chain signature"));
        };
        let next_material = signer
            .next_material(slot)
            .await
            .map_err(|error| anyhow!("derive next signer material: {error}"))?;
        let outcome = self
            .rpc
            .submit_channel_signature(
                token,
                SubmitChannelSignatureParams {
                    channel_id: channel_id.into(),
                    request_id,
                    partial_signature: signature.partial_signature.serialize(),
                    next_material: Some(next_material_to_rpc(&next_material)),
                },
            )
            .await?;
        match outcome {
            SubmitChannelSignatureResult::Applied
            | SubmitChannelSignatureResult::AlreadyApplied => Ok(()),
        }
    }

    async fn write_status(&self) -> Result<()> {
        let Some(path) = &self.config.status_file else {
            return Ok(());
        };
        let material = match self.pending {
            Some(key_id) => {
                let signer = self
                    .root
                    .open_channel(key_id)
                    .await
                    .map_err(|error| anyhow!("open pending channel signer: {error}"))?;
                Some(open_material_to_rpc(
                    &signer
                        .channel_open_material(false)
                        .await
                        .map_err(|error| anyhow!("create channel open material: {error}"))?,
                ))
            }
            None => None,
        };
        let mut bound_channel_ids = self
            .bindings
            .keys()
            .map(|channel_id| format!("{channel_id:#x}"))
            .collect::<Vec<_>>();
        bound_channel_ids.sort();
        let status = AgentStatus {
            tenant_id: self.tenant_id.as_str().to_string(),
            root_signer_pubkey: fiber_types::Pubkey::from(self.root.identity_public_key()).into(),
            tenant_token: self.tenant_token()?.to_string(),
            channel_open_signer_material: material,
            bound_channel_ids,
        };
        atomic_write(path, &serde_json::to_vec_pretty(&status)?)
    }

    fn tenant_token(&self) -> Result<&str> {
        self.tenant_token
            .as_deref()
            .ok_or_else(|| anyhow!("agent is not registered"))
    }

    fn persist_state(&self) -> Result<()> {
        let persisted = PersistedAgent {
            tenant_token: self.tenant_token.clone(),
            bindings: self
                .bindings
                .iter()
                .map(|(channel_id, key_id)| {
                    (format!("{channel_id:#x}"), format!("{:#x}", key_id.0))
                })
                .collect(),
            pending_channel_key_id: self.pending.map(|key_id| format!("{:#x}", key_id.0)),
        };
        atomic_write(&self.state_file, &serde_json::to_vec_pretty(&persisted)?)
    }
}

impl<R: FiberRpc> Agent<R, crate::FileSignerStore> {
    pub async fn open(rpc: R, config: AgentConfig) -> Result<Self> {
        fs::create_dir_all(&config.store_dir)
            .with_context(|| format!("create agent store {}", config.store_dir.display()))?;
        let store = crate::FileSignerStore::open(&config.store_dir)?;
        let root_key_path = config.store_dir.join(ROOT_KEY_FILE);
        let root = if root_key_path.exists() {
            RootSigner::open(RootKey::import(read_root_key(&root_key_path)?)?, store)
                .await
                .map_err(|error| anyhow!("open RootSigner: {error}"))?
        } else {
            let created = RootSigner::create_random(store)
                .await
                .map_err(|error| anyhow!("create RootSigner: {error}"))?;
            write_root_key(&root_key_path, &created.root_key_backup.expose_secret())?;
            created.root_signer
        };
        let persisted = load_persisted(&config.store_dir.join(AGENT_STATE_FILE))?;
        Self::from_parts(rpc, root, config, persisted)
    }
}

fn load_persisted(path: &Path) -> Result<PersistedAgent> {
    if !path.exists() {
        return Ok(PersistedAgent::default());
    }
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("decode {}", path.display()))
}

fn parse_hash(value: &str) -> Result<Hash256> {
    value.parse().with_context(|| format!("parse hash {value}"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
}

fn read_root_key(path: &Path) -> Result<[u8; 32]> {
    fs::read(path)?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("{} must contain 32 bytes", path.display()))
}

fn write_root_key(path: &Path, secret: &[u8; 32]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(secret)?;
    }
    #[cfg(not(unix))]
    fs::write(path, secret)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use fiber_json_types::{
        ChannelSigningTransition, GetChannelSigningStatusResult, GetLspTenantRegistryNonceResult,
        Hash256 as JsonHash256, LspTenantRuntimeStatus, LspTenantStatus, RegisterLspTenantResult,
    };
    use fiber_lsp_sdk::{MemoryStore, RootKey, RootSigner};
    use fiber_types::{Privkey, TenantRegistrySignature};

    use super::*;
    use crate::convert::{musig2_to_rpc, tests as convert_tests};

    #[derive(Clone)]
    struct FakeNode {
        inner: Arc<Mutex<FakeNodeState>>,
        lsp_key: Privkey,
    }

    #[derive(Default)]
    struct FakeNodeState {
        nonce_calls: usize,
        register_calls: usize,
        statuses: HashMap<JsonHash256, ChannelSigningStatus>,
        submissions: Vec<SubmitChannelSignatureParams>,
        tenant_tokens: Vec<String>,
    }

    impl Default for FakeNode {
        fn default() -> Self {
            Self {
                inner: Arc::new(Mutex::new(FakeNodeState::default())),
                lsp_key: Privkey::from(&[9u8; 32]),
            }
        }
    }

    impl FakeNode {
        fn state(&self) -> std::sync::MutexGuard<'_, FakeNodeState> {
            self.inner.lock().expect("lock fake node")
        }

        fn insert_external_channel(&self, channel_id: Hash256) {
            self.state()
                .statuses
                .insert(channel_id.into(), ChannelSigningStatus::NoSignatureRequired);
        }
    }

    #[async_trait]
    impl FiberRpc for FakeNode {
        async fn get_tenant_registry_nonce(
            &self,
            root_signer_pubkey: fiber_json_types::Pubkey,
        ) -> Result<GetLspTenantRegistryNonceResult> {
            self.state().nonce_calls += 1;
            Ok(GetLspTenantRegistryNonceResult {
                lsp_node_id: self.lsp_key.pubkey().into(),
                root_signer_pubkey,
                nonce: JsonHash256([5u8; 32]),
            })
        }

        async fn register_tenant(
            &self,
            params: RegisterLspTenantParams,
        ) -> Result<RegisterLspTenantResult> {
            let root_signer_pubkey = fiber_types::Pubkey::try_from(params.root_signer_pubkey)
                .map_err(|error| anyhow!(error))?;
            let nonce: Hash256 = params.nonce.into();
            let payload =
                TenantRegistryPayload::new(self.lsp_key.pubkey(), root_signer_pubkey, nonce.into());
            let signature = TenantRegistrySignature::from_slice(&hex::decode(params.signature)?)?;
            payload
                .verify_signature(&signature)
                .map_err(|error| anyhow!(error))?;
            let tenant_id = TenantId::from_root_signer_pubkey(&root_signer_pubkey);
            self.state().register_calls += 1;
            Ok(RegisterLspTenantResult {
                tenant: LspTenantStatus {
                    tenant_id: tenant_id.as_str().to_string(),
                    root_signer_pubkey: Some(root_signer_pubkey.into()),
                    invoice_pubkey: self.lsp_key.pubkey().into(),
                    private_channel_id: None,
                    created_at: 1,
                    runtime_status: LspTenantRuntimeStatus::Active,
                    channel_online: false,
                },
                access_token: Some("tenant-token".to_string()),
            })
        }

        async fn get_channel_signing_status(
            &self,
            tenant_token: &str,
            channel_id: JsonHash256,
        ) -> Result<GetChannelSigningStatusResult> {
            let mut state = self.state();
            state.tenant_tokens.push(tenant_token.to_string());
            let status = state
                .statuses
                .get(&channel_id)
                .cloned()
                .unwrap_or(ChannelSigningStatus::NoSignatureRequired);
            Ok(GetChannelSigningStatusResult { channel_id, status })
        }

        async fn submit_channel_signature(
            &self,
            tenant_token: &str,
            params: SubmitChannelSignatureParams,
        ) -> Result<SubmitChannelSignatureResult> {
            let mut state = self.state();
            state.tenant_tokens.push(tenant_token.to_string());
            state.submissions.push(params);
            Ok(SubmitChannelSignatureResult::Applied)
        }
    }

    fn channel_id() -> Hash256 {
        Hash256::from([0x11; 32])
    }

    fn funding_lock_for(
        local: fiber_types::Pubkey,
        remote: fiber_types::Pubkey,
    ) -> ckb_types::packed::Script {
        use ckb_types::prelude::*;
        use musig2::KeyAggContext;
        let ctx = KeyAggContext::new([local, remote]).expect("aggregate keys");
        let point: musig2::secp::Point = ctx.aggregated_pubkey();
        let digest = fiber_types::blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
        ckb_types::packed::Script::new_builder()
            .args(digest[..20].to_vec().pack())
            .build()
    }

    fn approved_funding_tx(
        funding_lock: ckb_types::packed::Script,
    ) -> ckb_types::packed::Transaction {
        use ckb_types::{packed::CellInput, prelude::*};
        ckb_types::core::TransactionBuilder::default()
            .input(
                CellInput::new_builder()
                    .previous_output(convert_tests::bound_funding_outpoint())
                    .build(),
            )
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

    async fn bind_pending(agent: &mut Agent<FakeNode, MemoryStore>, channel_id: Hash256) {
        let key_id = agent.pending_channel_key_id().expect("pending key");
        let signer = agent.root.open_channel(key_id).await.expect("open signer");
        let tx = approved_funding_tx(funding_lock_for(
            signer.public_material().base_public_keys.funding_pubkey,
            convert_tests::remote_binding_pubkey(),
        ));
        agent
            .bind_approved_funding(channel_id, tx, convert_tests::bound_shutdown_script(), 0)
            .await
            .expect("bind approved funding");
    }

    async fn memory_agent(node: FakeNode, dir: &Path) -> Agent<FakeNode, MemoryStore> {
        let root = RootSigner::create(
            RootKey::import([42; 32]).expect("root key"),
            MemoryStore::default(),
        )
        .await
        .expect("create root signer");
        Agent::from_parts(
            node,
            root,
            AgentConfig {
                store_dir: dir.to_path_buf(),
                status_file: Some(dir.join("status.json")),
            },
            PersistedAgent {
                tenant_token: Some("tenant-token".to_string()),
                ..PersistedAgent::default()
            },
        )
        .expect("create agent")
    }

    #[tokio::test]
    async fn initialize_registers_signed_root_identity_and_writes_open_material() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = FakeNode::default();
        let status_path = dir.path().join("status.json");
        let mut agent = Agent::open(
            node.clone(),
            AgentConfig {
                store_dir: dir.path().to_path_buf(),
                status_file: Some(status_path.clone()),
            },
        )
        .await
        .expect("open agent");

        agent.initialize().await.expect("initialize");

        let state = node.state();
        assert_eq!(state.nonce_calls, 1);
        assert_eq!(state.register_calls, 1);
        drop(state);
        let status: AgentStatus =
            serde_json::from_slice(&fs::read(status_path).expect("read status")).expect("status");
        assert_eq!(status.tenant_id, agent.tenant_id().as_str());
        assert_eq!(status.tenant_token, "tenant-token");
        assert!(status.channel_open_signer_material.is_some());
    }

    #[tokio::test]
    async fn restart_reuses_root_identity_token_and_pending_channel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = FakeNode::default();
        let config = || AgentConfig {
            store_dir: dir.path().to_path_buf(),
            status_file: Some(dir.path().join("status.json")),
        };
        let mut first = Agent::open(node.clone(), config())
            .await
            .expect("first open");
        first.initialize().await.expect("first initialize");
        let tenant_id = first.tenant_id().clone();
        let pending = first.pending_channel_key_id();
        drop(first);

        let mut reopened = Agent::open(node.clone(), config()).await.expect("reopen");
        reopened.initialize().await.expect("reinitialize");

        assert_eq!(reopened.tenant_id(), &tenant_id);
        assert_eq!(reopened.pending_channel_key_id(), pending);
        assert_eq!(node.state().nonce_calls, 1);
        assert_eq!(node.state().register_calls, 1);
    }

    #[tokio::test]
    async fn binding_is_persisted_and_consumes_open_material() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = FakeNode::default();
        node.insert_external_channel(channel_id());
        let mut agent = memory_agent(node.clone(), dir.path()).await;
        agent.initialize().await.expect("initialize");
        let pending = agent.pending_channel_key_id().expect("pending key");

        bind_pending(&mut agent, channel_id()).await;

        assert_eq!(agent.binding(channel_id()), Some(pending));
        assert!(agent.pending_channel_key_id().is_none());
        let status: AgentStatus =
            serde_json::from_slice(&fs::read(dir.path().join("status.json")).expect("read status"))
                .expect("status");
        assert!(status.channel_open_signer_material.is_none());
        assert_eq!(
            status.bound_channel_ids,
            vec![format!("{:#x}", channel_id())]
        );
        assert!(node
            .state()
            .tenant_tokens
            .iter()
            .all(|token| token == "tenant-token"));
    }

    #[tokio::test]
    async fn internal_channel_is_ignored_and_pending_signer_is_retained() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = FakeNode::default();
        {
            node.state()
                .statuses
                .insert(channel_id().into(), ChannelSigningStatus::Internal);
        }
        let mut agent = memory_agent(node, dir.path()).await;
        agent.initialize().await.expect("initialize");
        let pending = agent.pending_channel_key_id();

        agent.poll_once().await.expect("poll");

        assert_eq!(agent.pending_channel_key_id(), pending);
        assert_eq!(agent.binding(channel_id()), None);
    }

    #[tokio::test]
    async fn bound_request_is_signed_and_submitted_with_next_material() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = FakeNode::default();
        node.insert_external_channel(channel_id());
        let mut agent = memory_agent(node.clone(), dir.path()).await;
        agent.initialize().await.expect("initialize");
        let key_id = agent.pending_channel_key_id().expect("pending key");
        let signer = agent.root.open_channel(key_id).await.expect("open signer");
        let funding_tx = approved_funding_tx(funding_lock_for(
            signer.public_material().base_public_keys.funding_pubkey,
            convert_tests::remote_binding_pubkey(),
        ));
        let funding_outpoint = ckb_types::packed::OutPoint::new(funding_tx.calc_tx_hash(), 0);
        agent
            .bind_approved_funding(
                channel_id(),
                funding_tx,
                convert_tests::bound_shutdown_script(),
                0,
            )
            .await
            .expect("bind approved funding");
        let mut content = convert_tests::musig_content_for(&signer).await;
        content.content = fiber_lsp_sdk::Musig2SignableContent::CommitmentTransaction(
            convert_tests::commitment_spending(funding_outpoint),
        );
        node.state().statuses.insert(
            channel_id().into(),
            ChannelSigningStatus::SignatureRequired {
                request_id: JsonHash256([0x22; 32]),
                transition: ChannelSigningTransition::SendCommitmentSigned,
                content: musig2_to_rpc(&content),
            },
        );

        agent.poll_once().await.expect("poll and sign");

        let state = node.state();
        assert_eq!(state.submissions.len(), 1);
        assert_eq!(state.submissions[0].request_id, JsonHash256([0x22; 32]));
        assert_eq!(state.submissions[0].partial_signature.len(), 32);
        assert!(state.submissions[0].next_material.is_some());
    }
}
