use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use ckb_types::prelude::*;
use fiber_json_types::{
    ChannelOpenSignerMaterial as JsonChannelOpenSignerMaterial, SubmitChannelSignatureResult,
    WatchtowerSigningStatus,
};
use fiber_lsp_sdk::ChainVerifier;
use fiber_lsp_sdk::{
    json::open_material_to_rpc, ChannelKeyId, HostedSession, HostedSessionState, ProcessOutcome,
    RootKey, RootSigner, SignerStore, SigningPolicy, SubmitParams, TenantId,
};
use fiber_types::Hash256;
use serde::{Deserialize, Serialize};

use crate::rpc::FiberRpc;

struct RpcOpening<'a, R>(&'a R);

#[async_trait::async_trait]
impl<R: FiberRpc> fiber_lsp_sdk::TenantOpeningRpc for RpcOpening<'_, R> {
    async fn open_tenant_channel(
        &self,
        token: &str,
        request: fiber_json_types::OpenChannelWithExternalFundingParams,
    ) -> Result<fiber_json_types::OpenTenantChannelResult, fiber_lsp_sdk::SessionError> {
        self.0
            .open_tenant_channel(token, request)
            .await
            .map_err(session_error)
    }
}

fn session_error(error: impl std::fmt::Display) -> fiber_lsp_sdk::SessionError {
    fiber_lsp_sdk::SessionError::Invalid(error.to_string())
}

struct SessionFile<'a>(&'a Path);
impl SessionFile<'_> {
    fn save(&self, state: &HostedSessionState) -> Result<()> {
        let persisted = PersistedAgent {
            opening_request: state.opening_request.clone(),
            tenant_token: state.tenant_token.clone(),
            bindings: state
                .bindings
                .iter()
                .map(|(channel_id, key_id)| {
                    (format!("{channel_id:#x}"), format!("{:#x}", key_id.0))
                })
                .collect(),
            pending_channel_key_id: state.pending.map(|key_id| format!("{:#x}", key_id.0)),
        };
        atomic_write(self.0, &serde_json::to_vec_pretty(&persisted)?)
    }
}
#[async_trait::async_trait]
impl fiber_lsp_sdk::OpeningPersistence for SessionFile<'_> {
    async fn save_session(
        &self,
        state: &HostedSessionState,
    ) -> Result<(), fiber_lsp_sdk::SessionError> {
        self.save(state).map_err(session_error)
    }
}

struct FundingWallet<'a> {
    ckb_rpc_url: &'a str,
}
impl FundingWallet<'_> {
    async fn verify(
        &self,
        request: &fiber_json_types::OpenChannelWithExternalFundingParams,
        result: &fiber_json_types::OpenTenantChannelResult,
    ) -> Result<Vec<ckb_types::packed::OutPoint>> {
        let client = reqwest::Client::builder().no_proxy().build()?;
        let mut inputs = Vec::new();
        let mut wallet_capacity = 0u128;
        let mut wallet_tokens = 0u128;
        let token_amount = |data: &[u8]| -> Result<u128> {
            Ok(u128::from_le_bytes(
                data.try_into()
                    .context("funding token data must contain 16 bytes")?,
            ))
        };
        for input in &result.unsigned_funding_tx.inputs {
            let body: serde_json::Value = client.post(self.ckb_rpc_url).json(&serde_json::json!({
                "jsonrpc":"2.0", "id":1, "method":"get_live_cell", "params":[input.previous_output, true]
            })).send().await?.error_for_status()?.json().await?;
            anyhow::ensure!(
                body["result"]["status"] == "live",
                "funding input is not independently live"
            );
            let output: ckb_jsonrpc_types::CellOutput =
                serde_json::from_value(body["result"]["cell"]["output"].clone())?;
            if output.lock == request.funding_lock_script {
                wallet_capacity += u128::from(output.capacity.value());
                if output.type_.is_some() {
                    anyhow::ensure!(
                        output.type_ == request.funding_udt_type_script,
                        "funding spends an unapproved wallet asset"
                    );
                    let data: ckb_jsonrpc_types::JsonBytes =
                        serde_json::from_value(body["result"]["cell"]["data"]["content"].clone())?;
                    wallet_tokens = wallet_tokens
                        .checked_add(token_amount(data.as_bytes())?)
                        .context("funding token input overflow")?;
                }
            }
            inputs.push(input.previous_output.clone().into());
        }
        let change: u128 = result
            .unsigned_funding_tx
            .outputs
            .iter()
            .skip(1)
            .filter(|output| output.lock == request.funding_lock_script)
            .map(|output| u128::from(output.capacity.value()))
            .sum();
        if request.funding_udt_type_script.is_some() {
            let mut token_change = 0u128;
            for (output, data) in result
                .unsigned_funding_tx
                .outputs
                .iter()
                .zip(&result.unsigned_funding_tx.outputs_data)
                .skip(1)
            {
                if output.lock == request.funding_lock_script
                    && output.type_ == request.funding_udt_type_script
                {
                    token_change = token_change
                        .checked_add(token_amount(data.as_bytes())?)
                        .context("funding token change overflow")?;
                }
            }
            anyhow::ensure!(
                wallet_tokens.checked_sub(token_change) == Some(request.funding_amount),
                "funding token debit differs from approved amount"
            );
        }
        let contribution = if request.funding_udt_type_script.is_some() {
            u128::from(result.opening_context.local_reserved_ckb_amount)
        } else {
            request.funding_amount
        };
        let tx: ckb_types::packed::Transaction = result.unsigned_funding_tx.clone().into();
        let max_fee = u128::from(request.funding_fee_rate.unwrap_or(1000))
            * (tx.serialized_size_in_block() as u128 + 1024)
            / 1000;
        let debit = wallet_capacity
            .checked_sub(change)
            .ok_or_else(|| anyhow!("invalid wallet change"))?;
        anyhow::ensure!(
            debit >= contribution && debit <= contribution + max_fee,
            "funding wallet debit exceeds approved contribution and fee"
        );
        Ok(inputs)
    }
}
#[async_trait::async_trait]
impl fiber_lsp_sdk::FundingVerifier for FundingWallet<'_> {
    async fn verify_funding(
        &self,
        request: &fiber_json_types::OpenChannelWithExternalFundingParams,
        result: &fiber_json_types::OpenTenantChannelResult,
    ) -> Result<Vec<ckb_types::packed::OutPoint>, fiber_lsp_sdk::SessionError> {
        self.verify(request, result).await.map_err(session_error)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis()
        .try_into()
        .expect("timestamp fits u64")
}

