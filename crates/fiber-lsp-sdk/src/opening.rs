//! Client-owned opening intent and validation of the frozen tenant proposal.
use ckb_types::{
    core::TransactionBuilder,
    packed::{CellDep, CellInput, CellOutput, Script},
    prelude::*,
};
use fiber_json_types::{OpenChannelWithExternalFundingParams, OpenTenantChannelResult};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{commitment::invalid, CommitmentParameters, SignerError};

/// Trusted network configuration, supplied by the wallet rather than the LSP.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpeningNetwork {
    /// FundingLock deployment, with empty arguments.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub funding_lock: Script,
    /// CommitmentLock deployment, with empty arguments.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub commitment_lock: Script,
    /// Number of deps in a commitment transaction for this asset.
    pub commitment_cell_deps: usize,
    /// Trusted epoch duration in milliseconds.
    pub epoch_duration_ms: u64,
    /// Protocol reserve added to occupied settlement capacity, in shannons.
    pub minimum_shutdown_fee: u64,
}

/// Transport for the production tenant opening RPC. No local approvals are RPCs.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait TenantOpeningRpc: Sync {
    /// Send the saved opening intent using the tenant's credentials.
    async fn open_tenant_channel(
        &self,
        token: &str,
        request: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenTenantChannelResult, crate::SessionError>;
}

/// Independent wallet validation of the complete funding transaction.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait FundingVerifier: Sync {
    /// Resolve inputs independently and enforce wallet debit, asset and fee limits.
    /// Return the exact approved inputs in transaction order. Never approve inputs
    /// merely because the LSP included them in its response.
    async fn verify_funding(
        &self,
        request: &OpenChannelWithExternalFundingParams,
        result: &OpenTenantChannelResult,
    ) -> Result<Vec<ckb_types::packed::OutPoint>, crate::SessionError>;
}

/// Durable session storage used before network I/O and before returning success.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait OpeningPersistence: Sync {
    /// Atomically persist the session map; errors stop opening progression.
    async fn save_session(
        &self,
        state: &crate::HostedSessionState,
    ) -> Result<(), crate::SessionError>;
}

impl<S: crate::SignerStore> crate::ChannelSigner<S> {
    /// Validate the tenant proposal against the original wallet request and trusted network.
    /// The funding wallet must approve the complete transaction, including its net
    /// debit and fee; `expected_inputs` binds that approval to the selected cells.
    pub(crate) async fn verify_and_record_opening(
        &self,
        request: &OpenChannelWithExternalFundingParams,
        result: &OpenTenantChannelResult,
        network: &OpeningNetwork,
        expected_inputs: &[ckb_types::packed::OutPoint],
    ) -> Result<(), SignerError> {
        if request.public != Some(false) {
            return Err(invalid("tenant opening requires public: false"));
        }
        let expected = crate::json::open_material_to_rpc(&self.channel_open_material(false).await?);
        if serde_json::to_value(Some(expected)).map_err(|e| invalid(&e.to_string()))?
            != serde_json::to_value(&request.external_channel_signer)
                .map_err(|e| invalid(&e.to_string()))?
        {
            return Err(invalid("opening request does not belong to this signer"));
        }
        let parameters = validate_opening(request, result, network)?;
        self.approve_opening(
            &result.unsigned_funding_tx.clone().into(),
            0,
            request.shutdown_script.clone().into(),
            expected_inputs,
            parameters,
        )
        .await
    }
}

fn reserve(
    script: Script,
    asset: Option<Script>,
    network: &OpeningNetwork,
) -> Result<u64, SignerError> {
    let lock = if script.args().len() < 57 {
        Script::new_builder().args([0u8; 57].pack()).build()
    } else {
        script
    };
    let data = if asset.is_some() { 16 } else { 0 };
    let capacity = CellOutput::new_builder()
        .lock(lock)
        .type_(asset.pack())
        .build()
        .occupied_capacity(
            ckb_types::core::Capacity::bytes(data).map_err(|e| invalid(&e.to_string()))?,
        )
        .map_err(|e| invalid(&e.to_string()))?
        .as_u64();
    capacity
        .checked_add(network.minimum_shutdown_fee)
        .ok_or_else(|| invalid("reserve overflow"))
}

