use super::*;
use crate::tests::test_utils::generate_store;
use fiber_types::{Hash256, HashAlgorithm, Privkey, SettlementData, SettlementTlc, TLCId};

fn snapshot(count: usize) -> SettlementData {
    SettlementData {
        local_amount: 1,
        remote_amount: 2,
        tlcs: (0..count)
            .map(|id| SettlementTlc {
                tlc_id: TLCId::Offered(id as u64),
                hash_algorithm: HashAlgorithm::CkbHash,
                payment_amount: 1,
                payment_hash: Hash256::from([3; 32]),
                expiry: 1000,
                local_key: None,
                local_key_pubkey: Some(Privkey::from([1; 32]).pubkey()),
                local_key_commitment_number: Some(1),
                remote_key: Privkey::from([2; 32]).pubkey(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn tenant_watch_channel_rejects_oversized_snapshot_without_writing() {
    let (store, _dir) = generate_store();
    let server = WatchtowerRpcServerImpl::new(store.clone());
    let node = NodeId::from_bytes(vec![1, 2, 3]);
    let channel = Hash256::from([4; 32]);
    let ctx = RpcContext {
        node_id: node.to_string(),
        tenant_scoped: true,
    };
    let params = |count| CreateWatchChannelParams {
        channel_id: channel.into(),
        funding_udt_type_script: None,
        local_settlement_key: None,
        local_settlement_key_pubkey: Some(Privkey::from([1; 32]).pubkey().into()),
        remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
        local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
        remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
        settlement_data: snapshot(count).into(),
        channel_features: Default::default(),
        payout_lock_script: None,
    };
    // Exercise JSON decoding/conversion as well as the tenant RPC handler.
    let wire = serde_json::to_string(&params(255)).unwrap();
    let error = server
        .create_watch_channel(ctx.clone(), serde_json::from_str(&wire).unwrap())
        .await
        .unwrap_err();
    assert!(error.message().contains("max 254"), "{error}");
    assert!(store.get_watch_channel(&node, &channel).is_none());
    assert!(store.get_watch_channels_with_nodes().is_empty());

    server
        .create_watch_channel(ctx.clone(), params(254))
        .await
        .unwrap();
    let before = store.get_watch_channel(&node, &channel).unwrap();
    assert_eq!(before.local_settlement_data.tlcs.len(), 254);
    for update in [
        server
            .update_local_settlement(
                ctx.clone(),
                UpdateLocalSettlementParams {
                    channel_id: channel.into(),
                    settlement_data: snapshot(255).into(),
                },
            )
            .await,
        server
            .update_pending_remote_settlement(
                ctx.clone(),
                UpdatePendingRemoteSettlementParams {
                    channel_id: channel.into(),
                    settlement_data: snapshot(255).into(),
                },
            )
            .await,
    ] {
        assert!(update.unwrap_err().message().contains("max 254"));
    }
    // Store writers must enforce the same boundary even without the RPC facade.
    store.update_local_settlement(node.clone(), channel, snapshot(255));
    store.update_pending_remote_settlement(node.clone(), channel, snapshot(255));
    let after = store.get_watch_channel(&node, &channel).unwrap();
    assert_eq!(after.local_settlement_data, before.local_settlement_data);
    assert_eq!(
        after.pending_remote_settlement_data,
        before.pending_remote_settlement_data
    );
    assert_eq!(after.remote_settlement_data, before.remote_settlement_data);

    // Oversized payout_lock_script args must be rejected.
    use ckb_types::prelude::{Builder, Entity, Pack};
    let mut huge_payout_params = params(1);
    huge_payout_params.channel_id = Hash256::from([9; 32]).into();
    huge_payout_params.payout_lock_script = Some(
        ckb_types::packed::Script::new_builder()
            .args([0u8; 2048].as_slice().pack())
            .build()
            .into(),
    );
    let err = server
        .create_watch_channel(ctx.clone(), huge_payout_params)
        .await
        .unwrap_err();
    assert!(
        err.message().contains("payout_lock_script args exceeds"),
        "{err}"
    );
}

#[tokio::test]
async fn tenant_watch_channel_and_preimage_quotas_enforced_and_host_exempt() {
    use crate::lsp_config::WatchtowerQuotas;

    let (store, _dir) = generate_store();
    let quotas = WatchtowerQuotas {
        max_channels_per_node: 2,
        max_channels_total: 3,
        max_preimages_per_node: 2,
        max_preimages_total: 3,
    };
    let server = WatchtowerRpcServerImpl::new(store.clone()).with_quotas(quotas);

    let tenant_a = NodeId::from_bytes(Privkey::from([10; 32]).pubkey().serialize().to_vec());
    let tenant_b = NodeId::from_bytes(Privkey::from([11; 32]).pubkey().serialize().to_vec());
    let ctx_a = RpcContext {
        node_id: tenant_a.to_string(),
        tenant_scoped: true,
    };
    let ctx_b = RpcContext {
        node_id: tenant_b.to_string(),
        tenant_scoped: true,
    };
    let ctx_host = RpcContext {
        node_id: NodeId::local().to_string(),
        tenant_scoped: false,
    };

    let channel_params = |channel_byte: u8, local_key: bool| CreateWatchChannelParams {
        channel_id: Hash256::from([channel_byte; 32]).into(),
        funding_udt_type_script: None,
        local_settlement_key: local_key.then(|| Privkey::from([1; 32]).into()),
        local_settlement_key_pubkey: (!local_key).then(|| Privkey::from([1; 32]).pubkey().into()),
        remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
        local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
        remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
        settlement_data: snapshot(1).into(),
        channel_features: Default::default(),
        payout_lock_script: None,
    };

    // Tenant A fills per-node channel quota (2).
    server
        .create_watch_channel(ctx_a.clone(), channel_params(1, false))
        .await
        .unwrap();
    server
        .create_watch_channel(ctx_a.clone(), channel_params(2, false))
        .await
        .unwrap();
    // Updating an existing channel_id must still succeed at quota boundary.
    server
        .create_watch_channel(ctx_a.clone(), channel_params(1, false))
        .await
        .unwrap();
    // 3rd channel for Tenant A is rejected.
    let err = server
        .create_watch_channel(ctx_a.clone(), channel_params(3, false))
        .await
        .unwrap_err();
    assert!(err.message().contains("quota exceeded"), "{err}");
    assert!(store
        .get_watch_channel(&tenant_a, &Hash256::from([3; 32]))
        .is_none());

    // Tenant B can add 1 channel (total = 3), then hits the global cap.
    server
        .create_watch_channel(ctx_b.clone(), channel_params(4, false))
        .await
        .unwrap();
    let err = server
        .create_watch_channel(ctx_b.clone(), channel_params(5, false))
        .await
        .unwrap_err();
    assert!(err.message().contains("quota exceeded"), "{err}");

    // Removing one channel from Tenant A frees global capacity for Tenant B.
    server
        .remove_watch_channel(
            ctx_a.clone(),
            RemoveWatchChannelParams {
                channel_id: Hash256::from([2; 32]).into(),
            },
        )
        .await
        .unwrap();
    server
        .create_watch_channel(ctx_b.clone(), channel_params(5, false))
        .await
        .unwrap();

    // Host (NodeId::local()) bypasses tenant channel quotas.
    server
        .create_watch_channel(ctx_host.clone(), channel_params(6, true))
        .await
        .unwrap();
    assert!(store
        .get_watch_channel(&NodeId::local(), &Hash256::from([6; 32]))
        .is_some());

    // Preimage RPC quotas: per-node (2), global (3), removal frees slot, host exempt.
    let preimage_params = |seed: u8| {
        let preimage = Hash256::from([seed; 32]);
        let payment_hash: Hash256 = HashAlgorithm::Sha256.hash(preimage.as_ref()).into();
        CreatePreimageParams {
            payment_hash: payment_hash.into(),
            preimage: preimage.into(),
        }
    };

    let p1 = preimage_params(21);
    let p2 = preimage_params(22);
    let p3 = preimage_params(23);
    let p4 = preimage_params(24);
    let p5 = preimage_params(25);
    let p6 = preimage_params(26);

    server
        .create_preimage(ctx_a.clone(), p1.clone())
        .await
        .unwrap();
    server
        .create_preimage(ctx_a.clone(), p2.clone())
        .await
        .unwrap();
    // Overwriting existing payment_hash succeeds at quota boundary.
    server
        .create_preimage(ctx_a.clone(), p1.clone())
        .await
        .unwrap();
    // 3rd preimage for Tenant A fails.
    let err = server
        .create_preimage(ctx_a.clone(), p3.clone())
        .await
        .unwrap_err();
    assert!(err.message().contains("quota exceeded"), "{err}");

    // Tenant B adds 1 (total = 3), then hits global cap.
    server
        .create_preimage(ctx_b.clone(), p4.clone())
        .await
        .unwrap();
    let err = server
        .create_preimage(ctx_b.clone(), p5.clone())
        .await
        .unwrap_err();
    assert!(err.message().contains("quota exceeded"), "{err}");

    // Removing one preimage from Tenant A frees a slot for Tenant B.
    server
        .remove_preimage(
            ctx_a.clone(),
            RemovePreimageParams {
                payment_hash: p2.payment_hash,
            },
        )
        .await
        .unwrap();
    server
        .create_preimage(ctx_b.clone(), p5.clone())
        .await
        .unwrap();

    // Host bypasses tenant preimage quotas.
    server.create_preimage(ctx_host, p6.clone()).await.unwrap();
    assert_eq!(
        store.get_watch_preimage(&NodeId::local(), &p6.payment_hash.into()),
        Some(p6.preimage.into())
    );
}

#[tokio::test]
async fn watchtower_signing_status_exposes_backoff_and_pending_ttl() {
    use ckb_types::core::TransactionBuilder;
    use ckb_types::packed::OutPoint;
    use fiber_types::{OnchainKeyPurpose, OnchainSigningContent, WatchtowerSignerState};

    let (store, _dir) = generate_store();
    let server = WatchtowerRpcServerImpl::new(store.clone());
    let tenant = NodeId::from_bytes(Privkey::from([31; 32]).pubkey().serialize().to_vec());
    let ctx = RpcContext {
        node_id: tenant.to_string(),
        tenant_scoped: true,
    };

    let ch1 = Hash256::from([41; 32]);
    let ch2 = Hash256::from([42; 32]);
    for ch in [ch1, ch2] {
        server
            .create_watch_channel(
                ctx.clone(),
                CreateWatchChannelParams {
                    channel_id: ch.into(),
                    funding_udt_type_script: None,
                    local_settlement_key: None,
                    local_settlement_key_pubkey: Some(Privkey::from([1; 32]).pubkey().into()),
                    remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
                    local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
                    remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
                    settlement_data: snapshot(1).into(),
                    channel_features: Default::default(),
                    payout_lock_script: None,
                },
            )
            .await
            .unwrap();
    }

    // Initially healthy: NoSignatureRequired.
    let initial = server
        .get_watchtower_signing_status(
            ctx.clone(),
            GetWatchtowerSigningStatusParams {
                channel_id: ch1.into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(initial.status, WatchtowerSigningStatus::NoSignatureRequired);

    // Populate pending request + future backoff on ch1.
    let req_id = Hash256::from([99; 32]);
    let now_ms = crate::now_timestamp_as_millis_u64();
    let created_at_ms = now_ms;
    let retry_after_ms = now_ms + 300_000;
    store
        .update_watchtower_signer(&tenant, &ch1, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                let tx = TransactionBuilder::default()
                    .input(ckb_types::packed::CellInput::new(OutPoint::default(), 0))
                    .build()
                    .data();
                ext.pending_requests.insert(
                    req_id,
                    OnchainSigningContent {
                        key_purpose: OnchainKeyPurpose::Settlement,
                        transaction: tx,
                    },
                );
                ext.pending_request_created_at_ms
                    .insert(req_id, created_at_ms);
                ext.consecutive_expired_requests = 2;
                ext.retry_after_ms = retry_after_ms;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    // ch1 has a pending request -> SignatureRequired with exact expires_at_ms.
    let status_ch1 = server
        .get_watchtower_signing_status(
            ctx.clone(),
            GetWatchtowerSigningStatusParams {
                channel_id: ch1.into(),
            },
        )
        .await
        .unwrap();
    match status_ch1.status {
        WatchtowerSigningStatus::SignatureRequired {
            request_id,
            expires_at_ms,
            ..
        } => {
            assert_eq!(request_id, req_id.into());
            assert_eq!(
                expires_at_ms,
                created_at_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS
            );
        }
        other => panic!("expected SignatureRequired, got {other:?}"),
    }

    // Sibling channel ch2 has no pending request -> Backoff with effective per-node retry_after_ms.
    let status_ch2 = server
        .get_watchtower_signing_status(
            ctx.clone(),
            GetWatchtowerSigningStatusParams {
                channel_id: ch2.into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        status_ch2.status,
        WatchtowerSigningStatus::Backoff {
            retry_after_ms,
            consecutive_expired_requests: 0,
        }
    );

    // Once ch1's pending request is cleared while backoff is active -> Backoff with consecutive count.
    store
        .update_watchtower_signer(&tenant, &ch1, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests.clear();
                ext.pending_request_created_at_ms.clear();
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    let status_ch1_backoff = server
        .get_watchtower_signing_status(
            ctx,
            GetWatchtowerSigningStatusParams {
                channel_id: ch1.into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        status_ch1_backoff.status,
        WatchtowerSigningStatus::Backoff {
            retry_after_ms,
            consecutive_expired_requests: 2,
        }
    );
}

#[tokio::test]
async fn watchtower_rejects_tenant_calls_when_lsp_disabled() {
    let (store, _dir) = generate_store();
    let server = WatchtowerRpcServerImpl::new(store.clone()).with_lsp_enabled(false);
    let tenant = NodeId::from_bytes(Privkey::from([51; 32]).pubkey().serialize().to_vec());
    let tenant_ctx = RpcContext {
        node_id: tenant.to_string(),
        tenant_scoped: true,
    };
    let host_ctx = RpcContext {
        node_id: NodeId::local().to_string(),
        tenant_scoped: false,
    };
    let ch = Hash256::from([61; 32]);

    let err = server
        .create_watch_channel(
            tenant_ctx,
            CreateWatchChannelParams {
                channel_id: ch.into(),
                funding_udt_type_script: None,
                local_settlement_key: None,
                local_settlement_key_pubkey: Some(Privkey::from([1; 32]).pubkey().into()),
                remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
                local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
                remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
                settlement_data: snapshot(1).into(),
                channel_features: Default::default(),
                payout_lock_script: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        err.message().contains("hosted LSP service is not enabled"),
        "{err}"
    );
    assert!(store.get_watch_channel(&tenant, &ch).is_none());

    // Host (non-tenant) watchtower calls remain allowed when LSP is disabled.
    server
        .create_watch_channel(
            host_ctx,
            CreateWatchChannelParams {
                channel_id: ch.into(),
                funding_udt_type_script: None,
                local_settlement_key: Some(Privkey::from([1; 32]).into()),
                local_settlement_key_pubkey: None,
                remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
                local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
                remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
                settlement_data: snapshot(1).into(),
                channel_features: Default::default(),
                payout_lock_script: None,
            },
        )
        .await
        .unwrap();
    assert!(store.get_watch_channel(&NodeId::local(), &ch).is_some());
}