const ROOT_KEY_FILE: &str = "root.key";
const AGENT_STATE_FILE: &str = "agent.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersistedAgent {
    tenant_token: Option<String>,
    bindings: HashMap<String, String>,
    pending_channel_key_id: Option<String>,
    opening_request: Option<Vec<u8>>,
}

/// Local E2E-driver intent, never accepted from an LSP signing response.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum FixtureAuthorization {
    /// Register wallet-owned invoice terms and prove possession of the preimage.
    Invoice {
        channel_id: Hash256,
        #[serde(deserialize_with = "deserialize_invoice_terms")]
        terms: fiber_lsp_sdk::PaymentAuthorization,
        preimage: Hash256,
    },
    /// Verify enforceable recovery before immediately sending a preimage to the LSP.
    ReleasePreimage {
        channel_id: Hash256,
        payment_hash: Hash256,
        preimage: Hash256,
        target: PreimageTarget,
    },
    /// Approve both fee shares for a plain CKB close at explicit fixture rates.
    Close {
        channel_id: Hash256,
        local_script: ckb_jsonrpc_types::Script,
        remote_script: ckb_jsonrpc_types::Script,
        local_fee_rate: u64,
        remote_fee_rate: u64,
    },
    /// Permit settlement to the fixture's sponsor wallet, within a local fee cap.
    Watchtower {
        channel_id: Hash256,
        destination: ckb_jsonrpc_types::Script,
        max_fee: u64,
    },
}

// serde's internally tagged enum buffer does not deserialize u128 directly.
// Keep SDK amounts intact by decoding this nested JSON object through serde_json.
fn deserialize_invoice_terms<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<fiber_lsp_sdk::PaymentAuthorization, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

/// LSP operation to invoke immediately after SDK preimage release authorization.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum PreimageTarget {
    /// Fulfill a received hold invoice off chain.
    Invoice,
    /// Provide the watchtower with a preimage for an on-chain claim.
    Watchtower,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WatchtowerApproval {
    destination: ckb_jsonrpc_types::Script,
    max_fee: u64,
}

/// A signature accepted by the watchtower RPC, including the exercised key path.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WatchtowerSubmission {
    pub request_id: String,
    pub key_purpose: fiber_json_types::OnchainKeyPurpose,
}

/// Fixture information consumed by the external E2E driver.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentStatus {
    pub tenant_id: String,
    pub root_signer_pubkey: fiber_json_types::Pubkey,
    pub tenant_token: String,
    pub channel_open_signer_material: Option<JsonChannelOpenSignerMaterial>,
    pub bound_channel_ids: Vec<String>,
    /// Requests and key purposes whose signatures were accepted by the watchtower RPC.
    pub watchtower_submissions: HashMap<String, Vec<WatchtowerSubmission>>,
    /// Revoked states with complete punishment signatures verified by the SDK.
    pub revoked_commitments: HashMap<String, Vec<fiber_lsp_sdk::CommitmentReference>>,
}

pub struct AgentConfig {
    pub store_dir: PathBuf,
    pub status_file: Option<PathBuf>,
}

/// Auto-approving test client backed by [`fiber_lsp_sdk::HostedSession`].
///
/// Uses [`SigningPolicy::Manual`] and explicitly confirms verified requests. Production
/// clients construct `HostedSession::new` (default `Auto`) and feed RPC
/// results in themselves.
pub struct Agent<R, S> {
    rpc: R,
    session: HostedSession<S>,
    config: AgentConfig,
    state_file: PathBuf,
    watchtower_approvals: HashMap<String, WatchtowerApproval>,
    watchtower_submissions: HashMap<String, Vec<WatchtowerSubmission>>,
    chain: Option<(crate::DevChain, Vec<ckb_types::packed::CellDep>)>,
}

impl<R: FiberRpc, S: SignerStore> Agent<R, S> {
    fn from_parts(
        rpc: R,
        root: RootSigner<S>,
        config: AgentConfig,
        persisted: PersistedAgent,
    ) -> Result<Self> {
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
        let session = HostedSession::new(root)
            .with_policy(SigningPolicy::Manual)
            .with_state(HostedSessionState {
                tenant_token: persisted.tenant_token,
                bindings,
                pending,
                opening_request: persisted.opening_request,
            });
        let approvals_path = config.store_dir.join("watchtower-approvals.json");
        let watchtower_approvals = if approvals_path.exists() {
            serde_json::from_slice(&fs::read(approvals_path)?)?
        } else {
            HashMap::new()
        };
        let submissions_path = config.store_dir.join("watchtower-submissions.json");
        let watchtower_submissions = if submissions_path.exists() {
            serde_json::from_slice(&fs::read(submissions_path)?)?
        } else {
            HashMap::new()
        };
        Ok(Self {
            watchtower_approvals,
            watchtower_submissions,
            chain: None,
            rpc,
            session,
            config,
            state_file,
        })
    }

