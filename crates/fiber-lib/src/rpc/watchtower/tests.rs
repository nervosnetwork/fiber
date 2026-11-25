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
                    commitment_number: 2,
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
    assert!(store
        .update_pending_remote_settlement(node.clone(), channel, 2, snapshot(255))
        .is_err());
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

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn create_watch_channel_retry_does_not_publish_no_signature_required_while_pending() {
    use ckb_types::core::TransactionBuilder;
    use ckb_types::packed::OutPoint;
    use fiber_types::{OnchainKeyPurpose, OnchainSigningContent, WatchtowerSignerState};
    use futures::FutureExt;

    use crate::signing::{
        notifications::{SigningChange, SigningScope},
        SigningEvents,
    };

    let (store, _dir) = generate_store();
    let signing_events = SigningEvents::default();
    let server =
        WatchtowerRpcServerImpl::new(store.clone()).with_signing_events(signing_events.clone());

    let tenant = NodeId::from_bytes(Privkey::from([71; 32]).pubkey().serialize().to_vec());
    for (node_id, tenant_scoped, ch_byte, req_byte) in [
        (tenant, true, 72u8, 73u8),
        (NodeId::local(), false, 74u8, 75u8),
    ] {
        let ctx = RpcContext {
            node_id: node_id.to_string(),
            tenant_scoped,
        };
        let channel_id = Hash256::from([ch_byte; 32]);
        let make_params = || CreateWatchChannelParams {
            channel_id: channel_id.into(),
            funding_udt_type_script: None,
            local_settlement_key: None,
            local_settlement_key_pubkey: Some(Privkey::from([1; 32]).pubkey().into()),
            remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
            local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
            remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
            settlement_data: snapshot(1).into(),
            channel_features: Default::default(),
            payout_lock_script: None,
        };

        let mut changes = signing_events
            .hub
            .subscribe(SigningScope::Watchtower(node_id.clone()));

        // First registration inserts a new row and emits WatchtowerCreated(NoSignatureRequired).
        server
            .create_watch_channel(ctx.clone(), make_params())
            .await
            .unwrap();
        let created_event = changes
            .recv()
            .now_or_never()
            .expect("initial create_watch_channel must publish WatchtowerCreated")
            .unwrap();
        assert!(
            matches!(
                created_event.as_ref(),
                SigningChange::WatchtowerCreated {
                    channel_id: id,
                    status: WatchtowerSigningStatus::NoSignatureRequired,
                } if *id == channel_id
            ),
            "unexpected created event: {created_event:?}"
        );
        assert!(changes.recv().now_or_never().is_none());

        // Transition signer state to a pending external signature request (SignatureRequired).
        let req_id = Hash256::from([req_byte; 32]);
        let now_ms = crate::now_timestamp_as_millis_u64();
        let signing_scope = signing_events.watchtower(&node_id);
        let mut signing_update = signing_scope.prepare_update();
        store
            .update_watchtower_signer(&node_id, &channel_id, |state| {
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
                    ext.pending_request_created_at_ms.insert(req_id, now_ms);
                }
                signing_update.watchtower_state(channel_id, state);
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        signing_update.publish();

        let pending_event = changes
            .recv()
            .now_or_never()
            .expect("pending external request must publish WatchtowerUpdated")
            .unwrap();
        match pending_event.as_ref() {
            SigningChange::WatchtowerUpdated {
                channel_id: id,
                status: WatchtowerSigningStatus::SignatureRequired { request_id, .. },
            } => {
                assert_eq!(*id, channel_id);
                assert_eq!(*request_id, req_id.into());
            }
            other => panic!("expected WatchtowerUpdated(SignatureRequired), got {other:?}"),
        }

        // Retrying create_watch_channel on the already-monitored channel must succeed,
        // emit NO spurious NoSignatureRequired event, and keep SignatureRequired intact.
        server
            .create_watch_channel(ctx.clone(), make_params())
            .await
            .unwrap();
        assert!(
            changes.recv().now_or_never().is_none(),
            "re-registering an existing watch channel must not publish a signing event"
        );

        let status = server
            .get_watchtower_signing_status(
                ctx,
                GetWatchtowerSigningStatusParams {
                    channel_id: channel_id.into(),
                },
            )
            .await
            .unwrap();
        match status.status {
            WatchtowerSigningStatus::SignatureRequired { request_id, .. } => {
                assert_eq!(request_id, req_id.into());
            }
            other => panic!("expected SignatureRequired to survive retry, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn submit_watchtower_signature_rejects_expired_and_refreshes_hold_on_accept() {
    let _serial = crate::ckb::held_cells::TEST_SERIAL.lock().await;
    use ckb_sdk::traits::{CellCollector, CellCollectorError, CellQueryOptions, LiveCell};
    use ckb_types::core::TransactionBuilder;
    use ckb_types::packed::{Byte32, CellInput, OutPoint, Transaction};
    use fiber_types::{OnchainKeyPurpose, OnchainSigningContent, WatchtowerSignerState};
    use std::collections::HashSet;

    #[derive(Clone, Default)]
    struct LockTracker {
        locked: HashSet<OutPoint>,
    }

    #[async_trait::async_trait]
    impl CellCollector for LockTracker {
        async fn collect_live_cells_async(
            &mut self,
            _query: &CellQueryOptions,
            _apply_changes: bool,
        ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
            Ok((vec![], 0))
        }
        fn lock_cell(&mut self, out_point: OutPoint, _: u64) -> Result<(), CellCollectorError> {
            self.locked.insert(out_point);
            Ok(())
        }
        fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }

    let (store, _dir) = generate_store();
    let server = WatchtowerRpcServerImpl::new(store.clone());
    let settlement_key = Privkey::from([81; 32]);
    let tenant = NodeId::from_bytes(Privkey::from([82; 32]).pubkey().serialize().to_vec());
    let channel_id = Hash256::from([83; 32]);
    let ctx = RpcContext {
        node_id: tenant.to_string(),
        tenant_scoped: true,
    };

    server
        .create_watch_channel(
            ctx.clone(),
            CreateWatchChannelParams {
                channel_id: channel_id.into(),
                funding_udt_type_script: None,
                local_settlement_key: None,
                local_settlement_key_pubkey: Some(settlement_key.pubkey().into()),
                remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
                local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
                remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
                settlement_data: snapshot(0).into(),
                channel_features: Default::default(),
                payout_lock_script: None,
            },
        )
        .await
        .unwrap();

    let now_ms = crate::now_timestamp_as_millis_u64();

    // (a) Backdated pending request older than WATCHTOWER_HOLD_MS must be rejected
    // without moving into signed_signatures or holding its fee input.
    let expired_req_id = Hash256::from([0xe1; 32]);
    let expired_fee_outpoint = OutPoint::new(Byte32::from([0xe1; 32]), 1);
    let expired_content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([0xd1; 32]), 0),
                0,
            ))
            .input(CellInput::new(expired_fee_outpoint.clone(), 0))
            .build()
            .data(),
    };
    let expired_sig =
        crate::watchtower::sign_onchain_request(&settlement_key, &expired_content).unwrap();
    let expired_created_at = now_ms.saturating_sub(crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 1);

    crate::ckb::held_cells::release_for_watchtower([expired_fee_outpoint.clone()]);
    store
        .update_watchtower_signer(&tenant, &channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests
                    .insert(expired_req_id, expired_content.clone());
                ext.pending_request_created_at_ms
                    .insert(expired_req_id, expired_created_at);
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    let err = server
        .submit_watchtower_signature(
            ctx.clone(),
            SubmitWatchtowerSignatureParams {
                channel_id: channel_id.into(),
                request_id: expired_req_id.into(),
                signature: expired_sig.to_vec(),
            },
        )
        .await
        .expect_err("expired pending request must be rejected");
    assert!(err.message().contains("expired"), "{err}");

    match store.get_watchtower_signer(&tenant, &channel_id) {
        WatchtowerSignerState::External(ext) => {
            assert!(
                ext.signed_signatures.is_empty(),
                "expired request must not move into signed_signatures"
            );
            assert!(ext.signed_signature_created_at_ms.is_empty());
            assert!(ext.last_applied.is_none());
            assert!(ext.pending_requests.contains_key(&expired_req_id));
        }
        other => panic!("expected external signer state, got {other:?}"),
    }
    let mut expired_tracker = LockTracker::default();
    crate::ckb::held_cells::lock_held(&mut expired_tracker, now_ms).unwrap();
    assert!(
        !expired_tracker.locked.contains(&expired_fee_outpoint),
        "rejected expired request must not hold fee cell"
    );

    // (b) Fresh non-expired pending request (close to original expiry) is accepted and its
    // fee input hold is refreshed in held_cells to now_ms + WATCHTOWER_HOLD_MS.
    let fresh_req_id = Hash256::from([0xe2; 32]);
    let fresh_fee_outpoint = OutPoint::new(Byte32::from([0xe2; 32]), 1);
    let fresh_content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([0xd2; 32]), 0),
                0,
            ))
            .input(CellInput::new(fresh_fee_outpoint.clone(), 0))
            .build()
            .data(),
    };
    let fresh_sig =
        crate::watchtower::sign_onchain_request(&settlement_key, &fresh_content).unwrap();
    // Created almost WATCHTOWER_HOLD_MS ago (5s left): still valid, and checking just past
    // its old expiry verifies the hold was actually refreshed rather than left at old_created_at.
    let old_created_at = now_ms.saturating_sub(crate::ckb::held_cells::WATCHTOWER_HOLD_MS - 5_000);

    crate::ckb::held_cells::release_for_watchtower([fresh_fee_outpoint.clone()]);
    store
        .update_watchtower_signer(&tenant, &channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests
                    .insert(fresh_req_id, fresh_content.clone());
                ext.pending_request_created_at_ms
                    .insert(fresh_req_id, old_created_at);
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    let res = server
        .submit_watchtower_signature(
            ctx,
            SubmitWatchtowerSignatureParams {
                channel_id: channel_id.into(),
                request_id: fresh_req_id.into(),
                signature: fresh_sig.to_vec(),
            },
        )
        .await
        .expect("fresh pending request must be accepted");
    assert_eq!(res, SubmitWatchtowerSignatureResult::Applied);

    let stamped_ms = match store.get_watchtower_signer(&tenant, &channel_id) {
        WatchtowerSignerState::External(ext) => {
            assert!(!ext.pending_requests.contains_key(&fresh_req_id));
            assert!(!ext
                .pending_request_created_at_ms
                .contains_key(&fresh_req_id));
            assert!(ext.signed_signatures.contains_key(&fresh_req_id));
            *ext.signed_signature_created_at_ms
                .get(&fresh_req_id)
                .expect("signed_signature_created_at_ms must be recorded")
        }
        other => panic!("expected external signer state, got {other:?}"),
    };
    assert!(stamped_ms >= now_ms);

    let mut refreshed_tracker = LockTracker::default();
    crate::ckb::held_cells::lock_held(
        &mut refreshed_tracker,
        old_created_at + crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 1,
    )
    .unwrap();
    assert!(
        refreshed_tracker.locked.contains(&fresh_fee_outpoint),
        "accepted submission must refresh fee input hold past old pending expiry"
    );

    crate::ckb::held_cells::release_for_watchtower([expired_fee_outpoint, fresh_fee_outpoint]);
}

