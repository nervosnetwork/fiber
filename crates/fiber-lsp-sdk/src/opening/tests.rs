use super::*;
use crate::{ChannelSigner, HostedSession, MemoryStore, RootKey, RootSigner};

struct OpeningAdapters {
    request: OpenChannelWithExternalFundingParams,
    result: OpenChannelWithExternalFundingResult,
    inputs: Vec<ckb_types::packed::OutPoint>,
    failure: &'static str,
    events: std::sync::Mutex<Vec<&'static str>>,
    saved: std::sync::Mutex<Option<crate::HostedSessionState>>,
}

#[async_trait::async_trait]
impl TenantOpeningRpc for OpeningAdapters {
    async fn open_channel_with_external_funding(
        &self,
        token: &str,
        request: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, crate::SessionError> {
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
        _result: &OpenChannelWithExternalFundingResult,
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
            adapters
                .result
                .opening_context
                .as_mut()
                .unwrap()
                .local_amount += 1;
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
async fn interrupted_opening_preserves_its_intent_after_restart() {
    let store = MemoryStore::default();
    let created = RootSigner::create_random(store.clone()).await.unwrap();
    let key = created.root_key_backup.expose_secret();
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
    let mut adapters = OpeningAdapters {
        request: request.clone(),
        result,
        inputs,
        failure: "rpc",
        events: Default::default(),
        saved: Default::default(),
    };
    assert!(session
        .open_tenant_channel(request.clone(), &network, &adapters, &adapters, &adapters)
        .await
        .is_err());
    let saved = adapters.saved.lock().unwrap().clone().unwrap();
    let root = RootSigner::open(RootKey::import(key).unwrap(), store)
        .await
        .unwrap();
    let mut session = HostedSession::new(root).with_state(saved.clone());

    let mut changed_amount = request.clone();
    changed_amount.funding_amount += 1;
    let mut changed_asset = request.clone();
    changed_asset.funding_udt_type_script = Some(Script::default().into());
    for changed in [changed_amount, changed_asset] {
        adapters.events.lock().unwrap().clear();
        adapters.request = changed.clone();
        assert!(session
            .open_tenant_channel(changed, &network, &adapters, &adapters, &adapters)
            .await
            .is_err());
        assert!(
            adapters.events.lock().unwrap().is_empty(),
            "conflicting intent must be rejected before persistence or RPC"
        );
        assert_eq!(session.state(), &saved);
        assert_eq!(adapters.saved.lock().unwrap().as_ref(), Some(&saved));
    }

    adapters.request = request.clone();
    adapters.failure = "";
    session
        .open_tenant_channel(request, &network, &adapters, &adapters, &adapters)
        .await
        .unwrap();
    assert_eq!(session.state().bindings.len(), 1);
}

#[tokio::test]
async fn abandoning_rejected_opening_retires_pending_signer_and_allows_fresh_retry() {
    let store = MemoryStore::default();
    let created = RootSigner::create_random(store.clone()).await.unwrap();
    let mut session =
        HostedSession::new(created.root_signer).with_state(crate::HostedSessionState {
            tenant_token: Some("tenant-token".into()),
            ..Default::default()
        });

    // 1. Allocate first pending signer and fail on a rejected RPC.
    session.allocate_pending_channel().await.unwrap();
    let retired_key = session.pending_channel_key_id().unwrap();
    let first_signer = session.open_channel(retired_key).await.unwrap();
    let (first_request, first_result, network, first_inputs) = proposal(&first_signer).await;
    let adapters = OpeningAdapters {
        request: first_request.clone(),
        result: first_result,
        inputs: first_inputs,
        failure: "rpc",
        events: Default::default(),
        saved: Default::default(),
    };
    assert!(session
        .open_tenant_channel(
            first_request.clone(),
            &network,
            &adapters,
            &adapters,
            &adapters
        )
        .await
        .is_err());
    assert_eq!(session.pending_channel_key_id(), Some(retired_key));
    assert!(session.state().opening_request.is_some());

    // 2. Retrying with corrected params before abandoning fails with the same-signer error.
    let mut corrected_same_signer = first_request.clone();
    corrected_same_signer.funding_amount += 1_000_000_000;
    let err = session
        .open_tenant_channel(
            corrected_same_signer,
            &network,
            &adapters,
            &adapters,
            &adapters,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot replace opening intent for the same signer"),
        "unexpected error: {err}"
    );

    // 3. Abandoning retires the pending signer and clears the rejected opening intent.
    let abandoned = session.abandon_pending_channel_opening().await.unwrap();
    assert_eq!(abandoned, retired_key);
    assert!(session.pending_channel_key_id().is_none());
    assert!(session.state().opening_request.is_none());
    assert_eq!(
        session.abandon_pending_channel_opening().await.unwrap_err(),
        crate::SessionError::NoPendingChannel
    );

    // 4. Allocating again yields a fresh signer and binds cleanly with corrected params.
    session.allocate_pending_channel().await.unwrap();
    let fresh_key = session.pending_channel_key_id().unwrap();
    assert_ne!(fresh_key, retired_key);
    let fresh_signer = session.open_channel(fresh_key).await.unwrap();
    let (mut second_request, mut second_result, second_network, second_inputs) =
        proposal(&fresh_signer).await;
    second_request.funding_amount += 1_000_000_000;
    let second_context = second_result.opening_context.as_mut().unwrap();
    second_context.local_amount = second_request.funding_amount;
    second_result.unsigned_funding_tx.outputs[0].capacity =
        u64::try_from(second_context.local_amount + second_context.remote_amount)
            .unwrap()
            .into();
    let second_adapters = OpeningAdapters {
        request: second_request.clone(),
        result: second_result.clone(),
        inputs: second_inputs,
        failure: "",
        events: Default::default(),
        saved: Default::default(),
    };
    session
        .open_tenant_channel(
            second_request,
            &second_network,
            &second_adapters,
            &second_adapters,
            &second_adapters,
        )
        .await
        .unwrap();
    assert!(session.pending_channel_key_id().is_none());
    assert_eq!(session.state().bindings.len(), 1);
    assert_eq!(
        session.binding(second_result.channel_id.into()),
        Some(fresh_key)
    );
}

#[tokio::test]
async fn abandoning_pending_opening_preserves_bound_channels_and_rejects_bound_pending() {
    let store = MemoryStore::default();
    let created = RootSigner::create_random(store).await.unwrap();
    let mut session =
        HostedSession::new(created.root_signer).with_state(crate::HostedSessionState {
            tenant_token: Some("tenant-token".into()),
            ..Default::default()
        });

    // Bind a first channel.
    session.allocate_pending_channel().await.unwrap();
    let bound_key = session.pending_channel_key_id().unwrap();
    let first_signer = session.open_channel(bound_key).await.unwrap();
    let (first_request, first_result, network, first_inputs) = proposal(&first_signer).await;
    let adapters = OpeningAdapters {
        request: first_request.clone(),
        result: first_result.clone(),
        inputs: first_inputs,
        failure: "",
        events: Default::default(),
        saved: Default::default(),
    };
    session
        .open_tenant_channel(first_request, &network, &adapters, &adapters, &adapters)
        .await
        .unwrap();
    let retained_first_intent = session.state().opening_request.clone();
    assert!(retained_first_intent.is_some());

    // Abandoning with no pending channel fails and leaves bound state intact.
    assert_eq!(
        session.abandon_pending_channel_opening().await.unwrap_err(),
        crate::SessionError::NoPendingChannel
    );
    assert_eq!(
        session.binding(first_result.channel_id.into()),
        Some(bound_key)
    );
    assert_eq!(session.state().opening_request, retained_first_intent);

    // Allocate a second pending signer and abandon before sending: keeps first channel's intent.
    session.allocate_pending_channel().await.unwrap();
    let unused_pending = session.pending_channel_key_id().unwrap();
    assert_ne!(unused_pending, bound_key);
    assert_eq!(
        session.abandon_pending_channel_opening().await.unwrap(),
        unused_pending
    );
    assert_eq!(
        session.binding(first_result.channel_id.into()),
        Some(bound_key)
    );
    assert_eq!(session.state().opening_request, retained_first_intent);

    // If persistence fails after signer binding is recorded, abandoning that pending signer is rejected.
    let store2 = MemoryStore::default();
    let created2 = RootSigner::create_random(store2.clone()).await.unwrap();
    let root_key2 = created2.root_key_backup.expose_secret();
    let mut session2 =
        HostedSession::new(created2.root_signer).with_state(crate::HostedSessionState {
            tenant_token: Some("tenant-token".into()),
            ..Default::default()
        });
    session2.allocate_pending_channel().await.unwrap();
    let third_key = session2.pending_channel_key_id().unwrap();
    let third_signer = session2.open_channel(third_key).await.unwrap();
    let (third_request, third_result, third_network, third_inputs) = proposal(&third_signer).await;
    let save_binding_fail = OpeningAdapters {
        request: third_request.clone(),
        result: third_result,
        inputs: third_inputs,
        failure: "save binding",
        events: Default::default(),
        saved: Default::default(),
    };
    assert!(session2
        .open_tenant_channel(
            third_request,
            &third_network,
            &save_binding_fail,
            &save_binding_fail,
            &save_binding_fail,
        )
        .await
        .is_err());
    let saved_before_binding = save_binding_fail.saved.lock().unwrap().clone().unwrap();
    let reopened_root = RootSigner::open(RootKey::import(root_key2).unwrap(), store2)
        .await
        .unwrap();
    let mut recovered = HostedSession::new(reopened_root).with_state(saved_before_binding);
    assert_eq!(recovered.pending_channel_key_id(), Some(third_key));
    let err = recovered
        .abandon_pending_channel_opening()
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("already has a recorded channel binding"),
        "unexpected error: {err}"
    );
    assert_eq!(recovered.pending_channel_key_id(), Some(third_key));
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
    OpenChannelWithExternalFundingResult,
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
    let id: fiber_types::Hash256 = fiber_types::blake2b_hash_with_salt(&keys.concat(), &[]).into();
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
    let result = OpenChannelWithExternalFundingResult {
        channel_id: id.into(),
        unsigned_funding_tx: tx.into(),
        opening_context: Some(fiber_json_types::TenantChannelOpeningContext {
            remote_funding_key: remote.funding_key.pubkey().into(),
            remote_settlement_key: remote.tlc_base_key.pubkey().into(),
            remote_shutdown_script: script.clone().into(),
            commitment_delay_epoch: request.commitment_delay_epoch.unwrap(),
            commitment_fee_rate: 1000,
            local_amount: 50_000_000_000,
            remote_amount: 50_000_000_000,
            local_reserved_ckb_amount: reserve(script.clone(), None, &network).unwrap(),
            remote_reserved_ckb_amount: reserve(script, None, &network).unwrap(),
        }),
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
        result
            .opening_context
            .as_ref()
            .unwrap()
            .remote_shutdown_script
            .clone()
            .into(),
        Some(asset.clone()),
        &network,
    )
    .unwrap();
    result
        .opening_context
        .as_mut()
        .unwrap()
        .local_reserved_ckb_amount = local;
    result
        .opening_context
        .as_mut()
        .unwrap()
        .remote_reserved_ckb_amount = remote;
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
async fn subsequent_channel_opening_is_allowed_for_the_same_tenant() {
    let created = RootSigner::in_memory().await.unwrap();
    let mut session = HostedSession::new(created.root_signer);
    session.allocate_pending_channel().await.unwrap();
    let first_signer = session
        .open_channel(session.pending_channel_key_id().unwrap())
        .await
        .unwrap();
    let (first_request, first_result, first_network, first_inputs) = proposal(&first_signer).await;
    session.begin_channel_opening(first_request).await.unwrap();
    session
        .finish_channel_opening(&first_result, &first_network, &first_inputs)
        .await
        .unwrap();
    assert_eq!(session.state().bindings.len(), 1);

    session.allocate_pending_channel().await.unwrap();
    let second_signer = session
        .open_channel(session.pending_channel_key_id().unwrap())
        .await
        .unwrap();
    let (second_request, second_result, second_network, second_inputs) =
        proposal(&second_signer).await;
    session.begin_channel_opening(second_request).await.unwrap();
    session
        .finish_channel_opening(&second_result, &second_network, &second_inputs)
        .await
        .unwrap();
    assert_eq!(session.state().bindings.len(), 2);
    assert_ne!(
        first_result.channel_id, second_result.channel_id,
        "each channel opener must use a distinct channel id"
    );
}

#[tokio::test]
async fn retrying_completed_opening_after_allocating_next_pending_preserves_pending() {
    let created = RootSigner::in_memory().await.unwrap();
    let mut session =
        HostedSession::new(created.root_signer).with_state(crate::HostedSessionState {
            tenant_token: Some("tenant-token".into()),
            ..Default::default()
        });

    // Complete opening for channel 1.
    session.allocate_pending_channel().await.unwrap();
    let first_key = session.pending_channel_key_id().unwrap();
    let first_signer = session.open_channel(first_key).await.unwrap();
    let (first_request, first_result, network, first_inputs) = proposal(&first_signer).await;
    let first_adapters = OpeningAdapters {
        request: first_request.clone(),
        result: first_result.clone(),
        inputs: first_inputs,
        failure: "",
        events: Default::default(),
        saved: Default::default(),
    };
    session
        .open_tenant_channel(
            first_request.clone(),
            &network,
            &first_adapters,
            &first_adapters,
            &first_adapters,
        )
        .await
        .unwrap();
    assert_eq!(
        session.binding(first_result.channel_id.into()),
        Some(first_key)
    );
    assert!(session.pending_channel_key_id().is_none());

    // Allocate a new pending signer key for channel 2.
    session.allocate_pending_channel().await.unwrap();
    let second_key = session.pending_channel_key_id().unwrap();
    assert_ne!(second_key, first_key);

    // Retrying the completed opening for channel 1 must use channel 1's existing
    // binding and leave channel 2's pending key intact.
    session
        .open_tenant_channel(
            first_request,
            &network,
            &first_adapters,
            &first_adapters,
            &first_adapters,
        )
        .await
        .unwrap();
    assert_eq!(
        session.binding(first_result.channel_id.into()),
        Some(first_key)
    );
    assert_eq!(
        session.pending_channel_key_id(),
        Some(second_key),
        "retrying an already-bound channel must preserve the newly allocated pending key"
    );

    // The preserved pending key can still be used to open channel 2 cleanly.
    let second_signer = session.open_channel(second_key).await.unwrap();
    let (second_request, second_result, second_network, second_inputs) =
        proposal(&second_signer).await;
    let second_adapters = OpeningAdapters {
        request: second_request.clone(),
        result: second_result.clone(),
        inputs: second_inputs,
        failure: "",
        events: Default::default(),
        saved: Default::default(),
    };
    session
        .open_tenant_channel(
            second_request,
            &second_network,
            &second_adapters,
            &second_adapters,
            &second_adapters,
        )
        .await
        .unwrap();
    assert!(session.pending_channel_key_id().is_none());
    assert_eq!(
        session.binding(first_result.channel_id.into()),
        Some(first_key)
    );
    assert_eq!(
        session.binding(second_result.channel_id.into()),
        Some(second_key)
    );
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
        "missing_context",
    ] {
        let mut bad = result.clone();
        match field {
            "balance" => {
                bad.opening_context.as_mut().unwrap().local_amount -= 1;
                bad.opening_context.as_mut().unwrap().remote_amount += 1;
            }
            "reserve" => {
                bad.opening_context
                    .as_mut()
                    .unwrap()
                    .local_reserved_ckb_amount += 1
            }
            "fee" => bad.opening_context.as_mut().unwrap().commitment_fee_rate += 1,
            "delay" => bad.opening_context.as_mut().unwrap().commitment_delay_epoch = 2.into(),
            "channel_id" => bad.channel_id = fiber_types::Hash256::from([8; 32]).into(),
            "funding_key" => {
                bad.opening_context.as_mut().unwrap().remote_funding_key = signer
                    .public_material()
                    .base_public_keys
                    .funding_pubkey
                    .into()
            }
            "settlement_key" => {
                bad.opening_context.as_mut().unwrap().remote_settlement_key = signer
                    .public_material()
                    .base_public_keys
                    .tlc_base_key
                    .into()
            }
            "shutdown" => {
                bad.opening_context
                    .as_mut()
                    .unwrap()
                    .remote_shutdown_script
                    .args = ckb_jsonrpc_types::JsonBytes::from_vec(vec![1; 100]);
            }
            "missing_context" => bad.opening_context = None,
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