    pub fn tenant_id(&self) -> TenantId {
        self.session.tenant_id()
    }

    pub fn pending_channel_key_id(&self) -> Option<ChannelKeyId> {
        self.session.pending_channel_key_id()
    }

    pub fn binding(&self, channel_id: Hash256) -> Option<ChannelKeyId> {
        self.session.binding(channel_id)
    }

    /// Reopen a channel signer. Test helper for constructing approved funding txs.
    pub async fn open_channel(
        &self,
        key_id: ChannelKeyId,
    ) -> Result<fiber_lsp_sdk::ChannelSigner<S>> {
        self.session
            .open_channel(key_id)
            .await
            .map_err(|error| anyhow!("open channel signer: {error}"))
    }

    /// Register the RootSigner tenant once and allocate channel open material.
    pub async fn initialize(&mut self) -> Result<()> {
        self.ensure_registered().await?;
        if self.session.state().bindings.is_empty() {
            self.session
                .allocate_pending_channel()
                .await
                .map_err(|error| anyhow!("allocate pending channel: {error}"))?;
        }
        self.persist_state()?;
        self.write_status().await
    }

    async fn ensure_registered(&mut self) -> Result<()> {
        if self.session.tenant_token().is_some() {
            return Ok(());
        }
        let identity: fiber_types::Pubkey = self.session.identity_public_key().into();
        let nonce = self
            .rpc
            .get_tenant_registry_nonce(identity.into())
            .await
            .context("request tenant registration nonce")?;
        let params = self
            .session
            .begin_registration(nonce)
            .map_err(|error| anyhow!("begin registration: {error}"))?;
        let registered = self
            .rpc
            .register_tenant(params)
            .await
            .context("register RootSigner tenant")?;
        self.session
            .finish_registration(registered)
            .map_err(|error| anyhow!("finish registration: {error}"))?;
        self.persist_state()?;
        Ok(())
    }

    /// Open through the production tenant RPC and validate before installing local state.
    pub async fn open_tenant_channel(
        &mut self,
        request: fiber_json_types::OpenChannelWithExternalFundingParams,
        network: &fiber_lsp_sdk::OpeningNetwork,
        ckb_rpc_url: &str,
    ) -> Result<fiber_json_types::OpenTenantChannelResult> {
        let result = self
            .session
            .open_tenant_channel(
                request,
                network,
                &RpcOpening(&self.rpc),
                &FundingWallet { ckb_rpc_url },
                &SessionFile(&self.state_file),
            )
            .await?;
        self.write_status().await?;
        Ok(result)
    }

    /// Install an independent dev-chain source and trusted contract dependencies.
    pub async fn configure_chain(&mut self, url: &str, contracts: &Path) -> Result<()> {
        let mut chain = crate::DevChain::new(url)?;
        let deps = chain.settlement_deps(contracts).await?;
        self.chain = Some((chain, deps));
        Ok(())
    }

    /// Apply explicit test-driver intent, verifying recovery before releasing preimages.
    pub async fn authorize(&mut self, request: FixtureAuthorization) -> Result<()> {
        match request {
            FixtureAuthorization::Invoice {
                channel_id,
                terms,
                preimage,
            } => {
                self.session
                    .channel_signer(channel_id)
                    .await?
                    .authorize_invoice(terms, preimage)
                    .await?;
            }
            FixtureAuthorization::ReleasePreimage {
                channel_id,
                payment_hash,
                preimage,
                target,
            } => {
                let (chain, _) = self
                    .chain
                    .as_ref()
                    .context("preimage release requires independent ChainVerifier")?;
                self.release_preimage_verified(channel_id, payment_hash, preimage, target, chain)
                    .await?;
            }
            FixtureAuthorization::Close {
                channel_id,
                local_script,
                remote_script,
                local_fee_rate,
                remote_fee_rate,
            } => {
                let key = self.binding(channel_id).context("unknown channel")?;
                let tx = ckb_types::core::TransactionBuilder::default()
                    .cell_deps(vec![ckb_types::packed::CellDep::default(); 2])
                    .input(ckb_types::packed::CellInput::default())
                    .outputs([local_script, remote_script].into_iter().map(|script| {
                        ckb_types::packed::CellOutput::new_builder()
                            .lock(Into::<ckb_types::packed::Script>::into(script))
                            .build()
                    }))
                    .outputs_data(vec![ckb_types::packed::Bytes::default(); 2])
                    .witness([0u8; 112].pack())
                    .build();
                let size = tx.data().serialized_size_in_block() as u64;
                let local_fee_share = local_fee_rate
                    .checked_mul(size)
                    .context("close fee overflow")?
                    / 1000;
                let remote_fee_share = remote_fee_rate
                    .checked_mul(size)
                    .context("close fee overflow")?
                    / 1000;
                self.open_channel(key)
                    .await?
                    .authorize_close(fiber_lsp_sdk::CloseAuthorization {
                        fee: local_fee_share
                            .checked_add(remote_fee_share)
                            .context("close fee overflow")?,
                        local_fee_share,
                        remote_fee_share,
                    })
                    .await?;
            }
            FixtureAuthorization::Watchtower {
                channel_id,
                destination,
                max_fee,
            } => {
                anyhow::ensure!(self.binding(channel_id).is_some(), "unknown channel");
                anyhow::ensure!(max_fee > 0, "fee cap must be positive");
                let mut approvals = self.watchtower_approvals.clone();
                approvals.insert(
                    format!("{channel_id:#x}"),
                    WatchtowerApproval {
                        destination,
                        max_fee,
                    },
                );
                atomic_write(
                    &self.config.store_dir.join("watchtower-approvals.json"),
                    &serde_json::to_vec_pretty(&approvals)?,
                )?;
                self.watchtower_approvals = approvals;
            }
        }
        Ok(())
    }