pub(crate) fn validate_opening(
    request: &OpenChannelWithExternalFundingParams,
    result: &OpenTenantChannelResult,
    network: &OpeningNetwork,
) -> Result<CommitmentParameters, SignerError> {
    let context = &result.opening_context;
    if !network.funding_lock.args().is_empty() || !network.commitment_lock.args().is_empty() {
        return Err(invalid("network contract scripts must have empty args"));
    }
    let funding: ckb_types::packed::Transaction = result.unsigned_funding_tx.clone().into();
    let output = funding
        .raw()
        .outputs()
        .get(0)
        .ok_or_else(|| invalid("missing funding output"))?;
    let asset: Option<Script> = request.funding_udt_type_script.clone().map(Into::into);
    if output.type_().to_opt() != asset || context.local_amount != request.funding_amount {
        return Err(invalid(
            "opening asset or local balance differs from requested funding",
        ));
    }
    if output.lock().code_hash() != network.funding_lock.code_hash()
        || output.lock().hash_type() != network.funding_lock.hash_type()
    {
        return Err(invalid("funding output uses an untrusted contract"));
    }
    // Counterparty inputs can have their own change outputs. The external funding
    // wallet approves the complete transaction before this opening is installed;
    // commitment validation must not assume every change output belongs to us.
    let delay = request
        .commitment_delay_epoch
        .ok_or_else(|| invalid("client must choose commitment delay"))?;
    let fee_rate = request
        .commitment_fee_rate
        .ok_or_else(|| invalid("client must choose commitment fee rate"))?;
    if context.commitment_delay_epoch != delay || context.commitment_fee_rate != fee_rate {
        return Err(invalid(
            "LSP changed requested commitment delay or fee rate",
        ));
    }
    let local_shutdown: Script = request.shutdown_script.clone().into();
    let remote_shutdown: Script = context.remote_shutdown_script.clone().into();
    if context.local_reserved_ckb_amount != reserve(local_shutdown, asset.clone(), network)?
        || context.remote_reserved_ckb_amount
            != reserve(remote_shutdown.clone(), asset.clone(), network)?
    {
        return Err(invalid("opening reserve differs from protocol reserve"));
    }
    let local_key = fiber_types::Pubkey::try_from(
        request
            .external_channel_signer
            .as_ref()
            .ok_or_else(|| invalid("opening requires external_channel_signer"))?
            .base_public_keys
            .tlc_base_key,
    )
    .map_err(|e| invalid(&e))?;
    let remote_key =
        fiber_types::Pubkey::try_from(context.remote_settlement_key).map_err(|e| invalid(&e))?;
    let mut keys = [local_key.serialize(), remote_key.serialize()];
    keys.sort();
    let channel_id: fiber_types::Hash256 =
        fiber_types::blake2b_hash_with_salt(&keys.concat(), &[]).into();
    if channel_id != result.channel_id.into() {
        return Err(invalid(
            "channel id does not match the opening settlement keys",
        ));
    }
    let commitment_output = CellOutput::new_builder()
        .lock(
            network
                .commitment_lock
                .clone()
                .as_builder()
                .args([0u8; 57].pack())
                .build(),
        )
        .type_(asset.clone().pack())
        .build();
    let data = if asset.is_some() {
        0u128.to_le_bytes().to_vec()
    } else {
        vec![]
    };
    let tx = TransactionBuilder::default()
        .cell_deps(vec![CellDep::default(); network.commitment_cell_deps])
        .input(CellInput::default())
        .output(commitment_output)
        .output_data(data.pack())
        .witness([0u8; 112].pack())
        .build();
    let fee = (u128::from(fee_rate) * tx.data().serialized_size_in_block() as u128) / 1000;
    Ok(CommitmentParameters {
        remote_funding_key: fiber_types::Pubkey::try_from(context.remote_funding_key)
            .map_err(|e| invalid(&e))?,
        remote_settlement_key: remote_key,
        remote_shutdown_script: remote_shutdown,
        commitment_lock: network.commitment_lock.clone(),
        delay_epoch: (0xa000_0000_0000_0000u64 | delay.value()).to_le_bytes(),
        epoch_duration_ms: network.epoch_duration_ms,
        commitment_fee: u64::try_from(fee).map_err(|_| invalid("commitment fee overflow"))?,
        local_amount: context.local_amount,
        remote_amount: context.remote_amount,
        local_reserved_ckb_amount: context.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: context.remote_reserved_ckb_amount,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{ChannelSigner, HostedSession, MemoryStore, RootKey, RootSigner};

    struct OpeningAdapters {
        request: OpenChannelWithExternalFundingParams,
        result: OpenTenantChannelResult,
        inputs: Vec<ckb_types::packed::OutPoint>,
        failure: &'static str,
        events: std::sync::Mutex<Vec<&'static str>>,
        saved: std::sync::Mutex<Option<crate::HostedSessionState>>,
    }

    #[async_trait::async_trait]
    impl TenantOpeningRpc for OpeningAdapters {
        async fn open_tenant_channel(
            &self,
            token: &str,
            request: OpenChannelWithExternalFundingParams,
        ) -> Result<OpenTenantChannelResult, crate::SessionError> {
            assert_eq!(token, "tenant-token");
            assert_eq!(
                serde_json::to_value(request).unwrap(),
                serde_json::to_value(&self.request).unwrap()
            );
            assert!(self
                .saved
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .opening_request
                .is_some());
            self.events.lock().unwrap().push("rpc");
            if self.failure == "rpc" {
                return Err(crate::SessionError::Invalid("rpc failed".into()));
            }
            Ok(self.result.clone())
        }
    }
    #[async_trait::async_trait]
    impl FundingVerifier for OpeningAdapters {
        async fn verify_funding(
            &self,
            _request: &OpenChannelWithExternalFundingParams,
            _result: &OpenTenantChannelResult,
        ) -> Result<Vec<ckb_types::packed::OutPoint>, crate::SessionError> {
            self.events.lock().unwrap().push("wallet");
            if self.failure == "wallet" {
                return Err(crate::SessionError::Invalid("wallet rejected".into()));
            }
            Ok(self.inputs.clone())
        }
    }
    #[async_trait::async_trait]
    impl OpeningPersistence for OpeningAdapters {
        async fn save_session(
            &self,
            state: &crate::HostedSessionState,
        ) -> Result<(), crate::SessionError> {
            let stage = if state.bindings.is_empty() {
                "save intent"
            } else {
                "save binding"
            };
            self.events.lock().unwrap().push(stage);
            if self.failure == stage {
                return Err(crate::SessionError::Invalid("storage failed".into()));
            }
            *self.saved.lock().unwrap() = Some(state.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn opening_entrypoint_verifies_and_persists_before_returning() {
        let created = RootSigner::in_memory().await.unwrap();
        let mut session =
            HostedSession::new(created.root_signer).with_state(crate::HostedSessionState {
                tenant_token: Some("tenant-token".into()),
                ..Default::default()
            });
        session.allocate_pending_channel().await.unwrap();
        let signer = session
            .open_channel(session.pending_channel_key_id().unwrap())
            .await
            .unwrap();
        let (request, result, network, inputs) = proposal(&signer).await;
        let adapters = OpeningAdapters {
            request: request.clone(),
            result,
            inputs,
            failure: "",
            events: Default::default(),
            saved: Default::default(),
        };
        session
            .open_tenant_channel(request.clone(), &network, &adapters, &adapters, &adapters)
            .await
            .unwrap();
        assert_eq!(
            *adapters.events.lock().unwrap(),
            ["save intent", "rpc", "wallet", "save binding"]
        );
        assert_eq!(
            adapters.saved.lock().unwrap().as_ref().unwrap(),
            session.state()
        );
        assert_eq!(session.state().bindings.len(), 1);
        assert!(session.state().pending.is_none());
        let mut different = request.clone();
        different.funding_amount += 1;
        assert!(session
            .open_tenant_channel(different, &network, &adapters, &adapters, &adapters)
            .await
            .is_err());
        session
            .open_tenant_channel(request, &network, &adapters, &adapters, &adapters)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opening_failures_stop_progress_and_allow_retry_after_restart() {
        for failure in [
            "save intent",
            "rpc",
            "wallet",
            "context",
            "inputs",
            "save binding",
        ] {
            let store = MemoryStore::default();
            let created = RootSigner::create_random(store.clone()).await.unwrap();
            let key = created.root_key_backup.expose_secret();
            let mut session =
                HostedSession::new(created.root_signer).with_state(crate::HostedSessionState {
                    tenant_token: Some("tenant-token".into()),
                    ..Default::default()
                });
            session.allocate_pending_channel().await.unwrap();
            let before = session.state().clone();
            let signer = session
                .open_channel(session.pending_channel_key_id().unwrap())
                .await
                .unwrap();
            let (request, result, network, inputs) = proposal(&signer).await;
            let mut adapters = OpeningAdapters {
                request: request.clone(),
                result: result.clone(),
                inputs: inputs.clone(),
                failure,
                events: Default::default(),
                saved: Default::default(),
            };
            if failure == "context" {
                adapters.result.opening_context.local_amount += 1;
            }
            if failure == "inputs" {
                adapters.inputs.clear();
            }
            assert!(
                session
                    .open_tenant_channel(request.clone(), &network, &adapters, &adapters, &adapters)
                    .await
                    .is_err(),
                "{failure}"
            );
            let events = adapters.events.lock().unwrap().clone();
            if failure == "save intent" {
                assert_eq!(events, ["save intent"]);
            }
            if failure == "rpc" {
                assert_eq!(events, ["save intent", "rpc"]);
            }
            if failure != "save binding" {
                assert!(session.state().bindings.is_empty());
            }
            let restored = adapters.saved.lock().unwrap().clone().unwrap_or(before);
            let root = RootSigner::open(RootKey::import(key).unwrap(), store)
                .await
                .unwrap();
            let mut session = HostedSession::new(root).with_state(restored);
            adapters.failure = "";
            adapters.result = result;
            adapters.inputs = inputs;
            session
                .open_tenant_channel(request, &network, &adapters, &adapters, &adapters)
                .await
                .unwrap();
            assert_eq!(session.state().bindings.len(), 1);
        }
    }

    #[tokio::test]
    async fn tenant_opening_rejects_public_or_missing_signer_without_binding() {
        let created = RootSigner::in_memory().await.unwrap();
        let mut session = HostedSession::new(created.root_signer);
        session.allocate_pending_channel().await.unwrap();
        let signer = session
            .open_channel(session.pending_channel_key_id().unwrap())
            .await
            .unwrap();
        let (request, result, network, inputs) = proposal(&signer).await;
        for bad in [
            {
                let mut bad = request.clone();
                bad.public = None;
                bad
            },
            {
                let mut bad = request.clone();
                bad.public = Some(true);
                bad
            },
            {
                let mut bad = request.clone();
                bad.external_channel_signer = None;
                bad
            },
        ] {
            assert!(session.begin_channel_opening(bad.clone()).await.is_err());
            assert!(session.state().opening_request.is_none());
            assert!(signer
                .verify_and_record_opening(&bad, &result, &network, &inputs)
                .await
                .is_err());
        }
        session.begin_channel_opening(request).await.unwrap();
        session
            .finish_channel_opening(&result, &network, &inputs)
            .await
            .unwrap();
    }

    pub(crate) async fn proposal(
        signer: &ChannelSigner<MemoryStore>,
    ) -> (
        OpenChannelWithExternalFundingParams,
        OpenTenantChannelResult,
        OpeningNetwork,
        Vec<ckb_types::packed::OutPoint>,
    ) {
        let material = signer.channel_open_material(false).await.unwrap();
        let remote = fiber_types::InMemorySigner::generate_from_seed(b"opening peer");
        let script = Script::new_builder().code_hash([3; 32].pack()).build();
        let network = OpeningNetwork {
            funding_lock: script.clone(),
            commitment_lock: Script::new_builder().code_hash([4; 32].pack()).build(),
            commitment_cell_deps: 1,
            epoch_duration_ms: 14_400_000,
            minimum_shutdown_fee: 100_000_000,
        };
        let request: OpenChannelWithExternalFundingParams = serde_json::from_value(serde_json::json!({
            "pubkey": remote.funding_key.pubkey(), "public": false,
            "funding_amount":"0xba43b7400", "shutdown_script": ckb_jsonrpc_types::Script::from(script.clone()),
            "funding_lock_script": ckb_jsonrpc_types::Script::from(script.clone()),
            "commitment_delay_epoch":"0x10000000001", "commitment_fee_rate":"0x3e8",
            "external_channel_signer":crate::json::open_material_to_rpc(&material),
        })).unwrap();
        let ctx = musig2::KeyAggContext::new([
            material.base_public_keys.funding_pubkey,
            remote.funding_key.pubkey(),
        ])
        .unwrap();
        let point: musig2::secp::Point = ctx.aggregated_pubkey();
        let hash = fiber_types::blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
        let mut keys = [
            material.base_public_keys.tlc_base_key.serialize(),
            remote.tlc_base_key.pubkey().serialize(),
        ];
        keys.sort();
        let id: fiber_types::Hash256 =
            fiber_types::blake2b_hash_with_salt(&keys.concat(), &[]).into();
        let inputs = vec![ckb_types::packed::OutPoint::new([7; 32].pack(), 0)];
        let tx = TransactionBuilder::default()
            .input(CellInput::new(inputs[0].clone(), 0))
            .output(
                CellOutput::new_builder()
                    .capacity(100_000_000_000u64)
                    .lock(
                        script
                            .clone()
                            .as_builder()
                            .args(hash[..20].to_vec().pack())
                            .build(),
                    )
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        let result = OpenTenantChannelResult {
            channel_id: id.into(),
            unsigned_funding_tx: tx.into(),
            opening_context: fiber_json_types::TenantChannelOpeningContext {
                remote_funding_key: remote.funding_key.pubkey().into(),
                remote_settlement_key: remote.tlc_base_key.pubkey().into(),
                remote_shutdown_script: script.clone().into(),
                commitment_delay_epoch: request.commitment_delay_epoch.unwrap(),
                commitment_fee_rate: 1000,
                local_amount: 50_000_000_000,
                remote_amount: 50_000_000_000,
                local_reserved_ckb_amount: reserve(script.clone(), None, &network).unwrap(),
                remote_reserved_ckb_amount: reserve(script, None, &network).unwrap(),
            },
        };
        (request, result, network, inputs)
    }

    #[tokio::test]
    async fn udt_opening_survives_session_restart_and_rejects_wrong_asset_or_capacity() {
        let store = MemoryStore::default();
        let key = [32; 32];
        let root = RootSigner::create(RootKey::import(key).unwrap(), store.clone())
            .await
            .unwrap();
        let mut session = HostedSession::new(root);
        session.allocate_pending_channel().await.unwrap();
        let signer = session
            .open_channel(session.pending_channel_key_id().unwrap())
            .await
            .unwrap();
        let (mut request, mut result, mut network, inputs) = proposal(&signer).await;
        let asset = Script::new_builder()
            .code_hash([9; 32].pack())
            .args([1u8; 32].as_slice().pack())
            .build();
        request.funding_udt_type_script = Some(asset.clone().into());
        network.commitment_cell_deps += 1;
        let local = reserve(
            request.shutdown_script.clone().into(),
            Some(asset.clone()),
            &network,
        )
        .unwrap();
        let remote = reserve(
            result.opening_context.remote_shutdown_script.clone().into(),
            Some(asset.clone()),
            &network,
        )
        .unwrap();
        result.opening_context.local_reserved_ckb_amount = local;
        result.opening_context.remote_reserved_ckb_amount = remote;
        result.unsigned_funding_tx.outputs[0].capacity = (local + remote).into();
        result.unsigned_funding_tx.outputs[0].type_ = Some(asset.into());
        result.unsigned_funding_tx.outputs_data[0] =
            ckb_jsonrpc_types::JsonBytes::from_vec(100_000_000_000u128.to_le_bytes().to_vec());
        session
            .begin_channel_opening(request.clone())
            .await
            .unwrap();
        let state = serde_json::from_slice(&serde_json::to_vec(session.state()).unwrap()).unwrap();
        drop(session);
        let root = RootSigner::open(RootKey::import(key).unwrap(), store.clone())
            .await
            .unwrap();
        let mut session = HostedSession::new(root).with_state(state);
        for bad in [
            {
                let mut bad = result.clone();
                bad.unsigned_funding_tx.outputs[0].capacity = (local + remote + 1).into();
                bad
            },
            {
                let mut bad = result.clone();
                bad.unsigned_funding_tx.outputs[0].type_ = None;
                bad
            },
            {
                let mut bad = result.clone();
                bad.unsigned_funding_tx.outputs_data[0] =
                    ckb_jsonrpc_types::JsonBytes::from_vec(1u128.to_le_bytes().to_vec());
                bad
            },
        ] {
            assert!(session
                .finish_channel_opening(&bad, &network, &inputs)
                .await
                .is_err());
            assert!(session.state().bindings.is_empty());
        }
        session
            .finish_channel_opening(&result, &network, &inputs)
            .await
            .unwrap();
        session.begin_channel_opening(request).await.unwrap();
        assert!(session.pending_channel_key_id().is_none());
        session
            .finish_channel_opening(&result, &network, &inputs)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opening_rejects_malicious_proposals_without_binding() {
        let root = RootSigner::create(RootKey::import([31; 32]).unwrap(), MemoryStore::default())
            .await
            .unwrap();
        let mut session = HostedSession::new(root);
        session.allocate_pending_channel().await.unwrap();
        let signer = session
            .open_channel(session.pending_channel_key_id().unwrap())
            .await
            .unwrap();
        let (request, result, network, inputs) = proposal(&signer).await;
        session.begin_channel_opening(request).await.unwrap();
        for field in [
            "balance",
            "reserve",
            "fee",
            "delay",
            "channel_id",
            "funding_key",
            "settlement_key",
            "shutdown",
        ] {
            let mut bad = result.clone();
            match field {
                "balance" => {
                    bad.opening_context.local_amount -= 1;
                    bad.opening_context.remote_amount += 1;
                }
                "reserve" => bad.opening_context.local_reserved_ckb_amount += 1,
                "fee" => bad.opening_context.commitment_fee_rate += 1,
                "delay" => bad.opening_context.commitment_delay_epoch = 2.into(),
                "channel_id" => bad.channel_id = fiber_types::Hash256::from([8; 32]).into(),
                "funding_key" => {
                    bad.opening_context.remote_funding_key = signer
                        .public_material()
                        .base_public_keys
                        .funding_pubkey
                        .into()
                }
                "settlement_key" => {
                    bad.opening_context.remote_settlement_key = signer
                        .public_material()
                        .base_public_keys
                        .tlc_base_key
                        .into()
                }
                "shutdown" => {
                    bad.opening_context.remote_shutdown_script.args =
                        ckb_jsonrpc_types::JsonBytes::from_vec(vec![1; 100]);
                }
                _ => unreachable!(),
            }
            assert!(
                session
                    .finish_channel_opening(&bad, &network, &inputs)
                    .await
                    .is_err(),
                "{field}"
            );
            assert!(session.state().bindings.is_empty());
            assert!(session.pending_channel_key_id().is_some());
        }
        assert!(session
            .finish_channel_opening(&result, &network, &[])
            .await
            .is_err());
        session
            .finish_channel_opening(&result, &network, &inputs)
            .await
            .unwrap();
        session
            .finish_channel_opening(&result, &network, &inputs)
            .await
            .unwrap();
        let mut changed = result.clone();
        changed.unsigned_funding_tx.inputs[0].previous_output.index = 1.into();
        let changed_inputs = vec![ckb_types::packed::OutPoint::new([7; 32].pack(), 1)];
        assert!(session
            .finish_channel_opening(&changed, &network, &changed_inputs)
            .await
            .is_err());
    }
}
