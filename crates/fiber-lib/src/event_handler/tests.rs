use super::forward_event_to_watchtower_store;
use crate::fiber_types::{Hash256, NodeId, Privkey, SettlementData};
use crate::watchtower::WatchtowerStore;
use crate::NetworkServiceEvent;

fn tenant_node_id() -> NodeId {
    NodeId::from_bytes(Privkey::from(&[7; 32]).pubkey().serialize().to_vec())
}

#[test]
fn remote_tx_complete_without_a_secret_is_an_external_watch_channel() {
    let (store, _dir) = crate::generate_store();
    let node_id = tenant_node_id();
    let channel_id = Hash256::from([0x11; 32]);
    let local_settlement_pubkey = Privkey::from(&[1; 32]).pubkey();
    forward_event_to_watchtower_store(
        &Default::default(),
        NetworkServiceEvent::RemoteTxComplete(
            Privkey::from(&[9; 32]).pubkey(),
            channel_id,
            None,
            fiber_types::WatchChannelKeys {
                local_settlement: fiber_types::SettlementKey::external(local_settlement_pubkey),
                remote_settlement: Privkey::from(&[2; 32]).pubkey(),
                local_funding: Privkey::from(&[3; 32]).pubkey(),
                remote_funding: Privkey::from(&[4; 32]).pubkey(),
            },
            SettlementData {
                local_amount: 1,
                remote_amount: 1,
                tlcs: Vec::new(),
            },
            fiber_types::ChannelFeatures::V2,
        ),
        &store,
        node_id.clone(),
        &Default::default(),
    );

    let channel = store
        .get_watch_channel(&node_id, &channel_id)
        .expect("watched channel");
    assert!(channel.local_settlement_key.is_none());
    assert_eq!(
        channel.local_settlement_key_pubkey,
        Some(local_settlement_pubkey)
    );
    assert_eq!(
        store.get_watchtower_signer(&node_id, &channel_id),
        fiber_types::WatchtowerSignerState::External(
            fiber_types::WatchtowerExternalSignerState::default()
        )
    );
    assert!(store
        .get_watch_channel(&NodeId::local(), &channel_id)
        .is_none());
}

#[test]
fn hosted_preimage_created_enforces_quotas_while_host_is_exempt() {
    use crate::fiber_types::HashAlgorithm;
    use crate::lsp_config::WatchtowerQuotas;

    let (store, _dir) = crate::generate_store();
    let tenant_a = tenant_node_id();
    let tenant_b = NodeId::from_bytes(Privkey::from(&[8; 32]).pubkey().serialize().to_vec());
    let quotas = WatchtowerQuotas {
        max_channels_per_node: 2,
        max_channels_total: 3,
        max_preimages_per_node: 2,
        max_preimages_total: 3,
    };
    let events = Default::default();

    let make_pair = |seed: u8| -> (Hash256, Hash256) {
        let preimage = Hash256::from([seed; 32]);
        let hash: Hash256 = HashAlgorithm::Sha256.hash(preimage.as_ref()).into();
        (hash, preimage)
    };

    let (h1, p1) = make_pair(1);
    let (h2, p2) = make_pair(2);
    let (h3, p3) = make_pair(3);
    let (h4, p4) = make_pair(4);
    let (h5, p5) = make_pair(5);
    let (h6, p6) = make_pair(6);

    // Tenant A registers 2 preimages (fills per-node cap = 2).
    for (h, p) in [(h1, p1), (h2, p2), (h3, p3)] {
        forward_event_to_watchtower_store(
            &events,
            NetworkServiceEvent::PreimageCreated(h, p),
            &store,
            tenant_a.clone(),
            &quotas,
        );
    }
    assert_eq!(store.get_watch_preimage(&tenant_a, &h1), Some(p1));
    assert_eq!(store.get_watch_preimage(&tenant_a, &h2), Some(p2));
    assert_eq!(
        store.get_watch_preimage(&tenant_a, &h3),
        None,
        "3rd preimage for tenant A must be skipped when per-node quota is exceeded"
    );

    // Tenant B registers 1 preimage (fills global cap = 3); 2nd is skipped.
    for (h, p) in [(h4, p4), (h5, p5)] {
        forward_event_to_watchtower_store(
            &events,
            NetworkServiceEvent::PreimageCreated(h, p),
            &store,
            tenant_b.clone(),
            &quotas,
        );
    }
    assert_eq!(store.get_watch_preimage(&tenant_b, &h4), Some(p4));
    assert_eq!(
        store.get_watch_preimage(&tenant_b, &h5),
        None,
        "2nd preimage for tenant B must be skipped when global quota is exceeded"
    );

    // Removing a preimage from Tenant A frees global capacity for Tenant B.
    forward_event_to_watchtower_store(
        &events,
        NetworkServiceEvent::PreimageRemoved(h2),
        &store,
        tenant_a.clone(),
        &quotas,
    );
    forward_event_to_watchtower_store(
        &events,
        NetworkServiceEvent::PreimageCreated(h5, p5),
        &store,
        tenant_b.clone(),
        &quotas,
    );
    assert_eq!(store.get_watch_preimage(&tenant_b, &h5), Some(p5));

    // Host (NodeId::local()) bypasses tenant preimage quotas.
    forward_event_to_watchtower_store(
        &events,
        NetworkServiceEvent::PreimageCreated(h6, p6),
        &store,
        NodeId::local(),
        &quotas,
    );
    assert_eq!(store.get_watch_preimage(&NodeId::local(), &h6), Some(p6));
}