    async fn release_preimage_verified<C: ChainVerifier>(
        &self,
        channel_id: Hash256,
        payment_hash: Hash256,
        preimage: Hash256,
        target: PreimageTarget,
        chain: &C,
    ) -> Result<()> {
        let token = self
            .session
            .tenant_token()
            .context("agent is not registered")?;
        self.session
            .channel_signer(channel_id)
            .await?
            .authorize_preimage_release(payment_hash, preimage, now_ms(), chain)
            .await?;
        // Never send the secret before the SDK has durably authorized its release.
        match target {
            PreimageTarget::Invoice => {
                self.rpc
                    .settle_invoice(
                        token,
                        fiber_json_types::SettleInvoiceParams {
                            payment_hash: payment_hash.into(),
                            payment_preimage: preimage.into(),
                        },
                    )
                    .await?
            }
            PreimageTarget::Watchtower => {
                self.rpc
                    .create_preimage(
                        token,
                        fiber_json_types::CreatePreimageParams {
                            payment_hash: payment_hash.into(),
                            preimage: preimage.into(),
                        },
                    )
                    .await?
            }
        }
        Ok(())
    }

    /// Service outstanding signing requests for already-bound channels.
    pub async fn poll_once(&mut self) -> Result<()> {
        let token = self
            .session
            .tenant_token()
            .ok_or_else(|| anyhow!("agent is not registered"))?
            .to_string();
        let bound: Vec<_> = self.session.state().bindings.keys().copied().collect();
        for channel_id in bound {
            self.poll_channel(&token, channel_id).await?;
            self.poll_watchtower(&token, channel_id).await?;
        }
        let receipts = self.config.store_dir.join("watchtower-submissions.json");
        if receipts.exists() {
            self.watchtower_submissions = serde_json::from_slice(&fs::read(receipts)?)?;
        }
        self.write_status().await
    }

    async fn poll_channel(&mut self, token: &str, channel_id: Hash256) -> Result<()> {
        let status = self
            .rpc
            .get_channel_signing_status(token, channel_id.into())
            .await?
            .status;
        match self
            .session
            .handle_channel_status(channel_id, status, now_ms())
            .await
            .map_err(|error| anyhow!("handle channel status: {error}"))?
        {
            ProcessOutcome::Idle => Ok(()),
            ProcessOutcome::ReadyToSubmit(SubmitParams::Channel(params)) => {
                match self.rpc.submit_channel_signature(token, params).await? {
                    SubmitChannelSignatureResult::Applied
                    | SubmitChannelSignatureResult::AlreadyApplied => Ok(()),
                }
            }
            ProcessOutcome::ReadyToSubmit(SubmitParams::Watchtower(_)) => Err(anyhow!(
                "channel status produced a watchtower submit payload"
            )),
            ProcessOutcome::NeedConfirmation(pending) => {
                let SubmitParams::Channel(params) = self
                    .session
                    .confirm(pending, now_ms())
                    .await
                    .map_err(|e| anyhow!("confirm: {e}"))?
                else {
                    return Err(anyhow!("expected channel signature"));
                };
                self.rpc.submit_channel_signature(token, params).await?;
                Ok(())
            }
            ProcessOutcome::Denied => Err(anyhow!("signing policy denied a signing request")),
        }
    }

    async fn poll_watchtower(&mut self, token: &str, channel_id: Hash256) -> Result<()> {
        let status = match self
            .rpc
            .get_watchtower_signing_status(token, channel_id.into())
            .await
        {
            Ok(result) => result.status,
            Err(error) if error.to_string().contains("watched channel not found") => return Ok(()),
            Err(error) => return Err(error),
        };
        if matches!(status, WatchtowerSigningStatus::NoSignatureRequired) {
            return Ok(());
        }
        let approval = self
            .watchtower_approvals
            .get(&format!("{channel_id:#x}"))
            .context("watchtower request requires local authorization")?
            .clone();
        let (chain, deps) = self
            .chain
            .clone()
            .context("watchtower requires independent ChainVerifier")?;
        let WatchtowerSigningStatus::SignatureRequired { content, .. } = &status else {
            unreachable!()
        };
        let authorization = self
            .watchtower_authorization(channel_id, content, approval, &chain, deps)
            .await?;
        self.sign_watchtower_status(channel_id, status, authorization, &chain)
            .await
    }