#[tokio::test]
async fn standalone_watchtower_client_bypasses_lsp_tenant_quotas() {
    let (store, _dir) = generate_store();
    // Default WatchtowerQuotas cap hosted tenants at 64 channels and 256 preimages.
    let server = WatchtowerRpcServerImpl::new(store.clone()).with_lsp_enabled(false);
    let standalone_node = NodeId::from_bytes(Privkey::from([91; 32]).pubkey().serialize().to_vec());
    assert_ne!(standalone_node, NodeId::local());
    let ctx = RpcContext {
        node_id: standalone_node.to_string(),
        tenant_scoped: false,
    };

    // Standalone (non-tenant-scoped) client must be able to register > 64 watch channels.
    for i in 0u16..65 {
        let mut ch_bytes = [0u8; 32];
        ch_bytes[..2].copy_from_slice(&i.to_le_bytes());
        let channel_id = Hash256::from(ch_bytes);
        server
            .create_watch_channel(
                ctx.clone(),
                CreateWatchChannelParams {
                    channel_id: channel_id.into(),
                    funding_udt_type_script: None,
                    local_settlement_key: Some(Privkey::from([1; 32]).into()),
                    local_settlement_key_pubkey: None,
                    remote_settlement_key: Privkey::from([2; 32]).pubkey().into(),
                    local_funding_pubkey: Privkey::from([5; 32]).pubkey().into(),
                    remote_funding_pubkey: Privkey::from([6; 32]).pubkey().into(),
                    settlement_data: snapshot(0).into(),
                    channel_features: Default::default(),
                    payout_lock_script: None,
                },
            )
            .await
            .unwrap_or_else(|err| panic!("standalone channel #{i} must succeed: {err}"));
    }
    let mut last_ch_bytes = [0u8; 32];
    last_ch_bytes[..2].copy_from_slice(&64u16.to_le_bytes());
    assert!(store
        .get_watch_channel(&standalone_node, &Hash256::from(last_ch_bytes))
        .is_some());

    // Standalone (non-tenant-scoped) client must also be able to register > 256 preimages.
    let mut last_hash = Hash256::default();
    let mut last_preimage = Hash256::default();
    for i in 0u16..257 {
        let mut preimage_bytes = [0u8; 32];
        preimage_bytes[..2].copy_from_slice(&i.to_le_bytes());
        let preimage = Hash256::from(preimage_bytes);
        let payment_hash: Hash256 = HashAlgorithm::Sha256.hash(preimage.as_ref()).into();
        server
            .create_preimage(
                ctx.clone(),
                CreatePreimageParams {
                    payment_hash: payment_hash.into(),
                    preimage: preimage.into(),
                },
            )
            .await
            .unwrap_or_else(|err| panic!("standalone preimage #{i} must succeed: {err}"));
        last_hash = payment_hash;
        last_preimage = preimage;
    }
    assert_eq!(
        store.get_watch_preimage(&standalone_node, &last_hash),
        Some(last_preimage)
    );
}
