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