    async fn watchtower_authorization<C: ChainVerifier>(
        &self,
        channel_id: Hash256,
        content: &fiber_json_types::OnchainSigningContent,
        approval: WatchtowerApproval,
        chain: &C,
        deps: Vec<ckb_types::packed::CellDep>,
    ) -> Result<fiber_lsp_sdk::OnchainSpendAuthorization> {
        let tx: ckb_types::packed::Transaction = content.transaction.clone().into();
        let first = tx
            .raw()
            .inputs()
            .get(0)
            .context("missing commitment input")?
            .previous_output();
        let signer = self
            .open_channel(self.binding(channel_id).context("unknown channel")?)
            .await?;
        let mut source = None;
        let mut lineage_errors = Vec::new();
        for record in signer.recovery_records().await? {
            match chain
                .verify_commitment_lineage(record.reference.tx_hash, &first)
                .await
            {
                Ok(()) => {
                    source = Some(record.reference);
                    break;
                }
                Err(error) => lineage_errors.push(error.to_string()),
            }
        }
        let source = source.ok_or_else(|| {
            anyhow!(
                "spend has no independently verified commitment ancestor: {}",
                lineage_errors.join("; ")
            )
        })?;
        let destination: ckb_types::packed::Script = approval.destination.into();
        let mut total = 0u64;
        let mut additional_inputs = Vec::new();
        for (index, input) in tx.raw().inputs().into_iter().enumerate() {
            let cell = chain.live_cell(&input.previous_output()).await?;
            if index > 0 {
                anyhow::ensure!(
                    cell.output.lock() == destination
                        && cell.output.type_().to_opt().is_none()
                        && cell.data.is_empty(),
                    "unapproved sponsor input"
                );
                additional_inputs.push(input.previous_output());
            }
            let capacity: u64 = cell.output.capacity().unpack();
            total = total.checked_add(capacity).context("capacity overflow")?;
        }
        for output in tx.raw().outputs() {
            let capacity: u64 = output.capacity().unpack();
            total = total.checked_sub(capacity).context("negative fee")?;
        }
        anyhow::ensure!(
            total <= approval.max_fee,
            "watchtower fee exceeds local cap"
        );
        Ok(fiber_lsp_sdk::OnchainSpendAuthorization {
            source,
            destination,
            fee: total,
            additional_inputs,
            cell_deps: deps,
        })
    }

    /// Sign a watchtower request only with locally approved terms and independent live chain evidence.
    pub async fn poll_watchtower_verified<C: fiber_lsp_sdk::ChainVerifier>(
        &self,
        channel_id: Hash256,
        authorization: fiber_lsp_sdk::OnchainSpendAuthorization,
        chain: &C,
    ) -> Result<()> {
        let token = self
            .session
            .tenant_token()
            .ok_or_else(|| anyhow!("agent is not registered"))?;
        let status = self
            .rpc
            .get_watchtower_signing_status(token, channel_id.into())
            .await?
            .status;
        self.sign_watchtower_status(channel_id, status, authorization, chain)
            .await
    }

    async fn sign_watchtower_status<C: fiber_lsp_sdk::ChainVerifier>(
        &self,
        channel_id: Hash256,
        status: WatchtowerSigningStatus,
        authorization: fiber_lsp_sdk::OnchainSpendAuthorization,
        chain: &C,
    ) -> Result<()> {
        let token = self
            .session
            .tenant_token()
            .context("agent is not registered")?;
        let key_purpose = match &status {
            WatchtowerSigningStatus::NoSignatureRequired => return Ok(()),
            WatchtowerSigningStatus::SignatureRequired { content, .. } => content.key_purpose,
        };
        let outcome = self
            .session
            .handle_watchtower_status_verified(channel_id, status, authorization, chain)
            .await
            .map_err(|e| anyhow!("prepare watchtower: {e}"))?;
        match outcome {
            ProcessOutcome::Idle => Ok(()),
            ProcessOutcome::NeedConfirmation(pending) => {
                let SubmitParams::Watchtower(params) = self
                    .session
                    .confirm_onchain(pending, chain)
                    .await
                    .map_err(|e| anyhow!("confirm watchtower: {e}"))?
                else {
                    return Err(anyhow!("expected watchtower signature"));
                };
                let request_id = format!("{:#x}", Hash256::from(params.request_id));
                self.rpc.submit_watchtower_signature(token, params).await?;
                let path = self.config.store_dir.join("watchtower-submissions.json");
                let mut receipts: HashMap<String, Vec<WatchtowerSubmission>> = if path.exists() {
                    serde_json::from_slice(&fs::read(&path)?)?
                } else {
                    HashMap::new()
                };
                let entries = receipts.entry(format!("{channel_id:#x}")).or_default();
                if !entries.iter().any(|entry| entry.request_id == request_id) {
                    entries.push(WatchtowerSubmission {
                        request_id,
                        key_purpose,
                    });
                }
                atomic_write(&path, &serde_json::to_vec_pretty(&receipts)?)?;
                Ok(())
            }
            _ => Err(anyhow!("watchtower must require explicit confirmation")),
        }
    }

    async fn write_status(&mut self) -> Result<()> {
        let Some(path) = &self.config.status_file.clone() else {
            return Ok(());
        };
        let material = if self.session.pending_channel_key_id().is_some() {
            Some(open_material_to_rpc(
                &self
                    .session
                    .allocate_pending_channel()
                    .await
                    .map_err(|error| anyhow!("create channel open material: {error}"))?,
            ))
        } else {
            None
        };
        let mut bound_channel_ids = self
            .session
            .state()
            .bindings
            .keys()
            .map(|channel_id| format!("{channel_id:#x}"))
            .collect::<Vec<_>>();
        bound_channel_ids.sort();
        let mut revoked_commitments = HashMap::new();
        for (id, key) in &self.session.state().bindings {
            let records = self
                .session
                .open_channel(*key)
                .await?
                .revocation_records()
                .await?;
            revoked_commitments.insert(
                format!("{id:#x}"),
                records
                    .into_iter()
                    .filter(|record| record.signature.is_some())
                    .map(|record| record.context.revoked)
                    .collect(),
            );
        }
        let status = AgentStatus {
            revoked_commitments,
            tenant_id: self.session.tenant_id().as_str().to_string(),
            root_signer_pubkey: fiber_types::Pubkey::from(self.session.identity_public_key())
                .into(),
            tenant_token: self
                .session
                .tenant_token()
                .ok_or_else(|| anyhow!("agent is not registered"))?
                .to_string(),
            channel_open_signer_material: material,
            bound_channel_ids,
            watchtower_submissions: self.watchtower_submissions.clone(),
        };
        atomic_write(path, &serde_json::to_vec_pretty(&status)?)
    }

    fn persist_state(&self) -> Result<()> {
        SessionFile(&self.state_file).save(self.session.state())
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
    use ckb_types::prelude::*;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use fiber_json_types::SubmitWatchtowerSignatureResult;
    use fiber_json_types::{
        ChannelSigningStatus, GetChannelSigningStatusResult, GetLspTenantRegistryNonceResult,
        Hash256 as JsonHash256, LspTenantRuntimeStatus, LspTenantStatus, RegisterLspTenantParams,
        RegisterLspTenantResult, SubmitChannelSignatureParams,
    };
    use fiber_lsp_sdk::{MemoryStore, RootKey, RootSigner, SignerStore};
    use fiber_types::{Privkey, TenantId, TenantRegistryPayload, TenantRegistrySignature};

    use super::*;
    use crate::convert::tests as convert_tests;

    #[derive(Clone)]
    struct FakeNode {
        inner: Arc<Mutex<FakeNodeState>>,
        lsp_key: Privkey,
    }

    #[derive(Default)]
    struct FakeNodeState {
        nonce_calls: usize,
        preimage_releases: usize,
        register_calls: usize,
        statuses: HashMap<JsonHash256, ChannelSigningStatus>,
        submissions: Vec<SubmitChannelSignatureParams>,
        watchtower_statuses: HashMap<JsonHash256, WatchtowerSigningStatus>,
        watchtower_submissions: Vec<fiber_json_types::SubmitWatchtowerSignatureParams>,
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
        async fn settle_invoice(
            &self,
            _token: &str,
            _params: fiber_json_types::SettleInvoiceParams,
        ) -> Result<()> {
            self.state().preimage_releases += 1;
            Ok(())
        }
        async fn create_preimage(
            &self,
            _token: &str,
            _params: fiber_json_types::CreatePreimageParams,
        ) -> Result<()> {
            self.state().preimage_releases += 1;
            Ok(())
        }

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
                access_token: "tenant-token".to_string(),
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

        async fn get_watchtower_signing_status(
            &self,
            tenant_token: &str,
            channel_id: JsonHash256,
        ) -> Result<fiber_json_types::GetWatchtowerSigningStatusResult> {
            let mut state = self.state();
            state.tenant_tokens.push(tenant_token.to_string());
            let status = state
                .watchtower_statuses
                .get(&channel_id)
                .cloned()
                .unwrap_or(WatchtowerSigningStatus::NoSignatureRequired);
            Ok(fiber_json_types::GetWatchtowerSigningStatusResult { channel_id, status })
        }

        async fn submit_watchtower_signature(
            &self,
            tenant_token: &str,
            params: fiber_json_types::SubmitWatchtowerSignatureParams,
        ) -> Result<SubmitWatchtowerSignatureResult> {
            let mut state = self.state();
            state.tenant_tokens.push(tenant_token.to_string());
            state.watchtower_submissions.push(params);
            Ok(SubmitWatchtowerSignatureResult::Applied)
        }
    }

    fn channel_id() -> Hash256 {
        Hash256::from([0x11; 32])
    }

    fn funding_lock_for(
        local: fiber_types::Pubkey,
        remote: fiber_types::Pubkey,
    ) -> ckb_types::packed::Script {
        use musig2::KeyAggContext;
        let mut keys = [local, remote];
        keys.sort();
        let ctx = KeyAggContext::new(keys).expect("aggregate keys");
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
                    .capacity(100_000_000_000u64)
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data()
    }

    async fn approve_pending<S: SignerStore>(
        agent: &mut Agent<FakeNode, S>,
    ) -> (Hash256, ckb_types::packed::Transaction) {
        let key_id = agent.pending_channel_key_id().expect("pending key");
        let signer = agent.open_channel(key_id).await.unwrap();
        let material = signer.channel_open_material(false).await.unwrap();
        let tx = approved_funding_tx(funding_lock_for(
            material.base_public_keys.funding_pubkey,
            convert_tests::remote_binding_pubkey(),
        ));
        let terms = convert_tests::parameters();
        let mut keys = [
            material.base_public_keys.tlc_base_key.serialize(),
            terms.remote_settlement_key.serialize(),
        ];
        keys.sort();
        let channel_id: Hash256 = fiber_types::blake2b_hash_with_salt(&keys.concat(), &[]).into();
        let request: fiber_json_types::OpenChannelWithExternalFundingParams = serde_json::from_value(serde_json::json!({
            "pubkey": terms.remote_funding_key, "public": false,
            "funding_amount": "0x9502f9000",
            "shutdown_script": ckb_jsonrpc_types::Script::from(convert_tests::bound_shutdown_script()),
            "funding_lock_script": ckb_jsonrpc_types::Script::from(convert_tests::bound_shutdown_script()),
            "commitment_delay_epoch":"0x10000000001", "commitment_fee_rate":"0x0",
            "external_channel_signer":fiber_lsp_sdk::json::open_material_to_rpc(&material),
        })).unwrap();
        let network = fiber_lsp_sdk::OpeningNetwork {
            funding_lock: ckb_types::packed::Script::default(),
            commitment_lock: terms.commitment_lock.clone(),
            commitment_cell_deps: 0,
            epoch_duration_ms: 2000,
            minimum_shutdown_fee: 100_000_000,
        };
        let result = fiber_json_types::OpenTenantChannelResult {
            channel_id: channel_id.into(),
            unsigned_funding_tx: tx.clone().into(),
            opening_context: fiber_json_types::TenantChannelOpeningContext {
                remote_funding_key: terms.remote_funding_key.into(),
                remote_settlement_key: terms.remote_settlement_key.into(),
                remote_shutdown_script: terms.remote_shutdown_script.into(),
                commitment_delay_epoch: request.commitment_delay_epoch.unwrap(),
                commitment_fee_rate: 0,
                local_amount: terms.local_amount,
                remote_amount: terms.remote_amount,
                local_reserved_ckb_amount: terms.local_reserved_ckb_amount,
                remote_reserved_ckb_amount: terms.remote_reserved_ckb_amount,
            },
        };
        struct Proposal(
            fiber_json_types::OpenTenantChannelResult,
            Vec<ckb_types::packed::OutPoint>,
        );
        #[async_trait::async_trait]
        impl fiber_lsp_sdk::TenantOpeningRpc for Proposal {
            async fn open_tenant_channel(
                &self,
                _token: &str,
                _request: fiber_json_types::OpenChannelWithExternalFundingParams,
            ) -> Result<fiber_json_types::OpenTenantChannelResult, fiber_lsp_sdk::SessionError>
            {
                Ok(self.0.clone())
            }
        }
        #[async_trait::async_trait]
        impl fiber_lsp_sdk::FundingVerifier for Proposal {
            async fn verify_funding(
                &self,
                _request: &fiber_json_types::OpenChannelWithExternalFundingParams,
                _result: &fiber_json_types::OpenTenantChannelResult,
            ) -> Result<Vec<ckb_types::packed::OutPoint>, fiber_lsp_sdk::SessionError> {
                Ok(self.1.clone())
            }
        }
        let fixture = Proposal(
            result,
            tx.raw()
                .inputs()
                .into_iter()
                .map(|i| i.previous_output())
                .collect(),
        );
        agent
            .session
            .open_tenant_channel(
                request,
                &network,
                &fixture,
                &fixture,
                &SessionFile(&agent.state_file),
            )
            .await
            .unwrap();
        agent.persist_state().unwrap();
        agent.write_status().await.unwrap();
        agent.rpc.insert_external_channel(channel_id);
        (channel_id, tx)
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

        assert_eq!(reopened.tenant_id(), tenant_id);
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

        let (id, _) = approve_pending(&mut agent).await;
        let channel_id = || id;

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
    async fn idle_channel_is_ignored_and_pending_signer_is_retained() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = FakeNode::default();
        {
            node.state().statuses.insert(
                channel_id().into(),
                ChannelSigningStatus::NoSignatureRequired,
            );
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
        let mut agent = Agent::open(
            node.clone(),
            AgentConfig {
                store_dir: dir.path().to_path_buf(),
                status_file: None,
            },
        )
        .await
        .unwrap();
        agent.initialize().await.expect("initialize");
        let (id, tx) = approve_pending(&mut agent).await;
        let channel_id = || id;
        let signer = agent
            .open_channel(agent.binding(channel_id()).unwrap())
            .await
            .unwrap();
        let status = convert_tests::commitment_status(&signer, &tx, true).await;
        let mut malicious = status.clone();
        let ChannelSigningStatus::SignatureRequired { settlement, .. } = &mut malicious else {
            panic!()
        };
        settlement.as_mut().unwrap().local_amount += 1;
        node.state().statuses.insert(channel_id().into(), malicious);
        let before = fs::read(dir.path().join("snapshot.bin")).unwrap();
        assert!(agent.poll_once().await.is_err());
        assert!(node.state().submissions.is_empty());
        assert_eq!(fs::read(dir.path().join("snapshot.bin")).unwrap(), before);
        node.state().statuses.insert(channel_id().into(), status);

        agent.poll_once().await.expect("poll and sign");

        let state = node.state();
        assert_eq!(state.submissions.len(), 1);
        assert_eq!(state.submissions[0].request_id, JsonHash256([0x22; 32]));
        assert_eq!(state.submissions[0].partial_signature.len(), 32);
        assert!(state.submissions[0].next_material.is_some());
    }

    #[tokio::test]
    async fn invoice_authorization_rejects_wrong_proof_and_never_releases_unprotected_preimage() {
        let dir = tempfile::tempdir().unwrap();
        let node = FakeNode::default();
        let mut agent = Agent::open(
            node.clone(),
            AgentConfig {
                store_dir: dir.path().into(),
                status_file: None,
            },
        )
        .await
        .unwrap();
        agent.initialize().await.unwrap();
        let (id, funding) = approve_pending(&mut agent).await;
        let preimage = Hash256::from([0x22; 32]);
        let payment_hash = fiber_types::HashAlgorithm::CkbHash
            .hash(preimage.as_ref())
            .into();
        let terms = fiber_lsp_sdk::PaymentAuthorization {
            payment_hash,
            hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
            inbound: true,
            amount: 1_000_000,
            expires_at_ms: now_ms() + 3_600_000,
            min_tlc_expiry_delta_ms: 60_000,
            max_tlc_expiry_ms: now_ms() + 86_400_000,
        };
        let request = FixtureAuthorization::Invoice {
            channel_id: id,
            terms: terms.clone(),
            preimage,
        };
        let decoded: FixtureAuthorization =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert!(matches!(decoded, FixtureAuthorization::Invoice { .. }));
        let before = fs::read(dir.path().join("snapshot.bin")).unwrap();
        assert!(agent
            .authorize(FixtureAuthorization::Invoice {
                channel_id: id,
                terms: terms.clone(),
                preimage: Hash256::from([0x33; 32]),
            })
            .await
            .is_err());
        assert_eq!(fs::read(dir.path().join("snapshot.bin")).unwrap(), before);
        agent
            .authorize(FixtureAuthorization::Invoice {
                channel_id: id,
                terms: terms.clone(),
                preimage,
            })
            .await
            .unwrap();
        let before = fs::read(dir.path().join("snapshot.bin")).unwrap();
        let mut changed = terms;
        changed.amount += 1;
        assert!(agent
            .authorize(FixtureAuthorization::Invoice {
                channel_id: id,
                terms: changed,
                preimage
            })
            .await
            .is_err());
        let mut chain = convert_tests::Chain {
            point: ckb_types::packed::OutPoint::new(funding.calc_tx_hash(), 0),
            cell: fiber_lsp_sdk::VerifiedCell {
                output: funding.raw().outputs().get(0).unwrap(),
                data: funding
                    .raw()
                    .outputs_data()
                    .get(0)
                    .unwrap()
                    .raw_data()
                    .to_vec(),
            },
            live: true,
        };
        for target in [PreimageTarget::Invoice, PreimageTarget::Watchtower] {
            let error = agent
                .release_preimage_verified(id, payment_hash, preimage, target, &chain)
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("no recoverable local commitment"));
        }
        chain.live = false;
        assert!(agent
            .release_preimage_verified(id, payment_hash, preimage, PreimageTarget::Invoice, &chain)
            .await
            .is_err());
        assert_eq!(node.state().preimage_releases, 0);
        assert_eq!(fs::read(dir.path().join("snapshot.bin")).unwrap(), before);
    }

    #[tokio::test]
    async fn agent_recovers_pending_watchtower_signature_after_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = FakeNode::default();
        node.insert_external_channel(channel_id());
        let config = || AgentConfig {
            store_dir: dir.path().to_path_buf(),
            status_file: Some(dir.path().join("status.json")),
        };
        let mut first = Agent::open(node.clone(), config())
            .await
            .expect("first open");
        first.initialize().await.expect("first initialize");
        let (id, funding) = approve_pending(&mut first).await;
        let channel_id = || id;
        let key = first.binding(channel_id()).unwrap();
        let signer = first.open_channel(key).await.unwrap();
        let status = convert_tests::commitment_status(&signer, &funding, false).await;
        node.state().statuses.insert(channel_id().into(), status);
        first.poll_once().await.unwrap();
        let records = signer.recovery_records().await.unwrap();
        let (status, authorization, mut chain) = convert_tests::settlement(&records[0]);
        node.state().statuses.insert(
            channel_id().into(),
            ChannelSigningStatus::NoSignatureRequired,
        );
        node.state()
            .watchtower_statuses
            .insert(channel_id().into(), status);
        drop(signer);
        drop(first);
        let request_id = JsonHash256([0x33; 32]);

        let mut reopened = Agent::open(node.clone(), config()).await.expect("reopen");
        reopened.initialize().await.expect("reinitialize");
        let restored = reopened.open_channel(key).await.unwrap();
        assert_eq!(
            serde_json::to_vec(&restored.recovery_records().await.unwrap()).unwrap(),
            serde_json::to_vec(&records).unwrap()
        );
        let before = fs::read(dir.path().join("snapshot.bin")).unwrap();
        assert!(
            reopened.poll_once().await.is_err(),
            "missing chain context must reject"
        );
        chain.live = false;
        assert!(reopened
            .poll_watchtower_verified(channel_id(), authorization.clone(), &chain)
            .await
            .is_err());
        assert!(node.state().watchtower_submissions.is_empty());
        assert_eq!(fs::read(dir.path().join("snapshot.bin")).unwrap(), before);
        chain.live = true;
        let WatchtowerSigningStatus::SignatureRequired { content, .. } =
            node.state().watchtower_statuses[&channel_id().into()].clone()
        else {
            panic!()
        };
        let mut approval = WatchtowerApproval {
            destination: authorization.destination.clone().into(),
            max_fee: 4,
        };
        assert!(reopened
            .watchtower_authorization(channel_id(), &content, approval.clone(), &chain, vec![])
            .await
            .unwrap_err()
            .to_string()
            .contains("fee exceeds local cap"));
        approval.max_fee = 5;
        let checked = reopened
            .watchtower_authorization(channel_id(), &content, approval.clone(), &chain, vec![])
            .await
            .unwrap();
        assert_eq!(checked.fee, authorization.fee);
        let mut wrong_destination = checked.clone();
        wrong_destination.destination = ckb_types::packed::Script::default();
        assert!(reopened
            .poll_watchtower_verified(channel_id(), wrong_destination, &chain)
            .await
            .is_err());
        assert!(node.state().watchtower_submissions.is_empty());
        assert_eq!(fs::read(dir.path().join("snapshot.bin")).unwrap(), before);
        reopened
            .authorize(FixtureAuthorization::Watchtower {
                channel_id: channel_id(),
                destination: approval.destination,
                max_fee: approval.max_fee,
            })
            .await
            .unwrap();
        drop(reopened);
        let reopened = Agent::open(node.clone(), config()).await.unwrap();
        assert_eq!(
            reopened.watchtower_approvals[&format!("{:#x}", channel_id())].max_fee,
            5
        );
        reopened
            .poll_watchtower_verified(channel_id(), checked, &chain)
            .await
            .expect("checked watchtower after restart");

        let state = node.state();
        assert_eq!(state.watchtower_submissions.len(), 1);
        assert_eq!(state.watchtower_submissions[0].request_id, request_id);
        assert_eq!(
            state.watchtower_submissions[0].channel_id,
            channel_id().into()
        );
        assert_eq!(state.watchtower_submissions[0].signature.len(), 65);
    }
}
