use crate::ckb::signer::LocalSigner;
use crate::fiber::channel::*;
use crate::fiber::gossip::{get_latest_startup_broadcast_message_cursor, GossipMessageStore};
use crate::fiber::network::get_chain_hash;
use crate::fiber::onchain_tlc_reconcile::OnChainTlcSettlement;
use crate::fiber::types::new_channel_update_unsigned;
use crate::fiber::types::*;
#[allow(unused)]
use crate::fiber::{
    blake2b_hash_with_salt,
    config::{DEFAULT_TLC_EXPIRY_DELTA, MAX_PAYMENT_TLC_EXPIRY_LIMIT},
    graph::*,
    payment::{PaymentSessionExt, SendPaymentDataBuilder},
    AwaitingChannelReadyFlags, ChannelActorData, ChannelBasePublicKeys, ChannelConstraints,
    ChannelState, Direction, FeatureVector, InMemorySigner, NegotiatingFundingFlags, NodeId,
    PaymentCustomRecords, PaymentSession, PaymentStatus, Privkey, Pubkey, PublicChannelInfo,
    RevocationData, SendPaymentData, SettlementData, SigningCommitmentFlags, TimedResult,
};
use crate::gen_rand_channel_outpoint;
use crate::gen_rand_fiber_private_key;
use crate::gen_rand_fiber_public_key;
use crate::gen_rand_secp256k1_keypair_tuple;
use crate::gen_rand_sha256_hash;
use crate::invoice::*;
use crate::now_timestamp_as_millis_u64;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::payment::PaymentRpcServer;
#[cfg(not(target_arch = "wasm32"))]
use crate::store::sample::StoreSample;
use crate::store::store_impl::deserialize_from;
use crate::store::store_impl::serialize_to_vec;
#[cfg(not(target_arch = "wasm32"))]
use crate::store::store_trait::PrefixIterOptions;
#[cfg(not(target_arch = "wasm32"))]
use crate::store::{open_store, FiberStore, NodeNamespace};
use crate::tests::test_utils::*;
use crate::time::SystemTime;
#[cfg(not(target_arch = "wasm32"))]
use crate::watchtower::*;
#[cfg(not(target_arch = "wasm32"))]
use ckb_hash::blake2b_256;
use ckb_types::packed::*;
use ckb_types::prelude::*;
use ckb_types::H256;
#[cfg(not(target_arch = "wasm32"))]
use core::cmp::Ordering;
#[cfg(not(target_arch = "wasm32"))]
use fiber_store::backend::BatchWriter;
#[cfg(not(target_arch = "wasm32"))]
use fiber_store::backend::StorageBackend;
#[cfg(not(target_arch = "wasm32"))]
use fiber_store::IteratorDirection;
use fiber_types::protocol::AnnouncedNodeName;
#[cfg(not(target_arch = "wasm32"))]
use fiber_types::schema::WATCHTOWER_TLC_SETTLED_PREFIX;
#[cfg(not(target_arch = "wasm32"))]
use fiber_types::{
    AddTlcCommand, AppliedFlags, ChannelFeatures, CloseFlags, CommitmentNumbers, OutboundTlcStatus,
    RemoveTlcFulfill, RetryableTlcOperation, SettlementKey, SettlementTlc, TLCId, TlcErrPacket,
    TlcErrorCode, TlcInfo, TlcStatus, WatchChannelKeys,
};
#[cfg(not(target_arch = "wasm32"))]
use fiber_types::{Attempt, AttemptStatus, HashAlgorithm, PaymentHopData, RouterHop, SessionRoute};
use musig2::secp::MaybeScalar;
#[cfg(not(target_arch = "wasm32"))]
use musig2::CompactSignature;
use musig2::SecNonce;
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use tentacle::secio::PeerId;

fn gen_rand_local_signer() -> LocalSigner {
    let (secret_key, _) = gen_rand_secp256k1_keypair_tuple();
    LocalSigner::new(secret_key)
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_node_namespace_isolates_shared_physical_store() {
    let path = TempDir::new("node_namespace_store");
    let store = open_store(path).expect("create shared store");
    let lsp_metadata = store.namespaced(NodeNamespace::lsp_metadata());
    let tenant_u1 = store.namespaced(NodeNamespace::hosted_tenant("u1"));
    let tenant_u2 = store.namespaced(NodeNamespace::hosted_tenant("u2"));
    tenant_u1
        .ensure_current_schema()
        .expect("initialize u1 schema");
    tenant_u2
        .ensure_current_schema()
        .expect("initialize u2 schema");

    lsp_metadata.put(b"same-key", b"lsp-value");
    tenant_u1.put(b"same-key", b"u1-value");
    tenant_u1.put(b"scan/1", b"u1-first");
    tenant_u1.put(b"scan/2", b"u1-second");
    let mut tenant_u2_batch = tenant_u2.batch();
    tenant_u2_batch.put(b"same-key", b"u2-value");
    tenant_u2_batch.put(b"scan/1", b"u2-first");
    tenant_u2_batch.commit();

    assert_eq!(tenant_u1.get(b"same-key"), Some(b"u1-value".to_vec()));
    assert_eq!(tenant_u2.get(b"same-key"), Some(b"u2-value".to_vec()));
    assert_eq!(lsp_metadata.get(b"same-key"), Some(b"lsp-value".to_vec()));
    assert_eq!(store.get(b"same-key"), None);
    assert_eq!(
        tenant_u1
            .collect_iterator(
                b"scan/".to_vec(),
                IteratorDirection::Forward,
                Box::new(|key| key.starts_with(b"scan/")),
                0,
            )
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect::<Vec<_>>(),
        vec![
            (b"scan/1".to_vec(), b"u1-first".to_vec()),
            (b"scan/2".to_vec(), b"u1-second".to_vec()),
        ]
    );
    assert_eq!(
        tenant_u1
            .collect_by_prefix_with(b"scan/", PrefixIterOptions::new().reverse().limit(1))
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect::<Vec<_>>(),
        vec![(b"scan/2".to_vec(), b"u1-second".to_vec())]
    );

    let payment_hash = Hash256::from([3; 32]);
    let u1_preimage = Hash256::from([4; 32]);
    let u2_preimage = Hash256::from([5; 32]);
    tenant_u1.insert_preimage(payment_hash, u1_preimage);
    tenant_u2.insert_preimage(payment_hash, u2_preimage);

    assert_eq!(tenant_u1.get_preimage(&payment_hash), Some(u1_preimage));
    assert_eq!(tenant_u2.get_preimage(&payment_hash), Some(u2_preimage));
    assert_eq!(store.get_preimage(&payment_hash), None);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_schema_preflight_scans_only_its_logical_namespace() {
    let path = TempDir::new("namespace_migration_scope");
    let store = open_store(path).expect("shared store");
    let tenant = store.namespaced(NodeNamespace::hosted_tenant("one"));
    tenant.ensure_current_schema().expect("initialize tenant");
    let corrupt_key = [
        &[fiber_types::schema::CHANNEL_ACTOR_STATE_PREFIX],
        &[19; 32][..],
    ]
    .concat();
    tenant.put(&corrupt_key, b"corrupt tenant state");
    store
        .ensure_current_schema()
        .expect("root must not scan tenant rows");
    assert!(
        tenant.ensure_current_schema().is_err(),
        "tenant validates its own rows"
    );
    tenant.delete(&corrupt_key);
    store.put(&corrupt_key, b"corrupt root state");
    tenant
        .ensure_current_schema()
        .expect("tenant must not scan root rows");
    assert!(
        store.ensure_current_schema().is_err(),
        "root validates its own rows"
    );
}

#[cfg(all(not(target_arch = "wasm32"), feature = "watchtower"))]
#[test]
fn test_local_watch_channel_resolves_hosted_tenant_namespace() {
    let path = TempDir::new("local_watch_channel_namespace");
    let store = open_store(path).expect("create shared store");
    let tenant_u1 = store.namespaced(NodeNamespace::hosted_tenant("u1"));
    let tenant_u2 = store.namespaced(NodeNamespace::hosted_tenant("u2"));
    let unowned_tenant = store.namespaced(NodeNamespace::hosted_tenant("u3"));
    let u1_state = ChannelActorState::samples(42)
        .into_iter()
        .next()
        .expect("sample channel state");
    let mut u2_state = ChannelActorState::samples(43)
        .into_iter()
        .next()
        .expect("sample channel state");
    let channel_id = u1_state.get_id();
    u2_state.id = channel_id;
    let u1_node_id = NodeId::from_bytes(u1_state.get_local_pubkey().serialize().to_vec());
    let u2_node_id = NodeId::from_bytes(u2_state.get_local_pubkey().serialize().to_vec());
    assert_ne!(u1_node_id, u2_node_id);
    tenant_u1.insert_channel_actor_state(u1_state.clone());
    tenant_u2.insert_channel_actor_state(u2_state);

    let insert_watch = |node_id, channel_id, local_amount| {
        let local_key = Privkey::from(&[1; 32]);
        store.insert_watch_channel(
            node_id,
            channel_id,
            None,
            WatchChannelKeys {
                local_settlement: SettlementKey::local(local_key.clone()),
                remote_settlement: Privkey::from(&[2; 32]).pubkey(),
                local_funding: Privkey::from(&[3; 32]).pubkey(),
                remote_funding: Privkey::from(&[4; 32]).pubkey(),
            },
            SettlementData {
                local_amount,
                remote_amount: 100,
                tlcs: vec![],
            },
            fiber_types::ChannelFeatures::V2,
            None,
        );
    };
    // Watchtower rows live in the host's physical keyspace. The channel id alone
    // must not select the public node's or another tenant's settlement snapshot.
    insert_watch(u1_node_id.clone(), channel_id, 10);
    insert_watch(u2_node_id.clone(), channel_id, 20);
    insert_watch(NodeId::local(), channel_id, 30);
    for (reader, node_id) in [
        (&tenant_u1, u1_node_id.clone()),
        (&tenant_u2, u2_node_id),
        (&store, NodeId::local()),
    ] {
        let expected = store
            .get_watch_channel(&node_id, &channel_id)
            .expect("root watch row");
        assert_eq!(reader.get_local_watch_channel(&channel_id), Some(expected));
    }
    assert!(unowned_tenant
        .get_local_watch_channel(&channel_id)
        .is_none());

    // Even a row under U1's watchtower identity does not establish ownership of
    // a channel missing from U1's persisted channel state.
    let unowned_channel_id = Hash256::from([91; 32]);
    insert_watch(u1_node_id, unowned_channel_id, 40);
    insert_watch(NodeId::local(), unowned_channel_id, 50);
    assert!(tenant_u1
        .get_local_watch_channel(&unowned_channel_id)
        .is_none());

    let mut missing_watch_state = u1_state;
    missing_watch_state.id = Hash256::from([92; 32]);
    let missing_watch_id = missing_watch_state.get_id();
    tenant_u1.insert_channel_actor_state(missing_watch_state);
    assert!(tenant_u1
        .get_local_watch_channel(&missing_watch_id)
        .is_none());
    assert!(store.get_local_watch_channel(&missing_watch_id).is_none());
}

fn mock_node() -> (Privkey, NodeAnnouncement) {
    let signer = gen_rand_local_signer();
    let sk: Privkey = (*signer.secret_key()).into();
    (
        sk.clone(),
        NodeAnnouncement::new_signed(
            AnnouncedNodeName::from_string("node1").expect("invalid name"),
            FeatureVector::default(),
            vec![],
            &sk,
            get_chain_hash(),
            now_timestamp_as_millis_u64(),
            0,
            Default::default(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
    )
}

fn mock_channel() -> ChannelAnnouncement {
    let signer1 = gen_rand_local_signer();
    let signer2 = gen_rand_local_signer();
    let signer3 = gen_rand_local_signer();
    let xonly = signer3.x_only_pub_key();
    let rand_hash256 = gen_rand_sha256_hash();
    let pubkey1: Pubkey = (*signer1.pubkey()).into();
    let pubkey2: Pubkey = (*signer2.pubkey()).into();
    ChannelAnnouncement::new_unsigned(
        &pubkey1,
        &pubkey2,
        OutPoint::new_builder()
            .tx_hash(rand_hash256)
            .index(0u32)
            .build(),
        get_chain_hash(),
        &xonly,
        0,
        None,
    )
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_invoice() {
    let (store, _dir) = generate_store();

    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_preimage(preimage)
        .fallback_address("address".to_string())
        .add_attr(Attribute::FinalHtlcMinimumExpiryDelta(5))
        .build()
        .unwrap();

    let hash = invoice.payment_hash();
    store
        .insert_invoice(invoice.clone(), Some(preimage))
        .unwrap();
    assert_eq!(store.get_invoice(hash), Some(invoice.clone()));
    assert_eq!(store.get_preimage(hash), Some(preimage));

    let invalid_hash = gen_rand_sha256_hash();
    assert_eq!(store.get_preimage(&invalid_hash), None);

    assert_eq!(store.get_invoice_status(hash), Some(CkbInvoiceStatus::Open));
    assert_eq!(store.get_invoice_status(&gen_rand_sha256_hash()), None);

    let status = CkbInvoiceStatus::Paid;
    store.update_invoice_status(hash, status).unwrap();
    assert_eq!(store.get_invoice_status(hash), Some(status));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_invoice_preimage_origin_is_persistent() {
    let (store, dir) = generate_store();
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1000))
        .payment_preimage(preimage)
        .build()
        .expect("build regular invoice");
    let payment_hash = *invoice.payment_hash();
    store
        .insert_invoice(invoice, Some(preimage))
        .expect("insert regular invoice");
    assert!(store.has_invoice_preimage(&payment_hash));

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1000))
        .payment_hash(ckb_hash::blake2b_256(hold_preimage).into())
        .build()
        .expect("build hold invoice");
    let hold_hash = *hold_invoice.payment_hash();
    store
        .insert_invoice(hold_invoice, None)
        .expect("insert hold invoice");
    store.insert_preimage(hold_hash, hold_preimage);
    assert!(!store.has_invoice_preimage(&hold_hash));

    #[cfg(not(target_arch = "wasm32"))]
    {
        drop(store);
        let reopened = open_store(dir.as_ref()).expect("reopen store");
        assert!(reopened.has_invoice_preimage(&payment_hash));
        assert!(!reopened.has_invoice_preimage(&hold_hash));
    }
    #[cfg(target_arch = "wasm32")]
    let _ = dir;
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_get_broadcast_messages_iter() {
    let (store, _dir) = generate_store();
    let timestamp = now_timestamp_as_millis_u64();
    let channel_announcement = mock_channel();
    let outpoint = channel_announcement.out_point().clone();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let default_cursor = Cursor::default();
    let mut iter = store.get_broadcast_messages(&default_cursor, 0).into_iter();
    assert_eq!(
        iter.next(),
        Some(BroadcastMessageWithTimestamp::ChannelAnnouncement(
            timestamp,
            channel_announcement
        )),
    );
    assert_eq!(iter.next(), None);
    let cursor = Cursor::new(timestamp, BroadcastMessageID::ChannelAnnouncement(outpoint));
    let mut iter = store.get_broadcast_messages(&cursor, 0).into_iter();
    assert_eq!(iter.next(), None);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_get_broadcast_messages() {
    let (store, _dir) = generate_store();
    let timestamp = now_timestamp_as_millis_u64();
    let channel_announcement = mock_channel();
    let outpoint = channel_announcement.out_point().clone();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let default_cursor = Cursor::default();
    let result = store.get_broadcast_messages(&default_cursor, 0);
    assert_eq!(
        result,
        vec![BroadcastMessageWithTimestamp::ChannelAnnouncement(
            timestamp,
            channel_announcement
        )],
    );
    let cursor = Cursor::new(timestamp, BroadcastMessageID::ChannelAnnouncement(outpoint));
    let result = store.get_broadcast_messages(&cursor, 0);
    assert_eq!(result, vec![]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_save_channel_announcement() {
    let (store, _dir) = generate_store();
    let timestamp = now_timestamp_as_millis_u64();
    let channel_announcement = mock_channel();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let new_channel_announcement =
        store.get_latest_channel_announcement(channel_announcement.out_point());
    assert_eq!(
        new_channel_announcement,
        Some((timestamp, channel_announcement))
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_save_channel_update() {
    let (store, _dir) = generate_store();
    let flags_for_update_of_node1 = ChannelUpdateMessageFlags::UPDATE_OF_NODE1;
    let channel_update_of_node1 = new_channel_update_unsigned(
        OutPoint::new_builder()
            .tx_hash(gen_rand_sha256_hash())
            .index(0u32)
            .build(),
        now_timestamp_as_millis_u64(),
        flags_for_update_of_node1,
        ChannelUpdateChannelFlags::empty(),
        0,
        0,
        0,
    );
    let out_point = channel_update_of_node1.channel_outpoint.clone();
    store.save_channel_update(channel_update_of_node1.clone());
    assert_eq!(
        store.get_latest_channel_update(&out_point, true).as_ref(),
        Some(&channel_update_of_node1)
    );
    assert_eq!(store.get_latest_channel_update(&out_point, false), None);

    let mut channel_update_of_node2 = channel_update_of_node1.clone();
    let flags_for_update_of_node2 = ChannelUpdateMessageFlags::UPDATE_OF_NODE2;
    channel_update_of_node2.message_flags = flags_for_update_of_node2;
    // Note that per discussion in Notion, we don't handle the rare case of two channel updates having the same timestamp.
    // In the current implementation, channel update from one side with the same timestamp will not overwrite the existing one
    // from the other side. So we have to set the timestamp to be different.
    channel_update_of_node2.timestamp = 2;
    store.save_channel_update(channel_update_of_node2.clone());
    assert_eq!(
        store.get_latest_channel_update(&out_point, false).as_ref(),
        Some(&channel_update_of_node2)
    );
    assert_eq!(
        store.get_latest_channel_update(&out_point, true).as_ref(),
        Some(&channel_update_of_node1)
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_save_node_announcement() {
    let (store, _dir) = generate_store();
    let (sk, node_announcement) = mock_node();
    let pk = sk.pubkey();
    store.save_node_announcement(node_announcement.clone());
    let new_node_announcement = store.get_latest_node_announcement(&pk);
    assert_eq!(new_node_announcement, Some(node_announcement));
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_get_latest_startup_broadcast_message_cursor_skips_local_messages_conservatively() {
    let (store, _dir) = generate_store();
    let local_signer = gen_rand_local_signer();
    let local_privkey: Privkey = (*local_signer.secret_key()).into();
    let local_pubkey = local_privkey.pubkey();
    let remote_signer = gen_rand_local_signer();
    let remote_privkey: Privkey = (*remote_signer.secret_key()).into();
    let remote_pubkey = remote_privkey.pubkey();
    let remote_peer_signer = gen_rand_local_signer();
    let remote_peer_pubkey: Pubkey = (*remote_peer_signer.pubkey()).into();
    let announcement_signer = gen_rand_local_signer();
    let x_only_pubkey = announcement_signer.x_only_pub_key();

    let local_node_announcement = NodeAnnouncement::new_signed(
        AnnouncedNodeName::from_string("local").expect("invalid name"),
        FeatureVector::default(),
        vec![],
        &local_privkey,
        get_chain_hash(),
        10,
        0,
        Default::default(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    store.save_node_announcement(local_node_announcement.clone());

    let local_channel_outpoint = gen_rand_channel_outpoint();
    let local_channel_announcement = ChannelAnnouncement::new_unsigned(
        &local_pubkey,
        &remote_pubkey,
        local_channel_outpoint.clone(),
        get_chain_hash(),
        &x_only_pubkey,
        0,
        None,
    );
    store.save_channel_announcement(20, local_channel_announcement);
    store.save_channel_update(new_channel_update_unsigned(
        local_channel_outpoint,
        30,
        ChannelUpdateMessageFlags::UPDATE_OF_NODE1,
        ChannelUpdateChannelFlags::empty(),
        1,
        1,
        1,
    ));

    let remote_node_announcement = NodeAnnouncement::new_signed(
        AnnouncedNodeName::from_string("remote").expect("invalid name"),
        FeatureVector::default(),
        vec![],
        &remote_privkey,
        get_chain_hash(),
        40,
        0,
        Default::default(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    store.save_node_announcement(remote_node_announcement);

    let remote_channel_outpoint = gen_rand_channel_outpoint();
    let remote_channel_announcement = ChannelAnnouncement::new_unsigned(
        &remote_pubkey,
        &remote_peer_pubkey,
        remote_channel_outpoint.clone(),
        get_chain_hash(),
        &x_only_pubkey,
        0,
        None,
    );
    store.save_channel_announcement(50, remote_channel_announcement);
    let remote_channel_update = new_channel_update_unsigned(
        remote_channel_outpoint,
        60,
        ChannelUpdateMessageFlags::UPDATE_OF_NODE1,
        ChannelUpdateChannelFlags::empty(),
        1,
        1,
        1,
    );
    store.save_channel_update(remote_channel_update.clone());

    store.save_channel_update(new_channel_update_unsigned(
        gen_rand_channel_outpoint(),
        70,
        ChannelUpdateMessageFlags::UPDATE_OF_NODE1,
        ChannelUpdateChannelFlags::empty(),
        1,
        1,
        1,
    ));

    let newer_local_node_announcement = NodeAnnouncement::new_signed(
        AnnouncedNodeName::from_string("local").expect("invalid name"),
        FeatureVector::default(),
        vec![],
        &local_privkey,
        get_chain_hash(),
        80,
        0,
        Default::default(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    store.save_node_announcement(newer_local_node_announcement);

    assert_eq!(
        get_latest_startup_broadcast_message_cursor(&store, Some(&local_pubkey)),
        remote_channel_update.cursor()
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower() {
    let path = TempDir::new("test-watchtower-store");
    let store = open_store(path).expect("created store failed");

    let node_id = NodeId::from_bytes(PeerId::random().into_bytes());
    let channel_id = gen_rand_sha256_hash();

    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![],
    };

    let local_settlement_key = Privkey::from(&[1; 32]);
    let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
    let local_funding_pubkey = Privkey::from(&[3; 32]).pubkey();
    let remote_funding_pubkey = Privkey::from(&[4; 32]).pubkey();

    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(local_settlement_key.clone()),
            remote_settlement: remote_settlement_key,
            local_funding: local_funding_pubkey,
            remote_funding: remote_funding_pubkey,
        },
        settlement_data.clone(),
        fiber_types::ChannelFeatures::V2,
        None,
    );
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_udt_type_script: None,
            local_settlement_key: Some(local_settlement_key.clone()),
            local_settlement_key_pubkey: Some(local_settlement_key.pubkey()),
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            revocation_data: None,
            local_settlement_data: settlement_data.clone(),
            pending_remote_settlement_data: settlement_data.clone(),
            remote_settlement_data: settlement_data.clone(),
            channel_features: Default::default(),
            payout_lock_script: None,
        }]
    );

    let revocation_data = RevocationData {
        commitment_number: 0,
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        output: CellOutput::default(),
        output_data: Bytes::default(),
    };

    store.update_revocation(
        node_id.clone(),
        channel_id,
        revocation_data.clone(),
        settlement_data.clone(),
    );
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_udt_type_script: None,
            local_settlement_key: Some(local_settlement_key.clone()),
            local_settlement_key_pubkey: Some(local_settlement_key.pubkey()),
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            local_settlement_data: settlement_data.clone(),
            revocation_data: Some(revocation_data),
            pending_remote_settlement_data: settlement_data.clone(),
            remote_settlement_data: settlement_data,
            channel_features: Default::default(),
            payout_lock_script: None,
        }]
    );

    let old = RevocationData {
        commitment_number: 0,
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        output: CellOutput::default(),
        output_data: Bytes::default(),
    };
    let mut newer = old.clone();
    newer.commitment_number = 1;
    let newer_settlement = SettlementData {
        local_amount: 300,
        remote_amount: 400,
        tlcs: vec![],
    };
    store.update_revocation(
        node_id.clone(),
        channel_id,
        newer.clone(),
        newer_settlement.clone(),
    );
    store.update_revocation(
        node_id.clone(),
        channel_id,
        old,
        SettlementData {
            local_amount: 1,
            remote_amount: 2,
            tlcs: vec![],
        },
    );
    let stored = store.get_watch_channels().pop().unwrap();
    assert_eq!(stored.revocation_data, Some(newer));
    assert_eq!(stored.remote_settlement_data, newer_settlement);
    // Replayed bootstrap registration cannot erase accepted revocation data.
    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(Privkey::from(&[1; 32])),
            remote_settlement: remote_settlement_key,
            local_funding: local_funding_pubkey,
            remote_funding: remote_funding_pubkey,
        },
        SettlementData {
            local_amount: 100,
            remote_amount: 200,
            tlcs: vec![],
        },
        fiber_types::ChannelFeatures::V2,
        None,
    );
    assert_eq!(store.get_watch_channels().pop().unwrap(), stored);

    store.remove_watch_channel(node_id, channel_id);
    assert_eq!(store.get_watch_channels(), vec![]);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_external_watch_channel_contains_no_private_keys() {
    let path = TempDir::new("test-external-watchtower-store");
    let store = open_store(path).expect("created store failed");
    let node_id = NodeId::local();
    let channel_id = gen_rand_sha256_hash();
    let local_settlement_pubkey = Privkey::from(&[1; 32]).pubkey();
    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 42,
            payment_hash: gen_rand_sha256_hash(),
            expiry: u64::MAX,
            local_key: None,
            local_key_pubkey: Some(Privkey::from(&[5; 32]).pubkey()),
            local_key_commitment_number: Some(42),
            remote_key: Privkey::from(&[6; 32]).pubkey(),
        }],
    };

    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::external(local_settlement_pubkey),
            remote_settlement: Privkey::from(&[2; 32]).pubkey(),
            local_funding: Privkey::from(&[3; 32]).pubkey(),
            remote_funding: Privkey::from(&[4; 32]).pubkey(),
        },
        settlement_data,
        fiber_types::ChannelFeatures::V2,
        None,
    );

    let channel = store
        .get_watch_channels()
        .into_iter()
        .next()
        .expect("stored watch channel");
    assert!(channel.local_settlement_key.is_none());
    assert!(channel
        .remote_settlement_data
        .tlcs
        .iter()
        .all(|tlc| tlc.local_key.is_none()));
    assert_eq!(
        store.get_watchtower_signer(&node_id, &channel_id),
        fiber_types::WatchtowerSignerState::External(
            fiber_types::WatchtowerExternalSignerState::default()
        )
    );
    let original = SettlementData {
        local_amount: 300,
        remote_amount: 400,
        tlcs: vec![],
    };
    let resigned = SettlementData {
        local_amount: 500,
        remote_amount: 600,
        tlcs: vec![],
    };
    store
        .update_pending_remote_settlement(node_id.clone(), channel_id, 2, original.clone())
        .unwrap();
    store
        .update_pending_remote_settlement(node_id.clone(), channel_id, 2, resigned.clone())
        .unwrap();
    let stored = store.get_watch_channel(&node_id, &channel_id).unwrap();
    assert_eq!(
        stored.remote_settlement_data,
        channel.remote_settlement_data
    );
    assert_eq!(stored.pending_remote_settlement_data, resigned);
    assert_eq!(
        store.get_superseded_remote_settlements(&node_id, &channel_id),
        vec![original]
    );
    assert!(
        matches!(
            store.get_watchtower_signer(&node_id, &channel_id),
            fiber_types::WatchtowerSignerState::External(_)
        ),
        "superseded snapshots cannot overwrite signer state"
    );
    let mut with_secret = channel.local_settlement_data.clone();
    with_secret.tlcs[0].local_key = Some(Privkey::from(&[5; 32]));
    store.update_local_settlement(node_id.clone(), channel_id, with_secret);
    assert_eq!(
        store
            .get_watch_channel(&node_id, &channel_id)
            .unwrap()
            .local_settlement_data,
        channel.local_settlement_data,
        "external channels must not persist host-held TLC secrets"
    );
    assert!(store.remove_watch_channel(node_id.clone(), channel_id));
    assert!(store
        .get_superseded_remote_settlements(&node_id, &channel_id)
        .is_empty());
    assert_eq!(
        store.get_watchtower_signer(&node_id, &channel_id),
        fiber_types::WatchtowerSignerState::Internal
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_watchtower_quotas_enforced_per_node_and_globally() {
    use crate::lsp_config::WatchtowerQuotas;
    use fiber_types::SettlementKey;

    let path = TempDir::new("test-watchtower-quotas");
    let store = open_store(path).expect("created store failed");
    let quotas = WatchtowerQuotas {
        max_channels_per_node: 2,
        max_channels_total: 3,
        max_preimages_per_node: 2,
        max_preimages_total: 3,
    };
    let node_a = NodeId::from_bytes(PeerId::random().into_bytes());
    let node_b = NodeId::from_bytes(PeerId::random().into_bytes());
    let settlement = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![],
    };
    let keys = || WatchChannelKeys {
        local_settlement: SettlementKey::external(Privkey::from(&[1; 32]).pubkey()),
        remote_settlement: Privkey::from(&[2; 32]).pubkey(),
        local_funding: Privkey::from(&[3; 32]).pubkey(),
        remote_funding: Privkey::from(&[4; 32]).pubkey(),
    };

    let ch_a1 = gen_rand_sha256_hash();
    let ch_a2 = gen_rand_sha256_hash();
    let ch_a3 = gen_rand_sha256_hash();
    let ch_b1 = gen_rand_sha256_hash();
    let ch_b2 = gen_rand_sha256_hash();

    store
        .insert_watch_channel_checked(
            node_a.clone(),
            ch_a1,
            None,
            keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .expect("a1");
    store
        .insert_watch_channel_checked(
            node_a.clone(),
            ch_a2,
            None,
            keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .expect("a2");
    // Re-inserting an existing channel is idempotent even at the per-node limit.
    store
        .insert_watch_channel_checked(
            node_a.clone(),
            ch_a1,
            None,
            keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .expect("a1 idempotent");
    let err = store
        .insert_watch_channel_checked(
            node_a.clone(),
            ch_a3,
            None,
            keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .unwrap_err();
    assert!(err.contains("quota exceeded for node"), "{err}");

    store
        .insert_watch_channel_checked(
            node_b.clone(),
            ch_b1,
            None,
            keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .expect("b1");
    let err = store
        .insert_watch_channel_checked(
            node_b.clone(),
            ch_b2,
            None,
            keys(),
            settlement,
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .unwrap_err();
    assert!(err.contains("global watchtower channel quota"), "{err}");

    // Same enforcement for preimages.
    let h_a1 = gen_rand_sha256_hash();
    let h_a2 = gen_rand_sha256_hash();
    let h_a3 = gen_rand_sha256_hash();
    let h_b1 = gen_rand_sha256_hash();
    let h_b2 = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();

    store
        .insert_watch_preimage_checked(node_a.clone(), h_a1, preimage, &quotas)
        .expect("preimage a1");
    store
        .insert_watch_preimage_checked(node_a.clone(), h_a2, preimage, &quotas)
        .expect("preimage a2");
    store
        .insert_watch_preimage_checked(node_a.clone(), h_a1, preimage, &quotas)
        .expect("preimage a1 idempotent");
    let err = store
        .insert_watch_preimage_checked(node_a, h_a3, preimage, &quotas)
        .unwrap_err();
    assert!(err.contains("preimage quota exceeded for node"), "{err}");

    store
        .insert_watch_preimage_checked(node_b.clone(), h_b1, preimage, &quotas)
        .expect("preimage b1");
    let err = store
        .insert_watch_preimage_checked(node_b.clone(), h_b2, preimage, &quotas)
        .unwrap_err();
    assert!(err.contains("global watchtower preimage quota"), "{err}");

    // Removing a channel or preimage updates the cached counters and frees quota immediately.
    assert!(store.remove_watch_channel(node_b.clone(), ch_b1));
    store
        .insert_watch_channel_checked(
            node_b.clone(),
            ch_b2,
            None,
            keys(),
            SettlementData {
                local_amount: 100,
                remote_amount: 200,
                tlcs: vec![],
            },
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .expect("b2 succeeds after removing b1");
    store.remove_watch_preimage(node_b.clone(), h_b1);
    store
        .insert_watch_preimage_checked(node_b, h_b2, preimage, &quotas)
        .expect("preimage b2 succeeds after removing b1");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_watchtower_host_rows_excluded_from_tenant_global_quotas() {
    use crate::lsp_config::WatchtowerQuotas;
    use fiber_types::SettlementKey;

    let path = TempDir::new("test-watchtower-host-excluded-quotas");
    let quotas = WatchtowerQuotas {
        max_channels_per_node: 2,
        max_channels_total: 2,
        max_preimages_per_node: 2,
        max_preimages_total: 2,
    };
    let settlement = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![],
    };
    let local_keys = || WatchChannelKeys {
        local_settlement: SettlementKey::local(Privkey::from(&[1; 32])),
        remote_settlement: Privkey::from(&[2; 32]).pubkey(),
        local_funding: Privkey::from(&[3; 32]).pubkey(),
        remote_funding: Privkey::from(&[4; 32]).pubkey(),
    };
    let tenant_keys = || WatchChannelKeys {
        local_settlement: SettlementKey::external(Privkey::from(&[1; 32]).pubkey()),
        remote_settlement: Privkey::from(&[2; 32]).pubkey(),
        local_funding: Privkey::from(&[3; 32]).pubkey(),
        remote_funding: Privkey::from(&[4; 32]).pubkey(),
    };

    let host_ch1 = gen_rand_sha256_hash();
    let host_ch2 = gen_rand_sha256_hash();
    let host_ch3 = gen_rand_sha256_hash();
    let host_h1 = gen_rand_sha256_hash();
    let host_h2 = gen_rand_sha256_hash();
    let host_h3 = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();

    // Pre-populate host (NodeId::local()) rows BEFORE quota counts are lazily initialized.
    let store = open_store(&path).expect("created store failed");
    for ch in [host_ch1, host_ch2] {
        assert!(store.insert_watch_channel(
            NodeId::local(),
            ch,
            None,
            local_keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
        ));
    }
    for h in [host_h1, host_h2] {
        store.insert_watch_preimage(NodeId::local(), h, preimage);
    }

    // Reopen store to ensure cold-start scan of persisted host rows ignores NodeId::local().
    drop(store);
    let store = open_store(&path).expect("reopen store failed");

    let tenant_a = NodeId::from_bytes(PeerId::random().into_bytes());
    let tenant_b = NodeId::from_bytes(PeerId::random().into_bytes());

    // Even with 2 host channels and 2 host preimages persisted, tenants still get the full
    // max_channels_total (2) and max_preimages_total (2) quotas.
    store
        .insert_watch_channel_checked(
            tenant_a.clone(),
            gen_rand_sha256_hash(),
            None,
            tenant_keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .expect("tenant_a ch1 must succeed despite host channels");

    // Live host insert & remove after quota initialization must also not affect tenant totals.
    assert!(store.insert_watch_channel(
        NodeId::local(),
        host_ch3,
        None,
        local_keys(),
        settlement.clone(),
        fiber_types::ChannelFeatures::V2,
        None,
    ));
    assert!(store.remove_watch_channel(NodeId::local(), host_ch1));

    store
        .insert_watch_channel_checked(
            tenant_b.clone(),
            gen_rand_sha256_hash(),
            None,
            tenant_keys(),
            settlement.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .expect("tenant_b ch1 must succeed (total tenant channels = 2)");
    let err = store
        .insert_watch_channel_checked(
            tenant_b.clone(),
            gen_rand_sha256_hash(),
            None,
            tenant_keys(),
            settlement,
            fiber_types::ChannelFeatures::V2,
            None,
            &quotas,
        )
        .unwrap_err();
    assert!(err.contains("global watchtower channel quota"), "{err}");

    // Same for preimages: host rows (pre-existing, live-inserted, or removed) never consume or
    // corrupt tenant global preimage quotas.
    store
        .insert_watch_preimage_checked(tenant_a, gen_rand_sha256_hash(), preimage, &quotas)
        .expect("tenant_a preimage 1 must succeed despite host preimages");
    store.insert_watch_preimage(NodeId::local(), host_h3, preimage);
    store.remove_watch_preimage(NodeId::local(), host_h1);
    store
        .insert_watch_preimage_checked(tenant_b.clone(), gen_rand_sha256_hash(), preimage, &quotas)
        .expect("tenant_b preimage 1 must succeed (total tenant preimages = 2)");
    let err = store
        .insert_watch_preimage_checked(tenant_b, gen_rand_sha256_hash(), preimage, &quotas)
        .unwrap_err();
    assert!(err.contains("global watchtower preimage quota"), "{err}");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_watchtower_superseded_preserved_and_cleared_on_revocation() {
    use fiber_types::SettlementKey;

    let path = TempDir::new("test-watchtower-superseded-clear");
    let store = open_store(path).expect("created store failed");
    let node_id = NodeId::from_bytes(PeerId::random().into_bytes());
    let channel_id = gen_rand_sha256_hash();
    let initial = SettlementData {
        local_amount: 1000,
        remote_amount: 0,
        tlcs: vec![],
    };
    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::external(Privkey::from(&[1; 32]).pubkey()),
            remote_settlement: Privkey::from(&[2; 32]).pubkey(),
            local_funding: Privkey::from(&[3; 32]).pubkey(),
            remote_funding: Privkey::from(&[4; 32]).pubkey(),
        },
        initial,
        fiber_types::ChannelFeatures::V2,
        None,
    );

    let variants: Vec<SettlementData> = (1..=fiber_types::MAX_SETTLEMENT_VARIANTS as u128 + 1)
        .map(|i| SettlementData {
            local_amount: 1000 - i,
            remote_amount: i,
            tlcs: vec![],
        })
        .collect();
    let (extra, retained) = variants.split_last().unwrap();
    for v in retained {
        store
            .update_pending_remote_settlement(node_id.clone(), channel_id, 2, v.clone())
            .unwrap();
    }
    assert!(store
        .update_pending_remote_settlement(node_id.clone(), channel_id, 2, extra.clone())
        .is_err());
    let superseded = store.get_superseded_remote_settlements(&node_id, &channel_id);
    assert_eq!(
        superseded,
        retained[..retained.len() - 1],
        "earliest unrevoked same-number snapshots must not be evicted"
    );

    store.update_revocation(
        node_id.clone(),
        channel_id,
        RevocationData {
            commitment_number: 1,
            aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
            output: CellOutput::default(),
            output_data: Bytes::default(),
        },
        retained.last().unwrap().clone(),
    );
    assert_eq!(
        store.get_superseded_remote_settlements(&node_id, &channel_id),
        superseded
    );
    for s in [&retained[0], extra] {
        store
            .update_pending_remote_settlement(node_id.clone(), channel_id, 3, s.clone())
            .unwrap();
    }
    let mut revocation = store
        .get_watch_channel(&node_id, &channel_id)
        .unwrap()
        .revocation_data
        .unwrap();
    revocation.commitment_number = 2;
    store.update_revocation(node_id.clone(), channel_id, revocation, extra.clone());
    // A delayed write for the revoked round must not recreate it.
    store
        .update_pending_remote_settlement(node_id.clone(), channel_id, 2, retained[1].clone())
        .unwrap();
    assert_eq!(
        store.get_superseded_remote_settlements(&node_id, &channel_id),
        vec![retained[0].clone()]
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_watchtower_v2_registration_and_legacy_superseded_round_trip() {
    let path = TempDir::new("test-watchtower-v2-store");
    let store = open_store(path).expect("created store failed");
    let node_id = NodeId::from_bytes(PeerId::random().into_bytes());
    let channel_id = gen_rand_sha256_hash();
    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![],
    };

    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(Privkey::from(&[1; 32])),
            remote_settlement: Privkey::from(&[2; 32]).pubkey(),
            local_funding: Privkey::from(&[3; 32]).pubkey(),
            remote_funding: Privkey::from(&[4; 32]).pubkey(),
        },
        settlement_data,
        fiber_types::ChannelFeatures::V2,
        None,
    );

    assert_eq!(
        store
            .get_watch_channels()
            .into_iter()
            .find(|channel| channel.channel_id == channel_id)
            .expect("stored channel")
            .channel_features,
        ChannelFeatures::V2
    );

    // Released rows have only the unnumbered sidecar.
    let key = |prefix: u8| [&[prefix], node_id.as_ref(), channel_id.as_ref()].concat();
    store.delete(key(
        fiber_types::schema::WATCHTOWER_NUMBERED_SETTLEMENT_PREFIX,
    ));
    let initial = store
        .get_watch_channel(&node_id, &channel_id)
        .unwrap()
        .remote_settlement_data;
    let legacy = SettlementData {
        local_amount: 125,
        remote_amount: 175,
        tlcs: vec![],
    };
    store.put(
        key(fiber_types::schema::WATCHTOWER_SUPERSEDED_SETTLEMENT_PREFIX),
        bincode::serialize(&vec![legacy.clone()]).unwrap(),
    );
    let acknowledged = SettlementData {
        local_amount: 150,
        remote_amount: 150,
        tlcs: vec![],
    };
    store.update_revocation(
        node_id.clone(),
        channel_id,
        RevocationData {
            commitment_number: 1,
            aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
            output: CellOutput::default(),
            output_data: Bytes::default(),
        },
        acknowledged.clone(),
    );
    store
        .update_pending_remote_settlement(node_id.clone(), channel_id, 3, acknowledged)
        .unwrap();
    assert_eq!(
        store.get_superseded_remote_settlements(&node_id, &channel_id),
        vec![legacy, initial]
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_preimage() {
    let path = TempDir::new("test-watchtower-store");
    let store = open_store(path).expect("created store failed");

    let node_id_a = NodeId::local();
    let preimage_a = gen_rand_sha256_hash();
    let payment_hash_a = blake2b_256(preimage_a).into();

    let node_id_b = NodeId::from_bytes(PeerId::random().into_bytes());
    let preimage_b = gen_rand_sha256_hash();
    let payment_hash_b = blake2b_256(preimage_b).into();

    let preimage_c = gen_rand_sha256_hash();
    let payment_hash_c = blake2b_256(preimage_c).into();

    store.insert_watch_preimage(node_id_a.clone(), payment_hash_a, preimage_a);
    store.insert_watch_preimage(node_id_b.clone(), payment_hash_b, preimage_b);

    assert_eq!(
        store
            .get_watch_preimage(&node_id_a, &payment_hash_a)
            .unwrap(),
        preimage_a,
        "query watch preimage"
    );
    assert!(
        store
            .get_watch_preimage(&node_id_b, &payment_hash_a)
            .is_none(),
        "watch preimage should be scoped by node"
    );
    assert!(
        store.get_preimage(&payment_hash_a).is_some(),
        "normal preimage lookup should still read local watchtower preimages"
    );
    assert!(
        store.get_preimage(&payment_hash_b).is_none(),
        "normal preimage lookup must not read another node's watchtower preimage"
    );

    // watch preimage should not return a node preimage
    store.insert_preimage(payment_hash_c, preimage_c);
    assert!(
        store
            .get_watch_preimage(&node_id_a, &payment_hash_c)
            .is_none(),
        "query non exist watch preimage"
    );

    // delete preimage with wrong node
    store.remove_watch_preimage(node_id_a, payment_hash_b);
    assert!(
        store
            .get_watch_preimage(&node_id_b, &payment_hash_b)
            .is_some(),
        "exist"
    );

    store.remove_watch_preimage(node_id_b.clone(), payment_hash_b);
    assert!(
        store
            .get_watch_preimage(&node_id_b, &payment_hash_b)
            .is_none(),
        "removed"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_preimage_gc_waits_for_watched_tlc() {
    let path = TempDir::new("test-watchtower-preimage-gc-waits");
    let store = open_store(path).expect("created store failed");

    let node_id = NodeId::local();
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let payment_hash = blake2b_256(preimage).into();
    let local_settlement_key = Privkey::from(&[1; 32]);
    let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
    let local_funding_pubkey = Privkey::from(&[3; 32]).pubkey();
    let remote_funding_pubkey = Privkey::from(&[4; 32]).pubkey();
    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 42,
            payment_hash,
            expiry: now_timestamp_as_millis_u64() + 60_000,
            local_key: Some(Privkey::from(&[5; 32])),
            local_key_pubkey: None,
            local_key_commitment_number: None,
            remote_key: Privkey::from(&[6; 32]).pubkey(),
        }],
    };

    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(local_settlement_key.clone()),
            remote_settlement: remote_settlement_key,
            local_funding: local_funding_pubkey,
            remote_funding: remote_funding_pubkey,
        },
        settlement_data,
        fiber_types::ChannelFeatures::V2,
        None,
    );
    store.insert_watch_preimage(node_id.clone(), payment_hash, preimage);

    store.remove_watch_preimage(node_id.clone(), payment_hash);
    assert_eq!(
        store.get_watch_preimage(&node_id, &payment_hash),
        Some(preimage),
        "preimage is retained while a watched TLC still references it"
    );

    store.insert_onchain_tlc_settlement(
        &node_id,
        &channel_id,
        TLCId::Offered(0),
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: gen_rand_sha256_hash(),
            tlc_index: 0,
        },
    );
    assert!(
        store.get_watch_preimage(&node_id, &payment_hash).is_none(),
        "watchtower GC removes the preimage after the watched TLC is settled"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_preimage_gc_waits_for_same_hash_sibling_tlc() {
    let path = TempDir::new("test-watchtower-preimage-gc-same-hash-sibling");
    let store = open_store(path).expect("created store failed");

    let node_id = NodeId::local();
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
    let local_settlement_key = Privkey::from(&[1; 32]);
    let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
    let local_funding_pubkey = Privkey::from(&[3; 32]).pubkey();
    let remote_funding_pubkey = Privkey::from(&[4; 32]).pubkey();
    let first_tlc = SettlementTlc {
        tlc_id: TLCId::Offered(0),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 21,
        payment_hash,
        expiry: now_timestamp_as_millis_u64() + 60_000,
        local_key: Some(Privkey::from(&[5; 32])),
        local_key_pubkey: None,
        local_key_commitment_number: None,
        remote_key: Privkey::from(&[6; 32]).pubkey(),
    };
    let mut second_tlc = first_tlc.clone();
    second_tlc.tlc_id = TLCId::Offered(1);
    second_tlc.local_key = Some(Privkey::from(&[7; 32]));
    second_tlc.remote_key = Privkey::from(&[8; 32]).pubkey();
    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![first_tlc, second_tlc],
    };

    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(local_settlement_key.clone()),
            remote_settlement: remote_settlement_key,
            local_funding: local_funding_pubkey,
            remote_funding: remote_funding_pubkey,
        },
        settlement_data,
        fiber_types::ChannelFeatures::V2,
        None,
    );
    store.insert_watch_preimage(node_id.clone(), payment_hash, preimage);

    store.insert_onchain_tlc_settlement(
        &node_id,
        &channel_id,
        TLCId::Offered(0),
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: gen_rand_sha256_hash(),
            tlc_index: 0,
        },
    );

    assert_eq!(
        store.get_watch_preimage(&node_id, &payment_hash),
        Some(preimage),
        "the sibling TLC with the same payment hash still needs the preimage"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_preimage_gc_isolated_between_tenants() {
    let path = TempDir::new("test-watchtower-preimage-gc-tenant-scope");
    let store = open_store(path).expect("created store failed");

    let node_id_a = NodeId::from_bytes(PeerId::random().into_bytes());
    let node_id_b = NodeId::from_bytes(PeerId::random().into_bytes());
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 42,
            payment_hash,
            expiry: now_timestamp_as_millis_u64() + 60_000,
            local_key: Some(Privkey::from(&[5; 32])),
            local_key_pubkey: None,
            local_key_commitment_number: None,
            remote_key: Privkey::from(&[6; 32]).pubkey(),
        }],
    };

    for node_id in [&node_id_a, &node_id_b] {
        store.insert_watch_channel(
            node_id.clone(),
            channel_id,
            None,
            WatchChannelKeys {
                local_settlement: SettlementKey::local(Privkey::from(&[1; 32])),
                remote_settlement: Privkey::from(&[2; 32]).pubkey(),
                local_funding: Privkey::from(&[3; 32]).pubkey(),
                remote_funding: Privkey::from(&[4; 32]).pubkey(),
            },
            settlement_data.clone(),
            fiber_types::ChannelFeatures::V2,
            None,
        );
        store.insert_watch_preimage(node_id.clone(), payment_hash, preimage);
    }

    // This settlement belongs to node A's watched channel. Node B still needs its independently
    // scoped preimage for a TLC with the same endpoint-local identity and complete payment hash.
    store.insert_onchain_tlc_settlement(
        &node_id_a,
        &channel_id,
        TLCId::Offered(0),
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: gen_rand_sha256_hash(),
            tlc_index: 0,
        },
    );

    assert_eq!(store.get_watch_preimage(&node_id_a, &payment_hash), None);
    assert_eq!(
        store.get_watch_preimage(&node_id_b, &payment_hash),
        Some(preimage),
        "one tenant's settlement must not garbage-collect another tenant's preimage"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_preimage_gc_ignores_ambiguous_legacy_settlement() {
    let path = TempDir::new("test-watchtower-preimage-gc-legacy-ambiguous");
    let store = open_store(path).expect("created store failed");

    let node_id = NodeId::local();
    let channel_id = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 42,
            payment_hash,
            expiry: now_timestamp_as_millis_u64() + 60_000,
            local_key: Some(Privkey::from(&[5; 32])),
            local_key_pubkey: None,
            local_key_commitment_number: None,
            remote_key: Privkey::from(&[6; 32]).pubkey(),
        }],
    };

    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(Privkey::from(&[1; 32])),
            remote_settlement: Privkey::from(&[2; 32]).pubkey(),
            local_funding: Privkey::from(&[3; 32]).pubkey(),
            remote_funding: Privkey::from(&[4; 32]).pubkey(),
        },
        settlement_data,
        fiber_types::ChannelFeatures::V2,
        None,
    );
    store.insert_watch_preimage(node_id.clone(), payment_hash, preimage);

    let payment_hash_prefix: [u8; 20] = payment_hash.as_ref()[..20]
        .try_into()
        .expect("payment hash prefix");
    let legacy_key = [
        &[WATCHTOWER_TLC_SETTLED_PREFIX],
        channel_id.as_ref(),
        payment_hash_prefix.as_ref(),
    ]
    .concat();
    store.put(legacy_key, []);
    store.remove_watch_preimage(node_id.clone(), payment_hash);

    assert_eq!(
        store.get_watch_preimage(&node_id, &payment_hash),
        Some(preimage),
        "a prefix-keyed legacy record cannot prove that this TLC no longer needs the preimage"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_signer_roundtrip() {
    use fiber_types::{
        LastAppliedWatchtowerSignature, OnchainKeyPurpose, OnchainSigningContent,
        WatchtowerExternalSignerState, WatchtowerSignerState,
    };
    use std::collections::BTreeMap;

    let path = TempDir::new("test-watchtower-signer-roundtrip");
    let store = open_store(path).expect("created store failed");
    let node_id = NodeId::local();
    let channel_id = gen_rand_sha256_hash();

    // 1. Initial lookup on non-existent record returns Internal
    assert_eq!(
        store.get_watchtower_signer(&node_id, &channel_id),
        WatchtowerSignerState::Internal
    );

    // 2. Put and get format with pending and signed requests
    let req_id_1 = Hash256::from([1u8; 32]);
    let req_id_2 = Hash256::from([2u8; 32]);
    let content_1 = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: Transaction::default(),
    };
    let content_2 = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Tlc {
            commitment_number: 5,
        },
        transaction: Transaction::default(),
    };
    let sig_2 = [77u8; 65];

    let mut pending = BTreeMap::new();
    pending.insert(req_id_1, content_1);
    let mut signed = BTreeMap::new();
    signed.insert(req_id_2, (content_2, sig_2));
    let last_applied = Some(LastAppliedWatchtowerSignature {
        request_id: req_id_2,
        signature: sig_2,
    });

    let new_state = WatchtowerSignerState::External(WatchtowerExternalSignerState {
        pending_requests: pending,
        signed_signatures: signed,
        last_applied,
        ..Default::default()
    });

    store.put_watchtower_signer(&node_id, &channel_id, new_state.clone());
    assert_eq!(
        store.get_watchtower_signer(&node_id, &channel_id),
        new_state
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_watchtower_atomic_update_preserves_concurrent_signature() {
    use std::sync::mpsc;
    use std::time::Duration;

    use fiber_types::watchtower::WatchtowerExternalSignerState;
    use fiber_types::{
        LastAppliedWatchtowerSignature, OnchainKeyPurpose, OnchainSigningContent,
        WatchtowerSignerState,
    };

    let path = TempDir::new("watchtower-concurrent-signer-update");
    let store = open_store(path).expect("open store");
    let node_id = NodeId::local();
    let channel_id = Hash256::from([1; 32]);
    let signed_id = Hash256::from([2; 32]);
    let new_id = Hash256::from([3; 32]);
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: Transaction::default(),
    };
    let signature = [42; 65];
    let mut initial = WatchtowerExternalSignerState::default();
    initial.pending_requests.insert(signed_id, content.clone());
    store.put_watchtower_signer(
        &node_id,
        &channel_id,
        WatchtowerSignerState::External(initial),
    );

    let (scan_entered_tx, scan_entered_rx) = mpsc::channel();
    let (release_scan_tx, release_scan_rx) = mpsc::channel();
    let (submit_entered_tx, submit_entered_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let store = &store;
        let node_id = &node_id;
        let content = &content;
        let scan = scope.spawn(move || {
            store
                .update_watchtower_signer(node_id, &channel_id, |state| {
                    scan_entered_tx.send(()).expect("scan entered");
                    release_scan_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release scan");
                    let WatchtowerSignerState::External(external) = state else {
                        panic!("external signer");
                    };
                    external.pending_requests.insert(new_id, content.clone());
                    Ok::<_, ()>(())
                })
                .expect("scan update");
        });
        scan_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("scan read state");
        let submit = scope.spawn(move || {
            store
                .update_watchtower_signer(node_id, &channel_id, |state| {
                    submit_entered_tx.send(()).expect("submit entered");
                    let WatchtowerSignerState::External(external) = state else {
                        panic!("external signer");
                    };
                    let pending = external
                        .pending_requests
                        .remove(&signed_id)
                        .expect("pending signature");
                    external
                        .signed_signatures
                        .insert(signed_id, (pending, signature));
                    external.last_applied = Some(LastAppliedWatchtowerSignature {
                        request_id: signed_id,
                        signature,
                    });
                    Ok::<_, ()>(())
                })
                .expect("submit update");
        });
        // Without an atomic update the submitter can save its signature while
        // the scan still holds the old snapshot. With the lock it waits here.
        if submit_entered_rx
            .recv_timeout(Duration::from_millis(200))
            .is_ok()
        {
            submit.join().expect("submit thread");
            release_scan_tx.send(()).expect("release scan");
            scan.join().expect("scan thread");
        } else {
            release_scan_tx.send(()).expect("release scan");
            scan.join().expect("scan thread");
            submit.join().expect("submit thread");
        }
    });
    let WatchtowerSignerState::External(state) = store.get_watchtower_signer(&node_id, &channel_id)
    else {
        panic!("external signer");
    };
    assert_eq!(
        state.signed_signatures.get(&signed_id),
        Some(&(content.clone(), signature)),
        "scan must not overwrite an accepted signature"
    );
    assert!(!state.pending_requests.contains_key(&signed_id));
    assert_eq!(state.pending_requests.get(&new_id), Some(&content));
    assert_eq!(
        state.last_applied,
        Some(LastAppliedWatchtowerSignature {
            request_id: signed_id,
            signature
        })
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_watchtower_atomic_update_discards_failed_mutation() {
    use fiber_types::watchtower::WatchtowerExternalSignerState;
    use fiber_types::WatchtowerSignerState;

    let path = TempDir::new("watchtower-failed-signer-update");
    let store = open_store(path).expect("open store");
    let node_id = NodeId::local();
    let channel_id = Hash256::from([1; 32]);
    let result = store.update_watchtower_signer(&node_id, &channel_id, |state| {
        *state = WatchtowerSignerState::External(WatchtowerExternalSignerState::default());
        Err::<(), _>("invalid signature")
    });
    assert_eq!(result, Err("invalid signature"));
    assert_eq!(
        store.get_watchtower_signer(&node_id, &channel_id),
        WatchtowerSignerState::Internal
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_onchain_tlc_settlement_roundtrip() {
    let path = TempDir::new("test-onchain-tlc-settlement-roundtrip");
    let store = open_store(path).expect("created store failed");
    let channel_id = Hash256::from([1u8; 32]);
    let other_channel_id = Hash256::from([2u8; 32]);
    let payment_hash = Hash256::from([3u8; 32]);
    let tlc_id = TLCId::Received(42);
    let node_id = NodeId::local();

    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id,
            &channel_id,
            tlc_id,
            &payment_hash,
        ),
        None
    );

    let settlement = OnChainTlcSettlement {
        payment_hash,
        hash_algorithm: HashAlgorithm::Sha256,
        preimage: Some(Hash256::from([9u8; 32])),
        tx_hash: Hash256::from([7u8; 32]),
        tlc_index: 2,
    };
    store.insert_onchain_tlc_settlement(&node_id, &channel_id, tlc_id, settlement.clone());

    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id,
            &channel_id,
            tlc_id,
            &payment_hash,
        ),
        Some(settlement)
    );
    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id,
            &other_channel_id,
            tlc_id,
            &payment_hash,
        ),
        None
    );
}

#[cfg(all(not(target_arch = "wasm32"), feature = "watchtower"))]
#[test]
fn test_onchain_tlc_settlement_resolves_hosted_tenant_namespace() {
    let path = TempDir::new("onchain_tlc_settlement_namespace");
    let store = open_store(path).expect("create shared store");
    let tenant_u1 = store.namespaced(NodeNamespace::hosted_tenant("u1"));
    let tenant_u2 = store.namespaced(NodeNamespace::hosted_tenant("u2"));
    let unowned_tenant = store.namespaced(NodeNamespace::hosted_tenant("u3"));
    let u1_state = ChannelActorState::samples(42).remove(0);
    let mut u2_state = ChannelActorState::samples(43).remove(0);
    let channel_id = u1_state.get_id();
    u2_state.id = channel_id;
    let u1_node_id = NodeId::from_bytes(u1_state.get_local_pubkey().serialize().to_vec());
    let u2_node_id = NodeId::from_bytes(u2_state.get_local_pubkey().serialize().to_vec());
    assert_ne!(u1_node_id, u2_node_id);
    tenant_u1.insert_channel_actor_state(u1_state);
    tenant_u2.insert_channel_actor_state(u2_state);

    let preimage = Hash256::from([9; 32]);
    let payment_hash = HashAlgorithm::CkbHash.hash(preimage).into();
    let fulfilled = OnChainTlcSettlement {
        payment_hash,
        hash_algorithm: HashAlgorithm::CkbHash,
        preimage: Some(preimage),
        tx_hash: Hash256::from([7; 32]),
        tlc_index: 0,
    };
    let timed_out = OnChainTlcSettlement {
        preimage: None,
        tx_hash: Hash256::from([8; 32]),
        ..fulfilled.clone()
    };
    let read = |reader: &crate::store::Store, channel_id: &Hash256, tlc_id| {
        ChannelActorStateStore::get_onchain_tlc_settlement(
            reader,
            channel_id,
            tlc_id,
            &payment_hash,
        )
    };

    // The host watchtower writes outside the tenant namespace. Even identical
    // channel/TLC ids must select the proof belonging to this logical node.
    for (tlc_id, u1_proof, u2_proof) in [
        (TLCId::Offered(0), &fulfilled, &timed_out),
        (TLCId::Received(0), &timed_out, &fulfilled),
    ] {
        store.insert_onchain_tlc_settlement(&u1_node_id, &channel_id, tlc_id, u1_proof.clone());
        assert_eq!(
            read(&tenant_u1, &channel_id, tlc_id),
            Some(u1_proof.clone())
        );
        assert_eq!(read(&tenant_u2, &channel_id, tlc_id), None);
        assert_eq!(read(&store, &channel_id, tlc_id), None);
        assert_eq!(read(&unowned_tenant, &channel_id, tlc_id), None);

        store.insert_onchain_tlc_settlement(&u2_node_id, &channel_id, tlc_id, u2_proof.clone());
        let public_proof = OnChainTlcSettlement {
            tx_hash: Hash256::from([6; 32]),
            ..fulfilled.clone()
        };
        store.insert_onchain_tlc_settlement(
            &NodeId::local(),
            &channel_id,
            tlc_id,
            public_proof.clone(),
        );
        for (reader, expected) in [
            (&tenant_u1, u1_proof),
            (&tenant_u2, u2_proof),
            (&store, &public_proof),
        ] {
            assert_eq!(read(reader, &channel_id, tlc_id), Some(expected.clone()));
            assert_eq!(read(reader, &channel_id, TLCId::Offered(1)), None);
        }
        assert_eq!(read(&unowned_tenant, &channel_id, tlc_id), None);
    }

    // A proof under U1's identity alone is not enough: the channel must also
    // belong to U1's namespace before shared watchtower data can be read.
    let unowned_channel_id = Hash256::from([91; 32]);
    store.insert_onchain_tlc_settlement(
        &u1_node_id,
        &unowned_channel_id,
        TLCId::Offered(0),
        fulfilled,
    );
    assert_eq!(
        read(&tenant_u1, &unowned_channel_id, TLCId::Offered(0)),
        None
    );
}

#[cfg(all(not(target_arch = "wasm32"), feature = "watchtower"))]
#[test]
fn test_onchain_tlc_settlement_hosted_tenant_rejects_unscoped_legacy_proof() {
    let path = TempDir::new("onchain_tlc_settlement_legacy_namespace");
    let store = open_store(path).expect("create shared store");
    let tenant = store.namespaced(NodeNamespace::hosted_tenant("u1"));
    let state = ChannelActorState::samples(42).remove(0);
    let channel_id = state.get_id();
    tenant.insert_channel_actor_state(state);
    let payment_hash = Hash256::from([3; 32]);
    let tlc_id = TLCId::Offered(0);
    let legacy_key = [
        &[WATCHTOWER_TLC_SETTLED_PREFIX],
        channel_id.as_ref(),
        &payment_hash.as_ref()[..20],
    ]
    .concat();
    for value in [vec![], vec![0, 0, 0]] {
        store.put(&legacy_key, value);
        // V2 requires exact proofs with node and TLC identity. Neither the
        // public node nor a tenant may consume unscoped legacy rows.
        assert_eq!(
            ChannelActorStateStore::get_onchain_tlc_settlement(
                &store,
                &channel_id,
                tlc_id,
                &payment_hash,
            ),
            None
        );
        assert_eq!(
            ChannelActorStateStore::get_onchain_tlc_settlement(
                &tenant,
                &channel_id,
                tlc_id,
                &payment_hash,
            ),
            None
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_onchain_tlc_settlements_with_shared_prefix_are_independent() {
    let path = TempDir::new("test-onchain-tlc-settlement-shared-prefix");
    let store = open_store(path).expect("created store failed");
    let channel_id = Hash256::from([1u8; 32]);
    let first_hash = Hash256::from([3u8; 32]);
    let mut second_hash_bytes = [3u8; 32];
    second_hash_bytes[31] = 4;
    let second_hash = Hash256::from(second_hash_bytes);
    let first_id = TLCId::Offered(0);
    let second_id = TLCId::Offered(1);
    let node_id = NodeId::local();
    let first = OnChainTlcSettlement {
        payment_hash: first_hash,
        hash_algorithm: HashAlgorithm::CkbHash,
        preimage: None,
        tx_hash: Hash256::from([7u8; 32]),
        tlc_index: 0,
    };
    let second = OnChainTlcSettlement {
        payment_hash: second_hash,
        hash_algorithm: HashAlgorithm::CkbHash,
        preimage: None,
        tx_hash: Hash256::from([8u8; 32]),
        tlc_index: 1,
    };

    store.insert_onchain_tlc_settlement(&node_id, &channel_id, first_id, first.clone());
    store.insert_onchain_tlc_settlement(&node_id, &channel_id, second_id, second.clone());

    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id,
            &channel_id,
            first_id,
            &first_hash,
        ),
        Some(first)
    );
    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id,
            &channel_id,
            second_id,
            &second_hash,
        ),
        Some(second)
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_onchain_tlc_settlement_no_preimage_does_not_downgrade() {
    let path = TempDir::new("test-onchain-tlc-settlement-no-downgrade");
    let store = open_store(path).expect("created store failed");
    let channel_id = Hash256::from([1u8; 32]);
    let payment_hash = Hash256::from([3u8; 32]);
    let tlc_id = TLCId::Offered(0);
    let node_id = NodeId::local();

    let with_preimage = OnChainTlcSettlement {
        payment_hash,
        hash_algorithm: HashAlgorithm::CkbHash,
        preimage: Some(Hash256::from([9u8; 32])),
        tx_hash: Hash256::from([7u8; 32]),
        tlc_index: 0,
    };
    store.insert_onchain_tlc_settlement(&node_id, &channel_id, tlc_id, with_preimage.clone());
    store.insert_onchain_tlc_settlement(
        &node_id,
        &channel_id,
        tlc_id,
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: Hash256::from([8u8; 32]),
            tlc_index: 0,
        },
    );

    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id,
            &channel_id,
            tlc_id,
            &payment_hash,
        ),
        Some(with_preimage)
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_onchain_tlc_settlements_are_isolated_between_watchtower_tenants() {
    let path = TempDir::new("test-onchain-tlc-settlement-tenant-scope");
    let store = open_store(path).expect("created store failed");
    let channel_id = Hash256::from([1u8; 32]);
    let payment_hash = Hash256::from([3u8; 32]);
    let tlc_id = TLCId::Offered(0);
    let node_id_a = NodeId::from_bytes(PeerId::random().into_bytes());
    let node_id_b = NodeId::from_bytes(PeerId::random().into_bytes());

    store.insert_onchain_tlc_settlement(
        &node_id_a,
        &channel_id,
        tlc_id,
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: Some(Hash256::from([9u8; 32])),
            tx_hash: Hash256::from([7u8; 32]),
            tlc_index: 0,
        },
    );
    store.insert_onchain_tlc_settlement(
        &node_id_b,
        &channel_id,
        tlc_id,
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: Hash256::from([8u8; 32]),
            tlc_index: 0,
        },
    );

    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id_a,
            &channel_id,
            tlc_id,
            &payment_hash,
        ),
        Some(OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: Some(Hash256::from([9u8; 32])),
            tx_hash: Hash256::from([7u8; 32]),
            tlc_index: 0,
        })
    );
    assert_eq!(
        WatchtowerStore::get_onchain_tlc_settlement(
            &store,
            &node_id_b,
            &channel_id,
            tlc_id,
            &payment_hash,
        ),
        Some(OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: Hash256::from([8u8; 32]),
            tlc_index: 0,
        })
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_with_wrong_node_id() {
    let path = TempDir::new("test-watchtower-store");
    let store = open_store(path).expect("created store failed");

    let node_id = NodeId::from_bytes(PeerId::random().into_bytes());
    let wrong_node_id = NodeId::from_bytes(PeerId::random().into_bytes());
    let channel_id = gen_rand_sha256_hash();

    let local_settlement_key = Privkey::from(&[1; 32]);
    let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
    let local_funding_pubkey = Privkey::from(&[3; 32]).pubkey();
    let remote_funding_pubkey = Privkey::from(&[4; 32]).pubkey();

    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![],
    };

    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(local_settlement_key.clone()),
            remote_settlement: remote_settlement_key,
            local_funding: local_funding_pubkey,
            remote_funding: remote_funding_pubkey,
        },
        settlement_data.clone(),
        fiber_types::ChannelFeatures::V2,
        None,
    );
    let expected_value = vec![ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: Some(local_settlement_key.clone()),
        local_settlement_key_pubkey: Some(local_settlement_key.pubkey()),
        remote_settlement_key,
        local_funding_pubkey,
        remote_funding_pubkey,
        revocation_data: None,
        local_settlement_data: settlement_data.clone(),
        pending_remote_settlement_data: settlement_data.clone(),
        remote_settlement_data: settlement_data.clone(),
        channel_features: Default::default(),
        payout_lock_script: None,
    }];
    assert_eq!(store.get_watch_channels(), expected_value);

    // update with wrong node_id
    let revocation_data = RevocationData {
        commitment_number: 0,
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        output: CellOutput::default(),
        output_data: Bytes::default(),
    };

    store.update_revocation(
        wrong_node_id.clone(),
        channel_id,
        revocation_data.clone(),
        settlement_data.clone(),
    );
    assert_eq!(store.get_watch_channels(), expected_value);

    // remove wrong_node_id
    store.remove_watch_channel(wrong_node_id, channel_id);
    assert_eq!(store.get_watch_channels(), expected_value);

    store.remove_watch_channel(node_id, channel_id);
    assert_eq!(store.get_watch_channels(), vec![]);
}
#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_channel_state_serialize() {
    let state = ChannelState::AwaitingChannelReady(AwaitingChannelReadyFlags::CHANNEL_READY);
    let bincode_encoded = bincode::serialize(&state).unwrap();
    let new_state: ChannelState = bincode::deserialize(&bincode_encoded).unwrap();
    assert_eq!(state, new_state);

    let flags = SigningCommitmentFlags::COMMITMENT_SIGNED_SENT;
    let bincode_encoded = bincode::serialize(&flags).unwrap();
    let new_flags: SigningCommitmentFlags = bincode::deserialize(&bincode_encoded).unwrap();
    assert_eq!(flags, new_flags);
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_channel_actor_state_store() {
    let seed = [0u8; 32];
    let signer = InMemorySigner::generate_from_seed(&seed);

    let seckey = blake2b_hash_with_salt(
        signer.musig2_base_nonce.as_ref(),
        b"channel_announcement".as_slice(),
    );
    let sec_nonce = SecNonce::build(seckey).build();
    let pub_nonce = sec_nonce.public_nonce();
    let channel_id = gen_rand_sha256_hash();
    let state = ChannelActorState {
        channel_signer: crate::fiber::channel_signer::ChannelSigner::local(signer.clone()),
        core: ChannelActorData {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
            signing_context: Default::default(),
            public_channel_info: Some(PublicChannelInfo {
                local_channel_announcement_signature: Some((
                    mock_ecdsa_signature(),
                    MaybeScalar::two(),
                )),
                remote_channel_announcement_signature: Some((
                    mock_ecdsa_signature(),
                    MaybeScalar::two(),
                )),
                remote_channel_announcement_nonce: Some(pub_nonce.clone()),
                channel_announcement: None,
                channel_update: None,
            }),
            local_tlc_info: ChannelTlcInfo {
                enabled: false,
                timestamp: 0,
                tlc_fee_proportional_millionths: 123,
                tlc_expiry_delta: 3,
                tlc_minimum_value: 10,
            },
            remote_tlc_info: None,
            local_pubkey: gen_rand_fiber_public_key(),
            remote_pubkey: gen_rand_fiber_public_key(),
            funding_tx: Some(Transaction::default()),
            funding_tx_confirmed_at: Some((H256::default(), 1, 1)),
            is_acceptor: true,
            is_one_way: false,
            funding_udt_type_script: Some(Script::default()),
            to_local_amount: 100,
            to_remote_amount: 100,
            commitment_fee_rate: 100,
            commitment_delay_epoch: 100,
            funding_fee_rate: 100,
            id: channel_id,
            tlc_state: Default::default(),
            retryable_tlc_operations: Default::default(),
            waiting_forward_tlc_tasks: Default::default(),
            local_shutdown_script: Script::default(),
            local_channel_public_keys: signer.get_base_public_keys(),
            session: crate::fiber::session::new_session(channel_id, &signer.funding_key, None),
            signer: Some(signer),
            remote_channel_public_keys: Some(ChannelBasePublicKeys {
                funding_pubkey: gen_rand_fiber_public_key(),
                tlc_base_key: gen_rand_fiber_public_key(),
            }),
            commitment_numbers: Default::default(),
            remote_shutdown_script: Some(Script::default()),
            remote_commitment_points: vec![
                (0, gen_rand_fiber_public_key()),
                (1, gen_rand_fiber_public_key()),
            ],
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash: None,
            local_reserved_ckb_amount: 100,
            remote_reserved_ckb_amount: 100,
            local_constraints: ChannelConstraints::default(),
            remote_constraints: ChannelConstraints::default(),
            reestablishing: false,
            connectivity_state: fiber_types::ChannelConnectivityState::Online,
            pending_replay_updates: vec![TlcReplayUpdate::Add(AddTlc {
                channel_id,
                tlc_id: 1,
                amount: 1000,
                payment_hash: gen_rand_sha256_hash(),
                expiry: 1200,
                hash_algorithm: HashAlgorithm::CkbHash,
                onion_packet: None,
            })],
            last_was_revoke: true,
            external_funding: None,
            channel_features: Default::default(),
            created_at: SystemTime::now(),
        },
        waiting_peer_response: None,
        reestablish_timer: None,
        recovery_peer_v2: None,
        network: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        defer_peer_tlc_updates: false,
        deferred_peer_tlc_updates: Default::default(),
        ephemeral_config: Default::default(),
        funding_abort_detail: None,
        private_key: None,
        needs_backup: false,
        pending_messages: Default::default(),
        musig2_agg_context_local_first: Default::default(),
        musig2_agg_context_remote_first: Default::default(),
        pending_remove_effects_v2: 0,
        pending_payment_notifications: Default::default(),
        replayed_tlc_updates_v2: 0,
        resigning_outgoing_v2: false,
    };

    let bincode_encoded = bincode::serialize(&state).unwrap();
    let _new_state: ChannelActorState = bincode::deserialize(&bincode_encoded).unwrap();

    let path = TempDir::new("channel_actore_store");

    let store = open_store(path).expect("create store failed");
    assert!(store.get_channel_actor_state(&state.id).is_none());
    store.insert_channel_actor_state(state.clone());

    let get_state = store.get_channel_actor_state(&state.id).unwrap();
    assert!(!get_state.is_tlc_forwarding_enabled());
    assert_eq!(get_state.pending_replay_updates.len(), 1);
    assert!(matches!(
        get_state.pending_replay_updates.first(),
        Some(TlcReplayUpdate::Add(add)) if add.channel_id == channel_id && add.tlc_id == 1
    ));
    assert_eq!(get_state.last_was_revoke, state.last_was_revoke);

    let remote_pubkey = state.get_remote_pubkey();
    assert_eq!(
        store.get_channel_ids_by_pubkey(&remote_pubkey),
        vec![state.id]
    );
    let channel_point = state.must_get_funding_transaction_outpoint();
    assert!(store
        .get_channel_state_by_outpoint(&channel_point)
        .is_some());

    store.delete_channel_actor_state(&state.id);
    assert!(store.get_channel_actor_state(&state.id).is_none());
    assert_eq!(store.get_channel_ids_by_pubkey(&remote_pubkey), vec![]);
    let channel_point = state.must_get_funding_transaction_outpoint();
    assert!(store
        .get_channel_state_by_outpoint(&channel_point)
        .is_none());
}

fn sample_channel_actor_state(
    state: ChannelState,
    shutdown_transaction_hash: Option<H256>,
) -> ChannelActorState {
    let seed = [0u8; 32];
    let signer = InMemorySigner::generate_from_seed(&seed);

    let seckey = blake2b_hash_with_salt(
        signer.musig2_base_nonce.as_ref(),
        b"channel_announcement".as_slice(),
    );
    let sec_nonce = SecNonce::build(seckey).build();
    let pub_nonce = sec_nonce.public_nonce();

    let channel_id = gen_rand_sha256_hash();
    ChannelActorState {
        channel_signer: crate::fiber::channel_signer::ChannelSigner::local(signer.clone()),
        core: ChannelActorData {
            state,
            signing_context: Default::default(),
            public_channel_info: Some(PublicChannelInfo {
                local_channel_announcement_signature: Some((
                    mock_ecdsa_signature(),
                    MaybeScalar::two(),
                )),
                remote_channel_announcement_signature: Some((
                    mock_ecdsa_signature(),
                    MaybeScalar::two(),
                )),
                remote_channel_announcement_nonce: Some(pub_nonce.clone()),
                channel_announcement: None,
                channel_update: None,
            }),
            local_tlc_info: ChannelTlcInfo {
                enabled: false,
                timestamp: 0,
                tlc_fee_proportional_millionths: 123,
                tlc_expiry_delta: 3,
                tlc_minimum_value: 10,
            },
            remote_tlc_info: None,
            local_pubkey: gen_rand_fiber_public_key(),
            remote_pubkey: gen_rand_fiber_public_key(),
            funding_tx: Some(Transaction::default()),
            funding_tx_confirmed_at: Some((H256::default(), 1, 1)),
            is_acceptor: true,
            is_one_way: false,
            funding_udt_type_script: Some(Script::default()),
            to_local_amount: 100,
            to_remote_amount: 100,
            commitment_fee_rate: 100,
            commitment_delay_epoch: 100,
            funding_fee_rate: 100,
            id: channel_id,
            tlc_state: Default::default(),
            retryable_tlc_operations: Default::default(),
            waiting_forward_tlc_tasks: Default::default(),
            local_shutdown_script: Script::default(),
            local_channel_public_keys: signer.get_base_public_keys(),
            session: crate::fiber::session::new_session(channel_id, &signer.funding_key, None),
            signer: Some(signer),
            remote_channel_public_keys: Some(ChannelBasePublicKeys {
                funding_pubkey: gen_rand_fiber_public_key(),
                tlc_base_key: gen_rand_fiber_public_key(),
            }),
            commitment_numbers: Default::default(),
            remote_shutdown_script: Some(Script::default()),
            remote_commitment_points: vec![
                (0, gen_rand_fiber_public_key()),
                (1, gen_rand_fiber_public_key()),
            ],
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash,
            local_reserved_ckb_amount: 100,
            remote_reserved_ckb_amount: 100,
            local_constraints: ChannelConstraints::default(),
            remote_constraints: ChannelConstraints::default(),
            reestablishing: false,
            connectivity_state: fiber_types::ChannelConnectivityState::Online,
            pending_replay_updates: vec![],
            last_was_revoke: false,
            external_funding: None,
            channel_features: Default::default(),
            created_at: SystemTime::now(),
        },
        waiting_peer_response: None,
        reestablish_timer: None,
        recovery_peer_v2: None,
        network: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        defer_peer_tlc_updates: false,
        deferred_peer_tlc_updates: Default::default(),
        ephemeral_config: Default::default(),
        funding_abort_detail: None,
        private_key: None,
        needs_backup: false,
        pending_messages: Default::default(),
        musig2_agg_context_local_first: Default::default(),
        musig2_agg_context_remote_first: Default::default(),
        pending_remove_effects_v2: 0,
        pending_payment_notifications: Default::default(),
        replayed_tlc_updates_v2: 0,
        resigning_outgoing_v2: false,
    }
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_serde_channel_actor_state_ciborium() {
    let state = sample_channel_actor_state(
        ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
        None,
    );

    let mut serialized = Vec::new();
    ciborium::into_writer(&state, &mut serialized).unwrap();
    let _new_channel_state: ChannelActorState =
        ciborium::from_reader(serialized.as_slice()).expect("deserialize to new state");
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_closed_onchain_settlement_state_round_trips() {
    let mut state = sample_channel_actor_state(
        ChannelState::Closed(
            CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
        ),
        Some(H256::default()),
    );
    state.pending_remove_effects_v2 = 1;
    let (store, _dir) = generate_store();

    store.insert_channel_actor_state_for_test(state.clone());

    let restored = store
        .get_channel_actor_state(&state.id)
        .expect("restored closed channel state");
    assert!(matches!(
        restored.state,
        ChannelState::Closed(flags)
            if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
    ));
    assert_eq!(
        restored.shutdown_transaction_hash,
        state.shutdown_transaction_hash
    );
    assert!(restored.waiting_peer_response.is_none());
    assert!(restored.network.is_none());
    assert!(restored.scheduled_channel_update_handle.is_none());
    assert!(restored.pending_notify_settle_tlcs.is_empty());
    assert_eq!(restored.pending_remove_effects_v2, 0);
    assert!(!restored.defer_peer_tlc_updates);
    assert!(restored.deferred_peer_tlc_updates.is_empty());
    assert!(restored.private_key.is_none());
}
#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_payment_session() {
    let (store, _dir) = generate_store();
    let payment_hash = gen_rand_sha256_hash();
    let payment_data = SendPaymentDataBuilder::new(gen_rand_fiber_public_key(), 100, payment_hash)
        .final_tlc_expiry_delta(DEFAULT_TLC_EXPIRY_DELTA)
        .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
        .timeout(Some(10))
        .max_fee_amount(Some(1000))
        .build()
        .expect("valid payment_data");
    let payment_session = PaymentSession::new_session(&store, payment_data.clone(), 10);
    store.insert_payment_session(payment_session.clone());
    let res = store.get_payment_session(payment_hash).unwrap();
    assert_eq!(res.payment_hash(), payment_hash);
    assert_eq!(res.request.max_fee_amount, Some(1000));
    assert_eq!(res.status, PaymentStatus::Created);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_hosted_payment_execution_store_isolation() {
    use crate::fiber_types::UpstreamTlcKey;
    let (mut store, _dir) = generate_store();
    let events = std::sync::Arc::new(StoreChangeSaver::default());
    let watcher = events.clone();
    store.set_watcher(std::sync::Arc::new(move |event| {
        watcher.changes.write().unwrap().push(event);
    }));
    let hash = gen_rand_sha256_hash();
    let source = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let data = SendPaymentDataBuilder::new(target, 100, hash)
        .final_tlc_expiry_delta(DEFAULT_TLC_EXPIRY_DELTA)
        .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
        .max_fee_amount(Some(1000))
        .build()
        .unwrap();
    let incoming_channel = gen_rand_sha256_hash();
    let outpoint = OutPoint::new(gen_rand_sha256_hash().into(), 0);
    let hops = vec![PaymentHopData {
        amount: 100,
        expiry: u64::MAX,
        next_hop: Some(target),
        funding_tx_hash: outpoint.tx_hash().into(),
        payment_preimage: None,
        hash_algorithm: HashAlgorithm::CkbHash,
        custom_records: None,
    }];
    let mut keys = Vec::new();
    for upstream_tlc_key in [
        None,
        Some(UpstreamTlcKey {
            incoming_channel_id: incoming_channel,
            incoming_tlc_id: 1,
        }),
        Some(UpstreamTlcKey {
            incoming_channel_id: incoming_channel,
            incoming_tlc_id: 2,
        }),
    ] {
        let mut request = data.clone();
        request.upstream_tlc_key = upstream_tlc_key;
        let session = PaymentSession::new_session(&store, request, 10);
        let key = session.session_key();
        assert_eq!(session.attempts_count(), 0);
        store.insert_payment_session(session.clone());
        let mut attempt = session.new_attempt(1, source, target, hops.clone());
        attempt.status = AttemptStatus::Retrying;
        store.insert_attempt(attempt);
        keys.push(key);
    }
    for key in &keys {
        let restored = store.get_payment_session(*key).unwrap();
        assert_eq!(restored.session_key(), *key);
        assert_eq!(restored.attempts_count(), 1);
        assert_eq!(restored.attempts().next().unwrap().session_key(), *key);
    }
    assert_eq!(store.get_all_payment_sessions().len(), 3);
    assert_eq!(
        events.changes.read().unwrap().len(),
        2,
        "only the ordinary session and attempt may notify hash-addressed subscribers"
    );
    assert_eq!(
        store.get_payment_sessions_with_limit(10, None, None).len(),
        1
    );
    assert_eq!(
        store
            .get_pending_attempts_by_channel_outpoint(&outpoint)
            .len(),
        3
    );
    // Recovery indices must remain until the execution actually finishes.
    store.clear_attempts_channel_index(hash);
    assert_eq!(
        store
            .get_pending_attempts_by_channel_outpoint(&outpoint)
            .len(),
        3
    );
    let finish = |key| {
        let mut attempt = store.get_attempt(key, 1).unwrap();
        attempt.set_failed_status("finished", false);
        store.insert_attempt(attempt);
        let mut session = store.get_payment_session(key).unwrap();
        session.set_failed_status("finished");
        store.insert_payment_session(session);
    };
    finish(hash.into());
    // Clearing a completed hash-only index must preserve hosted executions and
    // all attempt records. Repeated cleanup must be harmless.
    store.clear_attempts_channel_index(hash);
    store.clear_attempts_channel_index(hash);
    assert!(store.get_attempt(hash, 1).is_some());
    let pending = store.get_pending_attempts_by_channel_outpoint(&outpoint);
    assert_eq!(pending.len(), 2);
    assert!(pending
        .iter()
        .any(|attempt| attempt.session_key() == keys[1]));
    assert!(pending
        .iter()
        .any(|attempt| attempt.session_key() == keys[2]));

    finish(keys[1]);
    store.clear_attempts_channel_index(keys[1]);
    store.clear_attempts_channel_index(keys[1]);
    assert_eq!(
        store
            .get_pending_attempts_by_channel_outpoint(&outpoint)
            .len(),
        1
    );
    assert!(store.get_attempt(keys[1], 1).is_some());
    // In particular, deleting hash-only attempts must not delete scoped records
    // that share the same byte prefix.
    store.delete_attempts(hash);
    store.delete_attempts(hash);
    assert!(store.get_attempt(hash, 1).is_none());
    assert!(store.get_attempt(keys[1], 1).is_some());
    assert!(store.get_attempt(keys[2], 1).is_some());
    assert_eq!(
        store
            .get_pending_attempts_by_channel_outpoint(&outpoint)
            .len(),
        1
    );
    store.delete_attempts(keys[2]);
    store.delete_attempts(keys[2]);
    assert!(store.get_attempt(keys[2], 1).is_none());
    assert!(store.get_attempt(keys[1], 1).is_some());
    assert!(store
        .get_pending_attempts_by_channel_outpoint(&outpoint)
        .is_empty());
    // Removing attempts must also work after their indexes were already cleared.
    store.delete_attempts(keys[1]);
    store.delete_attempts(keys[1]);
    for key in keys {
        assert!(store.get_attempts(key).is_empty());
        store.clear_attempts_channel_index(key);
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_channel_ready_retry_index_handles_created_attempt_channel_ownership() {
    let (store, _dir) = generate_store();
    let payment_hash = gen_rand_sha256_hash();
    let mut channel_state = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let first_hop_outpoint = channel_state.must_get_funding_transaction_outpoint();
    let first_hop_funding_tx_hash = first_hop_outpoint.tx_hash().into();
    let target = gen_rand_fiber_public_key();
    let first_hop = gen_rand_fiber_public_key();
    let _payment_data = SendPaymentDataBuilder::new(target, 100, payment_hash)
        .router(vec![RouterHop {
            target: first_hop,
            channel_outpoint: first_hop_outpoint.clone(),
            amount_received: 100,
            incoming_tlc_expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
        }])
        .final_tlc_expiry_delta(DEFAULT_TLC_EXPIRY_DELTA)
        .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
        .timeout(Some(10))
        .max_fee_amount(Some(1000))
        .build()
        .expect("valid payment_data");
    let now = now_timestamp_as_millis_u64();
    let route_hops = vec![PaymentHopData {
        amount: 100,
        expiry: now + DEFAULT_TLC_EXPIRY_DELTA,
        payment_preimage: None,
        hash_algorithm: HashAlgorithm::CkbHash,
        funding_tx_hash: first_hop_funding_tx_hash,
        next_hop: Some(target),
        custom_records: None,
    }];
    let route = SessionRoute::new(first_hop, target, &route_hops);
    let mut attempt = Attempt {
        upstream_tlc_key: None,
        id: 1,
        hash: payment_hash,
        try_limit: 10,
        tried_times: 1,
        payment_hash,
        route,
        route_hops,
        session_key: [0; 32],
        preimage: None,
        created_at: now,
        last_updated_at: now,
        last_error: None,
        status: AttemptStatus::Created,
    };

    assert_eq!(attempt.status, AttemptStatus::Created);
    store.insert_attempt(attempt.clone());
    let orphan_created_attempts =
        store.get_pending_attempts_by_channel_outpoint(&first_hop_outpoint);
    assert_eq!(orphan_created_attempts.len(), 1);
    assert_eq!(orphan_created_attempts[0].id, attempt.id);
    assert_eq!(orphan_created_attempts[0].status, AttemptStatus::Created);

    channel_state.tlc_state.add_offered_tlc(TlcInfo {
        upstream_tlc_key: None,
        status: TlcStatus::Outbound(OutboundTlcStatus::LocalAnnounced),
        tlc_id: TLCId::Offered(0),
        amount: 100,
        payment_hash,
        total_amount: None,
        payment_secret: None,
        attempt_id: Some(attempt.id),
        expiry: now + DEFAULT_TLC_EXPIRY_DELTA,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: [0u8; 32],
        is_trampoline_hop: false,
        created_at: CommitmentNumbers::new(),
        removed_reason: None,
        forwarding_tlc: None,
        removed_confirmed_at: None,
        applied_flags: AppliedFlags::empty(),
    });
    let scoped_key = crate::fiber_types::PaymentSessionKey {
        payment_hash,
        upstream_tlc_key: Some(crate::fiber_types::UpstreamTlcKey {
            incoming_channel_id: gen_rand_sha256_hash(),
            incoming_tlc_id: 1,
        }),
    };
    let mut scoped_attempt = attempt.clone();
    scoped_attempt.upstream_tlc_key = scoped_key.upstream_tlc_key;
    assert!(channel_state.owns_payment_attempt(&attempt));
    assert!(!channel_state.owns_payment_attempt(&scoped_attempt));
    channel_state.tlc_state.offered_tlcs.tlcs[0].upstream_tlc_key = scoped_key.upstream_tlc_key;
    assert_eq!(
        channel_state.tlc_state.offered_tlcs.tlcs[0].session_key(),
        scoped_key
    );
    assert!(channel_state.owns_payment_attempt(&scoped_attempt));
    assert!(!channel_state.owns_payment_attempt(&attempt));
    channel_state.tlc_state.offered_tlcs.tlcs[0].upstream_tlc_key = None;
    store.insert_channel_actor_state_for_test(channel_state.clone());
    assert!(
        store
            .get_pending_attempts_by_channel_outpoint(&first_hop_outpoint)
            .is_empty(),
        "created attempts already owned by channel TLCs should not be woken"
    );

    let terminal_tlc = channel_state
        .tlc_state
        .offered_tlcs
        .tlcs
        .first_mut()
        .expect("offered TLC exists");
    terminal_tlc.status = TlcStatus::Outbound(OutboundTlcStatus::RemoveAckConfirmed);
    terminal_tlc.applied_flags |= AppliedFlags::REMOVE;
    terminal_tlc.removed_confirmed_at = Some(1);
    store.insert_channel_actor_state_for_test(channel_state.clone());
    let orphan_created_attempts =
        store.get_pending_attempts_by_channel_outpoint(&first_hop_outpoint);
    assert_eq!(
        orphan_created_attempts.len(),
        1,
        "created attempts with terminal channel TLCs should be woken"
    );
    assert_eq!(orphan_created_attempts[0].id, attempt.id);

    channel_state.tlc_state = Default::default();
    channel_state
        .retryable_tlc_operations
        .push_back(RetryableTlcOperation::AddTlc(AddTlcCommand {
            upstream_tlc_key: None,
            amount: 100,
            payment_hash,
            attempt_id: Some(attempt.id),
            expiry: now + DEFAULT_TLC_EXPIRY_DELTA,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0u8; 32],
            is_trampoline_hop: false,
            previous_tlc: None,
        }));
    if let Some(RetryableTlcOperation::AddTlc(command)) =
        channel_state.retryable_tlc_operations.front_mut()
    {
        assert_eq!(command.session_key(), payment_hash.into());
        command.upstream_tlc_key = scoped_key.upstream_tlc_key;
        assert_eq!(command.session_key(), scoped_key);
    } else {
        panic!("expected queued AddTlc");
    }
    assert!(channel_state.owns_payment_attempt(&scoped_attempt));
    assert!(!channel_state.owns_payment_attempt(&attempt));
    if let Some(RetryableTlcOperation::AddTlc(command)) =
        channel_state.retryable_tlc_operations.front_mut()
    {
        command.upstream_tlc_key = None;
    }
    store.insert_channel_actor_state_for_test(channel_state);
    assert!(
        store
            .get_pending_attempts_by_channel_outpoint(&first_hop_outpoint)
            .is_empty(),
        "created attempts already owned by queued channel AddTlc should not be woken"
    );

    attempt.status = AttemptStatus::Retrying;
    store.insert_attempt(attempt.clone());
    let retrying_attempts = store.get_pending_attempts_by_channel_outpoint(&first_hop_outpoint);
    assert_eq!(retrying_attempts.len(), 1);
    assert_eq!(retrying_attempts[0].id, attempt.id);
    assert_eq!(retrying_attempts[0].status, AttemptStatus::Retrying);
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_payment_sessions_with_status() {
    let (store, _dir) = generate_store();
    let payment_hash0 = gen_rand_sha256_hash();
    let payment_data = SendPaymentDataBuilder::new(gen_rand_fiber_public_key(), 100, payment_hash0)
        .final_tlc_expiry_delta(DEFAULT_TLC_EXPIRY_DELTA)
        .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
        .timeout(Some(10))
        .max_fee_amount(Some(1000))
        .build()
        .expect("valid payment_data");
    let payment_session = PaymentSession::new_session(&store, payment_data.clone(), 10);
    store.insert_payment_session(payment_session.clone());

    let payment_hash1 = gen_rand_sha256_hash();
    let payment_data = SendPaymentDataBuilder::new(gen_rand_fiber_public_key(), 100, payment_hash1)
        .final_tlc_expiry_delta(DEFAULT_TLC_EXPIRY_DELTA)
        .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
        .timeout(Some(10))
        .max_fee_amount(Some(1000))
        .build()
        .expect("valid payment_data");
    let mut payment_session = PaymentSession::new_session(&store, payment_data.clone(), 10);
    payment_session.set_success_status();
    store.insert_payment_session(payment_session.clone());

    let res = store.get_payment_sessions_with_status(PaymentStatus::Created);
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].payment_hash(), payment_hash0);

    let res = store.get_payment_sessions_with_status(PaymentStatus::Success);
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].payment_hash(), payment_hash1);
    assert_eq!(res[0].status, PaymentStatus::Success);

    let res = store.get_payment_sessions_with_status(PaymentStatus::Failed);
    assert_eq!(res.len(), 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_review_rerouted_attempt_index_recovery() {
    use crate::store::store_trait::FiberStore;
    use fiber_types::schema::{
        ATTEMPT_CHANNEL_INDEX_PREFIX, ATTEMPT_PREFIX, PAYMENT_SESSION_PREFIX,
    };
    let (store, _dir) = generate_store();
    let hash = gen_rand_sha256_hash();
    let request = SendPaymentDataBuilder::new(gen_rand_fiber_public_key(), 100, hash)
        .max_fee_amount(Some(1000))
        .build()
        .unwrap();
    let mut session = PaymentSession::new_session(&store, request, 10);
    let a = ckb_types::packed::OutPoint::new(gen_rand_sha256_hash().into(), 0);
    let b = ckb_types::packed::OutPoint::new(gen_rand_sha256_hash().into(), 0);
    let mut attempt = session.new_attempt(
        1,
        gen_rand_fiber_public_key(),
        gen_rand_fiber_public_key(),
        vec![PaymentHopData {
            amount: 100,
            funding_tx_hash: a.tx_hash().into(),
            ..Default::default()
        }],
    );
    let key = |outpoint: &ckb_types::packed::OutPoint| {
        [
            &[ATTEMPT_CHANNEL_INDEX_PREFIX],
            outpoint.as_slice(),
            hash.as_ref(),
            &1u64.to_le_bytes(),
        ]
        .concat()
    };
    store.insert_payment_session(session.clone());
    store.insert_attempt(attempt.clone());
    attempt.status = AttemptStatus::Retrying;
    let mut hops = attempt.route_hops.clone();
    hops[0].funding_tx_hash = b.tx_hash().into();
    attempt.update_route(hops);
    store.insert_attempt(attempt.clone());
    let current = attempt.first_hop_channel_outpoint().unwrap();
    assert!(store.get(key(&a)).is_none());
    assert!(store.get(key(current)).is_some());
    assert!(store
        .get_pending_attempts_by_channel_outpoint(&a)
        .is_empty());
    assert_eq!(
        store
            .get_pending_attempts_by_channel_outpoint(current)
            .len(),
        1
    );

    // Restore the released layout: the attempt points at B, but only A is indexed.
    store.delete(key(current));
    store.put(key(&a), []);
    // Missing attempt/session records leave removable orphan keys, including
    // enough aliases to cross a bounded repair batch.
    for id in 2u64..=300 {
        store.put(
            [
                &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                a.as_slice(),
                hash.as_ref(),
                &id.to_le_bytes(),
            ]
            .concat(),
            [],
        );
    }
    let orphan = gen_rand_sha256_hash();
    store.put(
        [
            &[ATTEMPT_CHANNEL_INDEX_PREFIX],
            a.as_slice(),
            orphan.as_ref(),
            &1u64.to_le_bytes(),
        ]
        .concat(),
        [],
    );
    let attempt_key = [&[ATTEMPT_PREFIX], hash.as_ref(), &1u64.to_le_bytes()].concat();
    let session_key = [&[PAYMENT_SESSION_PREFIX], hash.as_ref()].concat();
    let records_before = (store.get(&attempt_key), store.get(&session_key));
    // Unindexed history must never be deserialized, even during repair.
    let history = gen_rand_sha256_hash();
    store.put([&[PAYMENT_SESSION_PREFIX], history.as_ref()].concat(), []);
    store.put(
        [&[ATTEMPT_PREFIX], history.as_ref(), &1u64.to_le_bytes()].concat(),
        [],
    );
    assert_eq!(
        store.reconcile_attempts_channel_index(),
        [hash.into()].into_iter().collect()
    );
    assert!(store.get(key(&a)).is_none());
    assert!(store.get(key(current)).is_some());
    assert_eq!(
        (store.get(&attempt_key), store.get(&session_key)),
        records_before
    );
    assert_eq!(
        store
            .collect_by_prefix(&[ATTEMPT_CHANNEL_INDEX_PREFIX])
            .len(),
        1
    );

    assert_eq!(
        store
            .get_pending_attempts_by_channel_outpoint(current)
            .len(),
        1
    );

    // A final local result cannot retire a still-owned queued Add.
    let mut channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let channel_outpoint = channel.must_get_funding_transaction_outpoint();
    let mut hops = attempt.route_hops.clone();
    hops[0].funding_tx_hash = channel_outpoint.tx_hash().into();
    attempt.update_route(hops);
    attempt.set_failed_status("stopped", false);
    store.insert_attempt(attempt.clone());
    session.set_failed_status("stopped");
    store.insert_payment_session(session);
    channel
        .retryable_tlc_operations
        .push_back(RetryableTlcOperation::AddTlc(AddTlcCommand {
            amount: 100,
            payment_hash: hash,
            attempt_id: Some(1),
            expiry: 0,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0; 32],
            is_trampoline_hop: false,
            previous_tlc: None,
            upstream_tlc_key: None,
        }));
    store.insert_channel_actor_state_for_test(channel.clone());
    store.delete(key(&channel_outpoint));
    store.put(key(&a), []);
    store.reconcile_attempts_channel_index();
    assert!(store.get(key(&channel_outpoint)).is_some());
    channel.retryable_tlc_operations.clear();
    store.insert_channel_actor_state_for_test(channel);
    // Include an additional historical alias: cleanup must delete actual keys.
    store.put(key(&a), []);
    store.reconcile_attempts_channel_index();
    assert!(store.get_payment_hashes_with_channel_index().is_empty());
    // Same-route result updates must not resurrect retired indexes.
    store.insert_attempt(attempt);
    assert!(store.get_payment_hashes_with_channel_index().is_empty());
    assert!(store.reconcile_attempts_channel_index().is_empty());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_payment_recovery_hashes_read_only_channel_index_keys() {
    use fiber_types::schema::{
        ATTEMPT_CHANNEL_INDEX_PREFIX, ATTEMPT_PREFIX, PAYMENT_SESSION_PREFIX,
    };
    use std::collections::HashSet;

    let (store, _dir) = generate_store();
    let hashes = [gen_rand_sha256_hash(), gen_rand_sha256_hash()];
    for hash in hashes {
        // Neither record is needed to enumerate recovery candidates.
        store.put([&[PAYMENT_SESSION_PREFIX], hash.as_ref()].concat(), []);
        for id in 1u64..=2 {
            let outpoint = gen_rand_channel_outpoint();
            store.put(
                [
                    &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                    outpoint.as_slice(),
                    hash.as_ref(),
                    &id.to_le_bytes(),
                ]
                .concat(),
                [],
            );
            store.put(
                [&[ATTEMPT_PREFIX], hash.as_ref(), &id.to_le_bytes()].concat(),
                [],
            );
        }
    }
    // Historical records without wake-up entries must not become candidates.
    let history = gen_rand_sha256_hash();
    store.put([&[PAYMENT_SESSION_PREFIX], history.as_ref()].concat(), []);
    assert_eq!(
        store.get_payment_hashes_with_channel_index(),
        HashSet::from(hashes)
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_payment_status_queries_skip_attempts_for_mismatched_terminal_sessions() {
    for terminal_status in [PaymentStatus::Success, PaymentStatus::Failed] {
        let (store, _dir) = generate_store();
        let hash = Hash256::from([1; 32]);
        let request = SendPaymentDataBuilder::new(gen_rand_fiber_public_key(), 100, hash)
            .max_fee_amount(Some(1000))
            .build()
            .unwrap();
        let mut session = PaymentSession::new_session(&store, request, 1);
        session.status = terminal_status;
        store.insert_payment_session(session);

        // An unreadable sentinel makes unnecessary attempt hydration fail.
        let attempt_key = [
            &[fiber_types::schema::ATTEMPT_PREFIX],
            hash.as_ref(),
            &1u64.to_le_bytes(),
        ]
        .concat();
        store.put(attempt_key, []);

        for status in [
            PaymentStatus::Created,
            PaymentStatus::Inflight,
            PaymentStatus::Success,
            PaymentStatus::Failed,
        ] {
            if status == terminal_status
                || (terminal_status == PaymentStatus::Failed
                    && matches!(status, PaymentStatus::Success | PaymentStatus::Inflight))
            {
                continue;
            }
            assert!(store.get_payment_sessions_with_status(status).is_empty());
            assert!(store
                .get_payment_sessions_with_limit(1, None, Some(status))
                .is_empty());
        }
    }
}

#[test]
fn test_established_channel_rewrites_only_the_mutable_record() {
    use fiber_types::schema::{CHANNEL_ACTOR_STATE_PREFIX, CHANNEL_IMMUTABLE_STATE_PREFIX};
    let (store, _dir) = generate_store();
    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    store.insert_channel_actor_state_for_test(channel.clone());

    let immutable_key = [&[CHANNEL_IMMUTABLE_STATE_PREFIX], channel.id.as_ref()].concat();
    let mutable_key = [&[CHANNEL_ACTOR_STATE_PREFIX], channel.id.as_ref()].concat();
    let immutable = store.get(immutable_key.clone()).expect("immutable record");
    let mutable = store.get(mutable_key.clone()).expect("mutable record");

    // A message-time change must not touch the immutable record.
    let mut updated = channel.clone();
    updated.to_local_amount += 1;
    store.insert_channel_actor_state(updated.clone());
    assert_eq!(store.get(immutable_key).unwrap(), immutable);
    assert_ne!(store.get(mutable_key).unwrap(), mutable);
    let restored = store.get_channel_actor_state(&channel.id).unwrap();
    assert_eq!(restored.to_local_amount, updated.to_local_amount);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "immutable fields changed")]
fn test_established_channel_rejects_late_immutable_change() {
    let (store, _dir) = generate_store();
    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    store.insert_channel_actor_state_for_test(channel.clone());
    let mut changed = channel;
    changed.commitment_fee_rate += 1;
    // The normal write path must reject a late change to a fixed field.
    store.insert_channel_actor_state(changed);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_reads_never_see_half_a_channel() {
    use crate::store::open_store_with_migration;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    // No cache, so every read goes through the two records.
    let dir = TempDir::new("channel-half-record-reads");
    let store =
        open_store_with_migration(dir.as_ref(), false, 0, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store without a channel cache");

    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let id = channel.id;
    store.insert_channel_actor_state_for_test(channel.clone());

    let done = Arc::new(AtomicBool::new(false));
    // Both threads start together, and the writer only finishes once the reader has read the
    // channel: a writer that finished first would leave the reader with nothing to check.
    let start = Arc::new(Barrier::new(2));
    let first_read = Arc::new(Barrier::new(2));
    let writer = {
        let store = store.clone();
        let channel = channel.clone();
        let done = done.clone();
        let start = start.clone();
        let first_read = first_read.clone();
        std::thread::spawn(move || {
            start.wait();
            for _ in 0..300 {
                store.insert_channel_actor_state_for_test(channel.clone());
                store.delete_channel_actor_state(&id);
            }
            first_read.wait();
            done.store(true, Ordering::SeqCst);
        })
    };
    let reader = {
        let store = store.clone();
        let done = done.clone();
        let start = start.clone();
        let first_read = first_read.clone();
        std::thread::spawn(move || {
            start.wait();
            // Both records are deleted in one batch, so a read must never observe one of them
            // without the other (which would panic). The first read is synchronized with the
            // writer; the rest overlap with it as far as the scheduling allows.
            let _ = store.get_channel_actor_state(&id);
            first_read.wait();
            let mut reads = 0u32;
            while !done.load(Ordering::SeqCst) {
                let _ = store.get_channel_actor_state(&id);
                reads += 1;
            }
            reads
        })
    };
    writer.join().expect("writer");
    reader.join().expect("reader");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_check_validate_requires_both_channel_records() {
    use crate::store::open_store_with_migration;
    use crate::store::store_impl::{channel_immutable_key, channel_mutable_key};
    use fiber_store::backend::StorageBackend;

    let seeded = |name: &str| {
        let dir = TempDir::new(name);
        let channel = valid_opening_channel();
        {
            let store = open_store_with_migration(
                dir.as_ref(),
                false,
                8,
                Box::new(|_| true),
                Box::new(|_| {}),
            )
            .expect("open store with cache");
            store.insert_channel_actor_state_for_test(channel.clone());
        }
        (dir, channel)
    };

    let (dir, channel) = seeded("validate-split-channel");
    crate::store::check_validate(dir.as_ref()).expect("a split channel validates");

    // A channel whose fixed record is gone must be reported, not discovered on its first read.
    {
        let store = fiber_store::Store::open_db(dir.as_ref()).unwrap();
        store.delete(channel_immutable_key(&channel.id));
    }
    let error = crate::store::check_validate(dir.as_ref()).unwrap_err();
    assert!(error.contains("without its fixed record"), "{error}");

    // The other direction: a fixed record nobody can read a channel from.
    let (dir, channel) = seeded("validate-orphan-fixed-record");
    {
        let store = fiber_store::Store::open_db(dir.as_ref()).unwrap();
        store.delete(channel_mutable_key(&channel.id));
    }
    let error = crate::store::check_validate(dir.as_ref()).unwrap_err();
    assert!(
        error.contains("fixed record without its mutable record"),
        "{error}"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_open_channel_phase_updates_the_immutable_record() {
    use fiber_types::schema::CHANNEL_IMMUTABLE_STATE_PREFIX;
    let (store, _dir) = generate_store();
    let mut channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    channel.state = ChannelState::AwaitingChannelReady(AwaitingChannelReadyFlags::empty());
    store.insert_channel_actor_state(channel.clone());
    let immutable_key = [&[CHANNEL_IMMUTABLE_STATE_PREFIX], channel.id.as_ref()].concat();
    let opening = store.get(immutable_key.clone()).expect("immutable record");
    // Still opening: the immutable record is refreshed with the new static value.
    channel.commitment_fee_rate += 1;
    store.insert_channel_actor_state(channel.clone());
    assert_ne!(store.get(immutable_key.clone()).unwrap(), opening);
    let restored = store.get_channel_actor_state(&channel.id).unwrap();
    assert_eq!(restored.commitment_fee_rate, channel.commitment_fee_rate);
}

/// An opening channel whose static keys agree with its signing session, so `validate_channel_v2`
/// accepts it and the split-record validation can be exercised on it.
#[cfg(not(target_arch = "wasm32"))]
fn valid_opening_channel() -> ChannelActorState {
    let mut channel = sample_channel_actor_state(
        ChannelState::NegotiatingFunding(fiber_types::NegotiatingFundingFlags::empty()),
        None,
    );
    channel.local_channel_public_keys = channel.signer.as_ref().unwrap().get_base_public_keys();
    channel
}

/// Seed an established channel: the fixtures fabricate a ready state, which the normal write path
/// cannot create because the fixed record was never written for it.
#[cfg(not(target_arch = "wasm32"))]
fn seed_established(store: &crate::store::Store, channel: &ChannelActorState) {
    store.insert_channel_actor_state_for_test(channel.clone());
}

/// Write both records of a channel directly, without going through the store's write path (so the
/// cache is not involved), as a record left behind by another process would look.
#[cfg(not(target_arch = "wasm32"))]
fn seed_records(store: &crate::store::Store, channel: &ChannelActorState) {
    use crate::store::store_impl::{channel_immutable_key, channel_mutable_key};
    let (immutable, mutable) = channel.core.clone().into_parts();
    store.put(
        channel_immutable_key(&channel.id),
        bincode::serialize(&immutable).unwrap(),
    );
    store.put(
        channel_mutable_key(&channel.id),
        bincode::serialize(&mutable).unwrap(),
    );
}

/// Replace the mutable record behind the store's back.
#[cfg(not(target_arch = "wasm32"))]
fn replace_mutable_record(store: &crate::store::Store, channel: &ChannelActorState) {
    let (_, mutable) = channel.core.clone().into_parts();
    store.put(
        crate::store::store_impl::channel_mutable_key(&channel.id),
        bincode::serialize(&mutable).unwrap(),
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_serves_snapshots_and_tracks_changes() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-tracking");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    seed_established(&store, &channel);
    let mut rewritten = channel.clone();
    rewritten.to_local_amount = 12345;

    // Reads come from the published snapshot, so a record rewritten behind the store's back
    // cannot leak in.
    replace_mutable_record(&store, &rewritten);
    let served = store.get_channel_actor_state(&channel.id).expect("cached");
    assert_eq!(served.to_local_amount, channel.to_local_amount);

    // Runtime-only fields never reach the snapshot: a cached read matches a decoded read.
    let mut with_runtime_state = channel.clone();
    with_runtime_state.waiting_peer_response = Some(42);
    with_runtime_state.defer_peer_tlc_updates = true;
    store.insert_channel_actor_state(with_runtime_state);
    let served = store.get_channel_actor_state(&channel.id).expect("cached");
    assert_eq!(served.waiting_peer_response, None);
    assert!(!served.defer_peer_tlc_updates);

    // A write replaces the published snapshot.
    let mut updated = channel.clone();
    updated.to_local_amount = 777;
    store.insert_channel_actor_state(updated.clone());
    let served = store.get_channel_actor_state(&channel.id).expect("updated");
    assert_eq!(served.to_local_amount, 777);

    // A channel only moves to another id while it is opening, and opening channels are not
    // cached, so the move is a plain record rewrite.
    let mut opening = sample_channel_actor_state(
        ChannelState::NegotiatingFunding(fiber_types::NegotiatingFundingFlags::empty()),
        None,
    );
    store.insert_channel_actor_state(opening.clone());
    let old_id = opening.id;
    let new_id = gen_rand_sha256_hash();
    opening.id = new_id;
    store.move_channel_actor_state(&old_id, opening.clone());
    assert!(store.get_channel_actor_state(&old_id).is_none());
    assert_eq!(
        store.get_channel_actor_state(&new_id).expect("moved").id,
        new_id
    );
    // A move onto the same id is just an insert.
    store.move_channel_actor_state(&new_id, opening.clone());
    assert_eq!(
        store.get_channel_actor_state(&new_id).expect("same id").id,
        new_id
    );

    // Deleting a cached channel drops its snapshot too, so it is gone for later reads and its
    // slot does not stay behind.
    assert!(store.channel_cache_peek(&channel.id).is_some());
    store.delete_channel_actor_state(&channel.id);
    assert!(store.channel_cache_peek(&channel.id).is_none());
    assert_eq!(store.channel_cache_len(), 0);
    assert!(store.get_channel_actor_state(&channel.id).is_none());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_moving_a_cached_channel_drops_its_snapshot() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-move");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    // A published snapshot under the old id must not survive the move, or a reader would be served
    // a state under an id that no longer names a channel.
    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let old_id = channel.id;
    seed_established(&store, &channel);
    assert!(store.channel_cache_peek(&old_id).is_some());

    let mut moved = channel.clone();
    let new_id = gen_rand_sha256_hash();
    moved.id = new_id;
    store.move_channel_actor_state(&old_id, moved);

    assert!(store.channel_cache_peek(&old_id).is_none());
    assert_eq!(store.channel_cache_len(), 0);
    assert!(store.get_channel_actor_state(&old_id).is_none());
    // The rewrite writes both records under the new id, so it is readable even though it is ready.
    assert_eq!(
        store.get_channel_actor_state(&new_id).expect("moved").id,
        new_id
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_clearing_the_channel_cache_drops_every_snapshot() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-clear");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    seed_established(&store, &channel);
    assert!(store.channel_cache_peek(&channel.id).is_some());

    // A restarting node starts from an empty cache and has to decode the records again.
    store.clear_channel_cache();
    assert!(store.channel_cache_peek(&channel.id).is_none());
    assert_eq!(store.channel_cache_len(), 0);
    assert_eq!(
        store
            .get_channel_actor_state(&channel.id)
            .expect("record")
            .to_local_amount,
        channel.to_local_amount
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_established_write_stages_only_the_mutable_record() {
    use crate::store::store_impl::{
        channel_immutable_key, channel_mutable_key, stage_channel_actor_state,
    };
    use fiber_store::backend::BatchWriter;
    use std::sync::{Arc, Mutex};

    // A batch that records which keys a write would touch, so the assertion does not rely on the
    // final bytes (rewriting identical fixed bytes would look the same).
    struct RecordingBatch(Arc<Mutex<Vec<Vec<u8>>>>);
    impl BatchWriter for RecordingBatch {
        fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, _value: V) {
            self.0.lock().unwrap().push(key.as_ref().to_vec());
        }
        fn delete<K: AsRef<[u8]>>(&mut self, key: K) {
            self.0.lock().unwrap().push(key.as_ref().to_vec());
        }
        fn commit(self) {}
    }

    let (store, _dir) = generate_store();
    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    store.insert_channel_actor_state_for_test(channel.clone());

    let keys = Arc::new(Mutex::new(Vec::new()));
    let mut batch = RecordingBatch(keys.clone());
    // Opening writes refresh the fixed record; a message-time write keeps it.
    stage_channel_actor_state(&store, &mut batch, channel.clone(), true);
    assert!(keys
        .lock()
        .unwrap()
        .contains(&channel_immutable_key(&channel.id)));
    keys.lock().unwrap().clear();

    let mut batch = RecordingBatch(keys.clone());
    stage_channel_actor_state(&store, &mut batch, channel.clone(), false);
    let written = keys.lock().unwrap().clone();
    assert!(!written.contains(&channel_immutable_key(&channel.id)));
    assert!(written.contains(&channel_mutable_key(&channel.id)));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_check_validate_reports_half_channels_without_preflight() {
    use crate::store::open_store_with_migration;
    use crate::store::store_impl::{
        channel_immutable_key, channel_mutable_key, validate_stored_channel_record,
    };
    use fiber_store::backend::StorageBackend;
    use std::collections::HashSet;

    let dir = TempDir::new("validate-half-channel");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");
    let channel = valid_opening_channel();
    store.insert_channel_actor_state_for_test(channel.clone());

    let mutable_key = channel_mutable_key(&channel.id);
    let immutable_key = channel_immutable_key(&channel.id);
    let mutable = store.get(&mutable_key).expect("mutable record");
    let immutable = store.get(&immutable_key).expect("fixed record");

    // The migration preflight reports these too, so call the validator's own checks directly: they
    // are what has to survive once a schema bump retires that migration.
    let mut errors = HashSet::new();
    validate_stored_channel_record(&store, &mutable_key, &mutable, true, &mut errors);
    assert!(errors.is_empty(), "{errors:?}");

    // A mutable record without its fixed record.
    store.delete(&immutable_key);
    let mut errors = HashSet::new();
    validate_stored_channel_record(&store, &mutable_key, &mutable, true, &mut errors);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("mutable record without its fixed record")),
        "{errors:?}"
    );

    // And the other direction: a fixed record nobody can read a channel from.
    store.put(&immutable_key, &immutable);
    store.delete(&mutable_key);
    let mut errors = HashSet::new();
    validate_stored_channel_record(&store, &immutable_key, &immutable, true, &mut errors);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("fixed record without its mutable record")),
        "{errors:?}"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_is_only_filled_by_writes() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-reads");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    // A ready record that this process never wrote: reads decode it but must not publish it, so a
    // snapshot can never outlive an invalidation.
    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    seed_records(&store, &channel);
    assert_eq!(
        store
            .get_channel_actor_state(&channel.id)
            .expect("state")
            .to_local_amount,
        channel.to_local_amount
    );
    assert_eq!(store.channel_cache_len(), 0);

    // A write fills it, and a later transition drops it again.
    seed_established(&store, &channel);
    assert_eq!(store.channel_cache_len(), 1);
    let mut settling = channel.clone();
    settling.state = ChannelState::ShuttingDown(fiber_types::ShuttingDownFlags::empty());
    settling.to_local_amount = 999;
    store.insert_channel_actor_state(settling.clone());
    assert_eq!(store.channel_cache_len(), 0);
    assert_eq!(
        store
            .get_channel_actor_state(&channel.id)
            .expect("state")
            .to_local_amount,
        999
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_skips_channels_that_are_not_ready() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-opening");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    // A channel that is still opening can change its id, so it is written but not cached.
    let mut opening = sample_channel_actor_state(
        ChannelState::NegotiatingFunding(fiber_types::NegotiatingFundingFlags::empty()),
        None,
    );
    store.insert_channel_actor_state(opening.clone());
    assert_eq!(store.channel_cache_len(), 0);

    opening.to_local_amount = 4242;
    replace_mutable_record(&store, &opening);
    // Reading through the store decodes the record, so a rewrite is visible.
    assert_eq!(
        store
            .get_channel_actor_state(&opening.id)
            .expect("state")
            .to_local_amount,
        4242
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_disabled_reads_the_database() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-off");
    let store =
        open_store_with_migration(dir.as_ref(), false, 0, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store without cache");

    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    seed_records(&store, &channel);
    let mut rewritten = channel.clone();
    rewritten.to_local_amount = 12345;
    replace_mutable_record(&store, &rewritten);

    // Without a cache every read decodes the stored record.
    assert_eq!(
        store
            .get_channel_actor_state(&channel.id)
            .expect("state")
            .to_local_amount,
        12345
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_stops_caching_when_full_and_frees_slots_on_delete() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-capacity");
    let store =
        open_store_with_migration(dir.as_ref(), false, 2, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with a two-entry cache");

    // The cache holds two channels; the third is written but not cached.
    let mut channels = Vec::new();
    for amount in 1..=3u128 {
        let mut channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
        channel.to_local_amount = amount;
        seed_established(&store, &channel);
        channels.push(channel);
    }
    assert_eq!(store.channel_cache_len(), 2);
    assert!(store.channel_cache_peek(&channels[0].id).is_some());
    assert!(store.channel_cache_peek(&channels[1].id).is_some());
    assert!(store.channel_cache_peek(&channels[2].id).is_none());

    // Every channel is still readable, cached or not.
    for channel in &channels {
        let read = store.get_channel_actor_state(&channel.id).expect("state");
        assert_eq!(read.to_local_amount, channel.to_local_amount);
    }

    // A cached channel keeps tracking writes.
    let mut updated = channels[0].clone();
    updated.to_local_amount = 999;
    store.insert_channel_actor_state(updated.clone());
    assert_eq!(
        store
            .channel_cache_peek(&updated.id)
            .expect("cached")
            .to_local_amount,
        999
    );

    // A channel that was not cached is read straight from the record, so a record replaced
    // behind the store's back is visible.
    let uncached = channels[2].clone();
    let mut rewritten = uncached.clone();
    rewritten.to_local_amount = 11111;
    replace_mutable_record(&store, &rewritten);
    assert_eq!(
        store
            .get_channel_actor_state(&uncached.id)
            .expect("state")
            .to_local_amount,
        11111
    );

    // Deleting a channel frees its slot, so a new channel can be cached again.
    store.delete_channel_actor_state(&channels[1].id);
    assert_eq!(store.channel_cache_len(), 1);
    let mut replacement = sample_channel_actor_state(ChannelState::ChannelReady, None);
    replacement.to_local_amount = 4242;
    seed_established(&store, &replacement);
    assert_eq!(
        store
            .channel_cache_peek(&replacement.id)
            .expect("cached after a delete freed a slot")
            .to_local_amount,
        4242
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_never_holds_a_snapshot_the_record_replaced() {
    use crate::store::open_store_with_migration;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    let dir = TempDir::new("channel-cache-mixed-transitions");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let id = channel.id;
    seed_established(&store, &channel);

    // Threads write the same channel as ready, as shutting down, and as closed: a snapshot that a
    // later transition invalidated must never come back, and one that is served must name the state
    // the record holds. Deleting is not part of the mix: the split layout assumes one owner per
    // channel, so a delete racing a ready write is not an interleaving it supports (deleting a
    // cached channel is covered separately).
    let done = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(5));
    let reader = {
        let store = store.clone();
        let done = done.clone();
        let start = start.clone();
        std::thread::spawn(move || {
            start.wait();
            let mut checks = 0u32;
            while !done.load(Ordering::SeqCst) {
                // The snapshot and the record are read under the channel's slot, so a write
                // cannot change the record between the two reads.
                if let Some((cached, record)) = store.channel_cache_and_record(&id) {
                    let record = record.expect("a published snapshot must have a record");
                    assert_eq!(
                        bincode::serialize(&cached).unwrap(),
                        bincode::serialize(&record).unwrap(),
                        "a reader was served a snapshot that differs from the record"
                    );
                    checks += 1;
                }
            }
            checks
        })
    };

    let writers: Vec<_> = (0..4u128)
        .map(|worker| {
            let store = store.clone();
            let channel = channel.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                for round in 1..=100u128 {
                    let mut state = channel.clone();
                    state.to_local_amount = worker * 1000 + round;
                    match (worker + round) % 3 {
                        0 => store.insert_channel_actor_state_for_test(state),
                        1 => {
                            state.state =
                                ChannelState::ShuttingDown(fiber_types::ShuttingDownFlags::empty());
                            store.insert_channel_actor_state(state)
                        }
                        _ => {
                            state.state =
                                ChannelState::Closed(fiber_types::CloseFlags::COOPERATIVE);
                            store.insert_channel_actor_state(state)
                        }
                    }
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer");
    }
    done.store(true, Ordering::SeqCst);
    let checks = reader.join().expect("reader");
    // Otherwise the assertions above never ran and prove nothing.
    assert!(checks > 0, "the reader never saw a published snapshot");

    // Whatever the interleaving, a cached snapshot must be the stored record.
    assert!(store.channel_cache_len() <= 1);
    if let Some((cached, record)) = store.channel_cache_and_record(&id) {
        let record = record.expect("a cached snapshot must have a stored record");
        assert_eq!(
            bincode::serialize(&cached).unwrap(),
            bincode::serialize(&record).unwrap(),
            "cached snapshot differs from the stored record"
        );
    }

    // Deleting drops the snapshot with the records.
    store.delete_channel_actor_state(&id);
    assert!(store.channel_cache_peek(&id).is_none());
    assert_eq!(store.channel_cache_len(), 0);
    assert!(store.get_channel_actor_state(&id).is_none());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_restore_clears_the_channel_cache() {
    use crate::store::open_store_with_migration;
    use fiber_store::backend::StorageBackend;

    let dir = TempDir::new("channel-cache-restore");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    seed_established(&store, &channel);
    assert_eq!(store.channel_cache_len(), 1);

    // A restore describes the database that is being replaced, so the snapshots go even when the
    // restore itself fails.
    let missing = dir.as_ref().join("missing-backup");
    let db_path = dir.as_ref().join("db");
    assert!(store.restore(&missing, &db_path).is_err());
    assert_eq!(store.channel_cache_len(), 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_capacity_is_not_exceeded_by_concurrent_writes() {
    use crate::store::open_store_with_migration;
    use std::sync::{Arc, Barrier};

    let dir = TempDir::new("channel-cache-capacity-race");
    let store =
        open_store_with_migration(dir.as_ref(), false, 1, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with a one-entry cache");

    // Two channels racing for the single capacity slot: the reservation is atomic, so one wins,
    // and both stay readable.
    let mut first = sample_channel_actor_state(ChannelState::ChannelReady, None);
    first.to_local_amount = 1;
    let mut second = sample_channel_actor_state(ChannelState::ChannelReady, None);
    second.to_local_amount = 2;
    let start = Arc::new(Barrier::new(2));
    let writers: Vec<_> = [first.clone(), second.clone()]
        .into_iter()
        .map(|channel| {
            let store = store.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                store.insert_channel_actor_state_for_test(channel);
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer");
    }

    assert!(
        store.channel_cache_len() <= 1,
        "cache holds {} entries with a capacity of one",
        store.channel_cache_len()
    );
    for channel in [&first, &second] {
        assert_eq!(
            store
                .get_channel_actor_state(&channel.id)
                .expect("state")
                .to_local_amount,
            channel.to_local_amount
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_matches_the_database_under_concurrent_writes() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-writers");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    seed_established(&store, &channel);

    // Several writers race on the same channel. Whatever the interleaving, the published
    // snapshot must be the record the database holds afterwards.
    let writers: Vec<_> = (1..=4u128)
        .map(|writer| {
            let store = store.clone();
            let channel = channel.clone();
            std::thread::spawn(move || {
                for round in 1..=100u128 {
                    let mut state = channel.clone();
                    state.to_local_amount = writer * 1000 + round;
                    store.insert_channel_actor_state(state);
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer");
    }

    // Compare the served snapshot with the records read under the same slot.
    let (cached, record) = store
        .channel_cache_and_record(&channel.id)
        .expect("a cached channel");
    let record = record.expect("a cached snapshot must have a stored record");
    assert_eq!(cached.to_local_amount, record.to_local_amount);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_reads_see_completed_writes() {
    use crate::store::open_store_with_migration;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    let dir = TempDir::new("channel-cache-concurrent");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");

    // Start above the sample's own value: a stale snapshot must not look fresh.
    let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let first_value = channel.to_local_amount as u64 + 1;
    seed_established(&store, &channel);

    let published = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(3));

    let writer = {
        let store = store.clone();
        let channel = channel.clone();
        let published = published.clone();
        let done = done.clone();
        let start = start.clone();
        std::thread::spawn(move || {
            start.wait();
            for value in first_value..first_value + 500 {
                let mut state = channel.clone();
                state.to_local_amount = value as u128;
                store.insert_channel_actor_state(state);
                published.store(value, Ordering::SeqCst);
            }
            done.store(true, Ordering::SeqCst);
        })
    };

    let readers: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let id = channel.id;
            let published = published.clone();
            let done = done.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                let mut reads = 0u64;
                while !done.load(Ordering::SeqCst) {
                    // A write that already returned must be visible to the next read.
                    let expected = published.load(Ordering::SeqCst);
                    let observed = store
                        .get_channel_actor_state(&id)
                        .expect("state")
                        .to_local_amount as u64;
                    assert!(
                        observed >= expected,
                        "read {observed} is older than the completed write {expected}"
                    );
                    reads += 1;
                }
                reads
            })
        })
        .collect();

    writer.join().expect("writer");
    let reads: u64 = readers
        .into_iter()
        .map(|reader| reader.join().expect("reader"))
        .sum();
    // The readers must actually have run, otherwise the assertions above prove nothing.
    assert!(reads > 0, "readers never read a channel state");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_channel_cache_isolates_namespaces_with_same_channel_id() {
    use crate::store::open_store_with_migration;

    let dir = TempDir::new("channel-cache-namespaces");
    let store =
        open_store_with_migration(dir.as_ref(), false, 8, Box::new(|_| true), Box::new(|_| {}))
            .expect("open store with cache");
    let tenant_a = store.namespaced(NodeNamespace::hosted_tenant("alice"));
    let tenant_a_reopened = store.namespaced(NodeNamespace::hosted_tenant("alice"));
    let tenant_b = store.namespaced(NodeNamespace::hosted_tenant("bob"));

    let mut host_channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let shared_id = host_channel.id;
    host_channel.to_local_amount = 100;

    let mut alice_channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    alice_channel.id = shared_id;
    alice_channel.to_local_amount = 200;

    let mut bob_channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    bob_channel.id = shared_id;
    bob_channel.to_local_amount = 300;

    seed_established(&store, &host_channel);
    seed_established(&tenant_a, &alice_channel);
    seed_established(&tenant_b, &bob_channel);

    assert_eq!(store.channel_cache_len(), 1);
    assert_eq!(tenant_a.channel_cache_len(), 1);
    assert_eq!(tenant_b.channel_cache_len(), 1);

    assert_eq!(
        store
            .get_channel_actor_state(&shared_id)
            .unwrap()
            .to_local_amount,
        100
    );
    assert_eq!(
        tenant_a
            .get_channel_actor_state(&shared_id)
            .unwrap()
            .to_local_amount,
        200
    );
    // A second handle for the same namespace shares the cache coherently.
    assert_eq!(
        tenant_a_reopened
            .channel_cache_peek(&shared_id)
            .unwrap()
            .to_local_amount,
        200
    );
    assert_eq!(
        tenant_b
            .get_channel_actor_state(&shared_id)
            .unwrap()
            .to_local_amount,
        300
    );

    // Updating through another handle of the same namespace invalidates/updates its cached snapshot,
    // without disturbing other namespaces sharing the same channel_id.
    alice_channel.to_local_amount = 250;
    tenant_a_reopened.insert_channel_actor_state(alice_channel.clone());
    assert_eq!(
        tenant_a
            .channel_cache_peek(&shared_id)
            .unwrap()
            .to_local_amount,
        250
    );
    assert_eq!(
        store
            .channel_cache_peek(&shared_id)
            .unwrap()
            .to_local_amount,
        100
    );
    assert_eq!(
        tenant_b
            .channel_cache_peek(&shared_id)
            .unwrap()
            .to_local_amount,
        300
    );

    // Deleting in one namespace leaves the other namespaces' records and snapshots intact.
    tenant_a.delete_channel_actor_state(&shared_id);
    assert!(tenant_a.get_channel_actor_state(&shared_id).is_none());
    assert!(tenant_a_reopened
        .get_channel_actor_state(&shared_id)
        .is_none());
    assert_eq!(tenant_a.channel_cache_len(), 0);
    assert_eq!(
        store
            .get_channel_actor_state(&shared_id)
            .unwrap()
            .to_local_amount,
        100
    );
    assert_eq!(
        tenant_b
            .get_channel_actor_state(&shared_id)
            .unwrap()
            .to_local_amount,
        300
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_payment_list_skips_unneeded_channel_reads() {
    use crate::store::open_store_with_migration;
    use fiber_types::schema::{ATTEMPT_CHANNEL_INDEX_PREFIX, CHANNEL_ACTOR_STATE_PREFIX};
    for indexed in [false, true] {
        // No channel cache: the record this test overwrites must be the only source of a channel.
        let dir = TempDir::new("payment-list-no-channel-cache");
        let store =
            open_store_with_migration(dir.as_ref(), false, 0, Box::new(|_| true), Box::new(|_| {}))
                .expect("open store without a channel cache");
        let channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
        let outpoint = channel.must_get_funding_transaction_outpoint();
        store.insert_channel_actor_state_for_test(channel.clone());
        let mut originals = Vec::new();
        for tag in 1u8..=32 {
            let hash = Hash256::from([tag; 32]);
            let request = SendPaymentDataBuilder::new(channel.remote_pubkey, 100, hash)
                .max_fee_amount(Some(1000))
                .build()
                .unwrap();
            let mut session = PaymentSession::new_session(&store, request, 1);
            let mut attempt = session.new_attempt(
                1,
                channel.local_pubkey,
                channel.remote_pubkey,
                vec![PaymentHopData {
                    amount: 100,
                    funding_tx_hash: outpoint.tx_hash().into(),
                    ..Default::default()
                }],
            );
            attempt.set_failed_status("historical failure", false);
            store.insert_attempt(attempt.clone());
            session.set_failed_status("historical failure");
            store.insert_payment_session(session);
            if !indexed {
                store.delete(
                    [
                        &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                        outpoint.as_slice(),
                        hash.as_ref(),
                        &attempt.id.to_le_bytes(),
                    ]
                    .concat(),
                );
            }
            originals.push(attempt);
        }
        // Any channel deserialization now panics: list filtering must not read it.
        store.put(
            [&[CHANNEL_ACTOR_STATE_PREFIX], channel.id.as_ref()].concat(),
            [],
        );
        assert!(store
            .get_payment_sessions_with_status(PaymentStatus::Success)
            .is_empty());
        assert!(store
            .get_payment_sessions_with_limit(10, None, Some(PaymentStatus::Success))
            .is_empty());
        if !indexed {
            assert!(store
                .get_payment_sessions_with_status(PaymentStatus::Inflight)
                .is_empty());
            assert!(store
                .get_payment_sessions_with_limit(10, None, Some(PaymentStatus::Inflight))
                .is_empty());
            for filter in [None, Some(PaymentStatus::Failed)] {
                let page = store.get_payment_sessions_with_limit(3, Some([1; 32].into()), filter);
                assert_eq!(
                    page.iter().map(|s| s.payment_hash()).collect::<Vec<_>>(),
                    vec![[2; 32].into(), [3; 32].into(), [4; 32].into()]
                );
                for session in page {
                    let status = session.status;
                    let response =
                        crate::fiber::network::SendPaymentResponse::from_session_with_status(
                            session, status,
                        );
                    assert_eq!(response.status, PaymentStatus::Failed);
                }
            }
            assert_eq!(
                store
                    .get_payment_sessions_with_status(PaymentStatus::Failed)
                    .len(),
                32
            );
        }
        // Successful attempt evidence must remain visible despite the persisted Failed.
        let mut success = originals[0].clone();
        success.set_success_status();
        store.insert_attempt(success);
        for page in [
            store.get_payment_sessions_with_status(PaymentStatus::Success),
            store.get_payment_sessions_with_limit(1, None, Some(PaymentStatus::Success)),
        ] {
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].payment_hash(), originals[0].payment_hash);
            assert_eq!(page[0].status, PaymentStatus::Success);
        }
        // An explicitly Inflight attempt needs neither an index nor channel reads.
        if !indexed {
            let mut inflight = originals[1].clone();
            inflight.status = AttemptStatus::Inflight;
            store.insert_attempt(inflight);
            let page = store.get_payment_sessions_with_status(PaymentStatus::Inflight);
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].status, PaymentStatus::Inflight);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_payment_list_index_gate_does_not_change_settlement_status() {
    use fiber_types::schema::{
        ATTEMPT_CHANNEL_INDEX_PREFIX, ATTEMPT_PREFIX, PAYMENT_SESSION_PREFIX,
    };
    let (store, _dir) = generate_store();
    let node = NetworkNode::new().await;
    let rpc =
        crate::rpc::payment::PaymentRpcServerImpl::new(node.network_actor.clone(), store.clone());

    let mut channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let outpoint = channel.must_get_funding_transaction_outpoint();
    let hash = gen_rand_sha256_hash();
    let request = SendPaymentDataBuilder::new(channel.remote_pubkey, 100, hash)
        .max_fee_amount(Some(1000))
        .build()
        .unwrap();
    let mut session = PaymentSession::new_session(&store, request, 1);
    let mut attempt = session.new_attempt(
        1,
        channel.local_pubkey,
        channel.remote_pubkey,
        vec![PaymentHopData {
            amount: 100,
            funding_tx_hash: outpoint.tx_hash().into(),
            ..Default::default()
        }],
    );
    attempt.set_failed_status("dispatch stopped", false);
    store.insert_attempt(attempt);
    session.set_failed_status("dispatch stopped");
    store.insert_payment_session(session);
    channel
        .retryable_tlc_operations
        .push_back(RetryableTlcOperation::AddTlc(AddTlcCommand {
            amount: 100,
            payment_hash: hash,
            attempt_id: Some(1),
            expiry: 0,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0; 32],
            is_trampoline_hop: false,
            previous_tlc: None,
            upstream_tlc_key: None,
        }));
    store.insert_channel_actor_state_for_test(channel);
    let session_key = [&[PAYMENT_SESSION_PREFIX], hash.as_ref()].concat();
    let attempt_key = [&[ATTEMPT_PREFIX], hash.as_ref(), &1u64.to_le_bytes()].concat();
    let before = (store.get(&session_key), store.get(&attempt_key));
    for indexed in [true, false] {
        if !indexed {
            store.delete(
                [
                    &[ATTEMPT_CHANNEL_INDEX_PREFIX],
                    outpoint.as_slice(),
                    hash.as_ref(),
                    &1u64.to_le_bytes(),
                ]
                .concat(),
            );
        }
        let expected = if indexed {
            PaymentStatus::Inflight
        } else {
            PaymentStatus::Failed
        };
        let excluded = if indexed {
            PaymentStatus::Failed
        } else {
            PaymentStatus::Inflight
        };
        assert!(store.get_payment_sessions_with_status(excluded).is_empty());
        assert!(store
            .get_payment_sessions_with_limit(1, None, Some(excluded))
            .is_empty());
        for page in [
            store.get_payment_sessions_with_status(expected),
            store.get_payment_sessions_with_limit(1, None, Some(expected)),
            store.get_payment_sessions_with_limit(1, None, None),
        ] {
            assert_eq!(page.len(), 1);
            let view = page.into_iter().next().unwrap();
            assert_eq!(view.status, expected);
            let response = crate::fiber::network::SendPaymentResponse::from_session_with_status(
                view, expected,
            );
            assert_eq!(response.status, expected);
        }
        for filter in [None, Some(expected)] {
            let response = rpc
                .list_payments(
                    &jsonrpsee::Extensions::new(),
                    crate::rpc::payment::ListPaymentsParams {
                        status: filter.map(Into::into),
                        limit: Some(1),
                        after: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(response.payments.len(), 1);
            assert_eq!(response.payments[0].status, expected.into());
            assert_eq!(response.last_cursor, Some(hash.into()));
            let next = rpc
                .list_payments(
                    &jsonrpsee::Extensions::new(),
                    crate::rpc::payment::ListPaymentsParams {
                        status: filter.map(Into::into),
                        limit: Some(1),
                        after: response.last_cursor,
                    },
                )
                .await
                .unwrap();
            assert!(next.payments.is_empty());
        }
        let session = store.get_payment_session(hash).unwrap();
        assert_eq!(session.effective_status(&store), PaymentStatus::Inflight);
        assert_eq!(
            crate::fiber::network::SendPaymentResponse::from_session(session, &store).status,
            PaymentStatus::Inflight
        );
        assert_eq!((store.get(&session_key), store.get(&attempt_key)), before);
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_legacy_partial_mpp_proof_is_success_in_all_payment_queries() {
    let (store, _dir) = generate_store();
    let source = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let preimage = gen_rand_sha256_hash();
    let hash = Hash256::from(HashAlgorithm::CkbHash.hash(preimage));
    let request = SendPaymentDataBuilder::new(target, 100, hash)
        .max_fee_amount(Some(1000))
        .build()
        .unwrap();
    let mut session = PaymentSession::new_session(&store, request, 1);
    let mut attempt = session.new_attempt(
        1,
        source,
        target,
        vec![PaymentHopData {
            amount: 60,
            ..Default::default()
        }],
    );
    attempt.set_success_status();
    attempt.preimage = Some(preimage);
    store.insert_attempt(attempt);
    session.set_failed_status("legacy partial MPP failure");
    store.insert_payment_session(session);
    assert_eq!(
        store.get_payment_session(hash).unwrap().status,
        PaymentStatus::Success
    );
    assert_eq!(
        store
            .get_payment_sessions_with_status(PaymentStatus::Success)
            .len(),
        1
    );
    assert_eq!(
        store
            .get_payment_sessions_with_limit(1, None, Some(PaymentStatus::Success))
            .len(),
        1
    );
    assert!(store
        .get_payment_sessions_with_status(PaymentStatus::Failed)
        .is_empty());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_review_settled_legacy_timeout_is_not_pending() {
    let (store, _dir) = generate_store();
    let preimage = gen_rand_sha256_hash();
    let hash = HashAlgorithm::CkbHash.hash(preimage).into();
    let mut channel = sample_channel_actor_state(ChannelState::ChannelReady, None);
    let outpoint = channel.must_get_funding_transaction_outpoint();
    let request = SendPaymentDataBuilder::new(gen_rand_fiber_public_key(), 100, hash)
        .max_fee_amount(Some(1000))
        .build()
        .unwrap();
    let mut session = PaymentSession::new_session(&store, request, 1);
    let mut attempt = session.new_attempt(
        1,
        channel.local_pubkey,
        channel.remote_pubkey,
        vec![PaymentHopData {
            amount: 100,
            funding_tx_hash: outpoint.tx_hash().into(),
            ..Default::default()
        }],
    );
    attempt.set_failed_status("legacy chain timeout", false);
    store.insert_attempt(attempt.clone());
    session.set_failed_status("legacy chain timeout");
    store.insert_payment_session(session);
    channel.tlc_state.add_offered_tlc(TlcInfo {
        upstream_tlc_key: None,
        status: TlcStatus::Outbound(OutboundTlcStatus::Committed),
        tlc_id: TLCId::Offered(0),
        amount: 100,
        payment_hash: hash,
        total_amount: None,
        payment_secret: None,
        attempt_id: Some(attempt.id),
        expiry: 0,
        hash_algorithm: HashAlgorithm::CkbHash,
        onion_packet: None,
        shared_secret: [0; 32],
        is_trampoline_hop: false,
        created_at: CommitmentNumbers::new(),
        removed_reason: None,
        forwarding_tlc: None,
        removed_confirmed_at: None,
        applied_flags: AppliedFlags::ADD,
    });
    // Released chain-timeout completion used this setter without applying REMOVE.
    channel.tlc_state.set_offered_tlc_removed(
        0,
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new(TlcErrorCode::ExpiryTooSoon),
            &[0; 32],
        )),
    );
    channel.state = ChannelState::Closed(CloseFlags::UNCOOPERATIVE_LOCAL);
    store.insert_channel_actor_state_for_test(channel.clone());
    let session = store.get_payment_session(hash).unwrap();
    let original = bincode::serialize(&channel.core).unwrap();
    assert!(
        !session.has_outstanding_channel_work(&store),
        "a finalized legacy timeout cannot block explicit retries"
    );
    assert_eq!(session.effective_status(&store), PaymentStatus::Failed);
    assert_eq!(
        store
            .get_payment_sessions_with_limit(1, None, Some(PaymentStatus::Failed))
            .len(),
        1
    );
    assert_eq!(
        bincode::serialize(&store.get_channel_actor_state(&channel.id).unwrap().core).unwrap(),
        original
    );

    channel
        .retryable_tlc_operations
        .push_back(RetryableTlcOperation::AddTlc(AddTlcCommand {
            amount: 100,
            payment_hash: hash,
            attempt_id: Some(attempt.id),
            expiry: DEFAULT_TLC_EXPIRY_DELTA,
            hash_algorithm: HashAlgorithm::CkbHash,
            onion_packet: None,
            shared_secret: [0; 32],
            is_trampoline_hop: false,
            previous_tlc: None,
            upstream_tlc_key: None,
        }));
    store.insert_channel_actor_state_for_test(channel.clone());
    assert!(
        session.has_outstanding_channel_work(&store),
        "a pending Add is still owned even when a historical TLC has finished"
    );
    channel.retryable_tlc_operations.clear();

    channel.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    store.insert_channel_actor_state_for_test(channel.clone());
    assert_eq!(
        session.effective_status(&store),
        PaymentStatus::Inflight,
        "a reason alone is not final while settlement is pending"
    );
    channel.state = ChannelState::Closed(CloseFlags::UNCOOPERATIVE_LOCAL);
    channel
        .tlc_state
        .get_mut(&TLCId::Offered(0))
        .unwrap()
        .removed_reason = Some(RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
        payment_preimage: preimage,
    }));
    store.insert_channel_actor_state_for_test(channel.clone());
    assert!(
        session.has_outstanding_channel_work(&store),
        "an unobserved fulfillment must still wake the payer"
    );
    attempt.set_success_status();
    attempt.preimage = Some(preimage);
    store.insert_attempt(attempt.clone());
    let session = store.get_payment_session(hash).unwrap();
    let original = bincode::serialize(&channel.core).unwrap();
    assert!(
        !session.has_pending_payment_work(&store),
        "an acknowledged historical on-chain fulfill must not retain a payer forever"
    );
    assert_eq!(
        bincode::serialize(&store.get_channel_actor_state(&channel.id).unwrap().core).unwrap(),
        original
    );
    channel.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_LOCAL | CloseFlags::WAITING_ONCHAIN_SETTLEMENT,
    );
    store.insert_channel_actor_state_for_test(channel.clone());
    assert!(session.has_pending_payment_work(&store));
    channel.state = ChannelState::Closed(CloseFlags::UNCOOPERATIVE_LOCAL);
    attempt.preimage = Some(gen_rand_sha256_hash());
    store.insert_attempt(attempt);
    store.insert_channel_actor_state_for_test(channel);
    assert!(
        store
            .get_payment_session(hash)
            .unwrap()
            .has_pending_payment_work(&store),
        "a mismatched preimage must not retire historical work"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_publish_payment_result_notifies_without_changing_persisted_stop() {
    use std::sync::{Arc, Mutex};

    let (mut store, _dir) = generate_store();
    let hash = gen_rand_sha256_hash();
    let source = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let request = SendPaymentDataBuilder::new(target, 100, hash)
        .max_fee_amount(Some(1000))
        .build()
        .unwrap();
    let mut session = PaymentSession::new_session(&store, request, 1);
    session.set_failed_status("stop dispatch");
    let mut attempt = session.new_attempt(
        1,
        source,
        target,
        vec![PaymentHopData {
            amount: 100,
            ..Default::default()
        }],
    );
    attempt.status = AttemptStatus::Inflight;
    store.insert_attempt(attempt.clone());
    store.insert_payment_session(session);
    let key = [
        &[fiber_types::schema::PAYMENT_SESSION_PREFIX],
        hash.as_ref(),
    ]
    .concat();
    let persisted = store.get(&key).unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let statuses = observed.clone();
    store.set_watcher(Arc::new(move |change| {
        if let crate::store::store_impl::StoreChange::PutPaymentSession {
            payment_session, ..
        } = change
        {
            statuses.lock().unwrap().push(payment_session.status);
        }
    }));
    store.publish_payment_result(hash);
    attempt.set_failed_status("last issued work completed", false);
    store.insert_attempt(attempt);
    store.publish_payment_result(hash);
    assert_eq!(
        *observed.lock().unwrap(),
        vec![PaymentStatus::Inflight, PaymentStatus::Failed]
    );
    assert_eq!(store.get(key).unwrap(), persisted);
    assert_eq!(
        store.get_persisted_payment_status(hash),
        Some(PaymentStatus::Failed)
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_payment_status_queries_filter_hydrated_attempts_before_pagination() {
    let (store, _dir) = generate_store();
    let source = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let cases = [
        (
            PaymentStatus::Created,
            AttemptStatus::Inflight,
            PaymentStatus::Inflight,
        ),
        (
            PaymentStatus::Inflight,
            AttemptStatus::Created,
            PaymentStatus::Created,
        ),
        (
            PaymentStatus::Created,
            AttemptStatus::Success,
            PaymentStatus::Success,
        ),
        (
            PaymentStatus::Created,
            AttemptStatus::Failed,
            PaymentStatus::Failed,
        ),
        (
            PaymentStatus::Success,
            AttemptStatus::Inflight,
            PaymentStatus::Success,
        ),
        (
            PaymentStatus::Failed,
            AttemptStatus::Inflight,
            PaymentStatus::Inflight,
        ),
        (
            PaymentStatus::Created,
            AttemptStatus::Inflight,
            PaymentStatus::Inflight,
        ),
        (
            PaymentStatus::Created,
            AttemptStatus::Inflight,
            PaymentStatus::Inflight,
        ),
    ];
    for (index, &(persisted, attempt_status, effective)) in cases.iter().enumerate() {
        let hash = Hash256::from([index as u8 + 1; 32]);
        let request = SendPaymentDataBuilder::new(target, 100, hash)
            .max_fee_amount(Some(1000))
            .build()
            .unwrap();
        let mut session = PaymentSession::new_session(&store, request, 1);
        session.status = persisted;
        store.insert_payment_session(session.clone());
        let mut attempt = session.new_attempt(
            1,
            source,
            target,
            vec![PaymentHopData {
                amount: 100,
                expiry: DEFAULT_TLC_EXPIRY_DELTA,
                ..Default::default()
            }],
        );
        attempt.status = attempt_status;
        store.insert_attempt(attempt);
        assert_eq!(
            store
                .get_payment_session(hash)
                .unwrap()
                .effective_status(&store),
            effective
        );
    }
    for status in [
        PaymentStatus::Created,
        PaymentStatus::Inflight,
        PaymentStatus::Success,
        PaymentStatus::Failed,
    ] {
        let expected: Vec<_> = cases
            .iter()
            .enumerate()
            .filter(|(_, (_, _, effective))| *effective == status)
            .map(|(index, _)| Hash256::from([index as u8 + 1; 32]))
            .collect();
        for sessions in [
            store.get_payment_sessions_with_status(status),
            store.get_payment_sessions_with_limit(100, None, Some(status)),
        ] {
            assert!(
                sessions
                    .iter()
                    .all(|session| session.effective_status(&store) == status),
                "filter must match returned status: {status:?}"
            );
            assert_eq!(
                sessions
                    .iter()
                    .map(|session| session.payment_hash())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
    for (limit, after, expected) in [
        (2, 1, vec![6, 7]),
        (1, 3, vec![6]),
        (1, 7, vec![8]),
        (1, 8, vec![]),
        (0, 0, vec![]),
    ] {
        let page = store.get_payment_sessions_with_limit(
            limit,
            Some([after; 32].into()),
            Some(PaymentStatus::Inflight),
        );
        assert_eq!(
            page.iter()
                .map(|session| session.payment_hash())
                .collect::<Vec<_>>(),
            expected
                .into_iter()
                .map(|tag| Hash256::from([tag; 32]))
                .collect::<Vec<_>>()
        );
    }
    let unfiltered = store.get_payment_sessions_with_limit(3, Some([1; 32].into()), None);
    assert_eq!(
        unfiltered
            .iter()
            .map(|session| session.effective_status(&store))
            .collect::<Vec<_>>(),
        vec![
            PaymentStatus::Created,
            PaymentStatus::Success,
            PaymentStatus::Failed
        ]
    );
    for (index, &(persisted, _, _)) in cases.iter().enumerate() {
        assert_eq!(
            store.get_persisted_payment_status(Hash256::from([index as u8 + 1; 32])),
            Some(persisted),
            "queries must not rewrite durable aggregate status"
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_payment_history() {
    let (mut store, _dir) = generate_store();
    let result = TimedResult {
        fail_amount: 1,
        fail_time: 2,
        success_time: 3,
        success_amount: 4,
    };
    let channel_outpoint = OutPoint::default();
    let direction = Direction::Forward;
    store.insert_payment_history_result(channel_outpoint.clone(), direction, result);
    assert_eq!(
        store.get_payment_history_results(),
        vec![(channel_outpoint.clone(), direction, result)]
    );

    fn sort_results(results: &mut [(OutPoint, Direction, TimedResult)]) {
        results.sort_by(|a, b| match a.0.cmp(&b.0) {
            Ordering::Equal => a.1.cmp(&b.1),
            other => other,
        });
    }

    let result_2 = TimedResult {
        fail_amount: 2,
        fail_time: 3,
        success_time: 4,
        success_amount: 5,
    };
    let direction_2 = Direction::Backward;
    store.insert_payment_history_result(channel_outpoint.clone(), direction_2, result_2);
    let mut r1 = store.get_payment_history_results();
    sort_results(&mut r1);
    let mut r2: Vec<(OutPoint, Direction, TimedResult)> = vec![
        (channel_outpoint.clone(), direction, result),
        (channel_outpoint.clone(), direction_2, result_2),
    ];
    sort_results(&mut r2);
    assert_eq!(r1, r2);

    let outpoint_3 = OutPoint::new_builder()
        .tx_hash(gen_rand_sha256_hash())
        .index(1u32)
        .build();
    let direction_3 = Direction::Forward;
    let result_3 = TimedResult {
        fail_amount: 3,
        fail_time: 4,
        success_time: 5,
        success_amount: 6,
    };

    store.insert_payment_history_result(outpoint_3.clone(), direction_3, result_3);
    let mut r1 = store.get_payment_history_results();
    sort_results(&mut r1);

    let mut r2: Vec<(OutPoint, Direction, TimedResult)> = vec![
        (channel_outpoint.clone(), direction, result),
        (channel_outpoint.clone(), direction_2, result_2),
        (outpoint_3, direction_3, result_3),
    ];
    sort_results(&mut r2);
    assert_eq!(r1, r2);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_payment_custom_record() {
    let payment_hash = gen_rand_sha256_hash();
    let mut data = HashMap::new();
    data.insert(1, "hello".to_string().into_bytes());
    data.insert(2, "world".to_string().into_bytes());

    let record = PaymentCustomRecords { data };
    let (store, _temp) = generate_store();
    store.insert_payment_custom_records(&payment_hash, record.clone());
    let res = store.get_payment_custom_records(&payment_hash).unwrap();
    assert_eq!(res, record);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_serde_node_announcement_as_broadcast_message() {
    let privkey = gen_rand_fiber_private_key();
    let node_announcement = NodeAnnouncement::new_signed(
        AnnouncedNodeName::from_string("node1").expect("valid name"),
        FeatureVector::default(),
        vec![],
        &privkey,
        get_chain_hash(),
        now_timestamp_as_millis_u64(),
        0,
        Default::default(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    assert!(
        node_announcement.verify(),
        "Node announcement verification failed: {:?}",
        &node_announcement
    );
    let broadcast_message = BroadcastMessage::NodeAnnouncement(node_announcement.clone());
    let serialized = serialize_to_vec(&broadcast_message, "BroadcastMessage");
    dbg!("serialized", hex::encode(&serialized));
    let deserialized: BroadcastMessage = deserialize_from(serialized.as_ref(), "BroadcastMessage");
    assert_eq!(
        BroadcastMessage::NodeAnnouncement(node_announcement),
        deserialized
    );
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_save_channel_announcement_and_get_timestamp() {
    let (store, _dir) = generate_store();

    let timestamp = now_timestamp_as_millis_u64();
    let channel_announcement = mock_channel();
    let outpoint = channel_announcement.out_point().clone();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let timestamps = store
        .get_channel_timestamps_iter()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(timestamps, vec![(outpoint, [timestamp, 0, 0])]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_save_channel_update_and_get_timestamp() {
    let (store, _dir) = generate_store();

    let flags_for_update_of_node1 = ChannelUpdateMessageFlags::UPDATE_OF_NODE1;
    let channel_update_of_node1 = new_channel_update_unsigned(
        OutPoint::new_builder()
            .tx_hash(gen_rand_sha256_hash())
            .index(0u32)
            .build(),
        now_timestamp_as_millis_u64(),
        flags_for_update_of_node1,
        ChannelUpdateChannelFlags::empty(),
        0,
        0,
        0,
    );
    let outpoint = channel_update_of_node1.channel_outpoint.clone();
    store.save_channel_update(channel_update_of_node1.clone());
    let timestamps = store
        .get_channel_timestamps_iter()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(
        timestamps,
        vec![(outpoint.clone(), [0, channel_update_of_node1.timestamp, 0])]
    );

    let mut channel_update_of_node2 = channel_update_of_node1.clone();
    let flags_for_update_of_node2 = ChannelUpdateMessageFlags::UPDATE_OF_NODE2;
    channel_update_of_node2.message_flags = flags_for_update_of_node2;
    // Note that per discussion in Notion, we don't handle the rare case of two channel updates having the same timestamp.
    // In the current implementation, channel update from one side with the same timestamp will not overwrite the existing one
    // from the other side. So we have to set the timestamp to be different.
    channel_update_of_node2.timestamp = 2;
    store.save_channel_update(channel_update_of_node2.clone());
    let timestamps = store
        .get_channel_timestamps_iter()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(
        timestamps,
        vec![(
            outpoint,
            [
                0,
                channel_update_of_node1.timestamp,
                channel_update_of_node2.timestamp
            ]
        )]
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
struct StoreChangeSaver {
    pub changes: std::sync::RwLock<Vec<crate::store::store_impl::StoreChange>>,
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_node_namespace_does_not_inherit_public_store_watcher() {
    use crate::store::store_impl::StoreChange;
    use std::sync::Arc;

    let (mut store, _dir) = generate_store();
    let saver = Arc::new(StoreChangeSaver::default());
    let saver_clone = saver.clone();
    store.set_watcher(Arc::new(move |change: StoreChange| {
        saver_clone.changes.write().unwrap().push(change);
    }));
    let tenant_store = store.namespaced(NodeNamespace::hosted_tenant("u1"));

    tenant_store.insert_preimage(gen_rand_sha256_hash(), gen_rand_sha256_hash());
    assert!(saver.changes.read().unwrap().is_empty());

    store.insert_preimage(gen_rand_sha256_hash(), gen_rand_sha256_hash());
    assert_eq!(saver.changes.read().unwrap().len(), 1);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_change_watcher() {
    use crate::store::store_impl::StoreChange;
    use std::sync::Arc;

    let (mut store, _dir) = generate_store();
    let saver = Arc::new(StoreChangeSaver::default());
    let saver_clone = saver.clone();
    store.set_watcher(Arc::new(move |change: StoreChange| {
        saver_clone.changes.write().unwrap().push(change);
    }));

    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_preimage(preimage)
        .fallback_address("address".to_string())
        .add_attr(Attribute::FinalHtlcMinimumExpiryDelta(5))
        .build()
        .unwrap();
    let payment_hash = *invoice.payment_hash();

    store
        .insert_invoice(invoice.clone(), Some(preimage))
        .unwrap();

    let payment_data = SendPaymentDataBuilder::new(gen_rand_fiber_public_key(), 100, payment_hash)
        .final_tlc_expiry_delta(DEFAULT_TLC_EXPIRY_DELTA)
        .tlc_expiry_limit(MAX_PAYMENT_TLC_EXPIRY_LIMIT)
        .timeout(Some(10))
        .max_fee_amount(Some(1000))
        .build()
        .expect("valid payment_data");
    let payment_session = PaymentSession::new_session(&store, payment_data, 10);
    let source = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let route_hops = vec![PaymentHopData {
        amount: 100,
        expiry: DEFAULT_TLC_EXPIRY_DELTA,
        payment_preimage: None,
        hash_algorithm: HashAlgorithm::default(),
        funding_tx_hash: gen_rand_sha256_hash(),
        next_hop: None,
        custom_records: None,
    }];
    let mut attempt = payment_session.new_attempt(1, source, target, route_hops);
    attempt.set_inflight_status();
    store.insert_attempt(attempt);

    let changes = saver.changes.read().unwrap();
    assert!(changes.iter().any(
        |e| matches!(e, StoreChange::PutCkbInvoiceStatus { payment_hash: h, invoice_status: CkbInvoiceStatus::Open } if h == &payment_hash)
    ));
    assert!(changes.iter().any(
        |e| matches!(e, StoreChange::PutPreimage { payment_hash: h, payment_preimage: i } if h == &payment_hash && i == &preimage)
    ));
    assert!(changes.iter().any(
        |e| matches!(e, StoreChange::PutAttempt { payment_hash: h, attempt_status: AttemptStatus::Inflight } if h == &payment_hash)
    ));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_insert_preimage_replay_notifies_watcher() {
    use crate::store::store_impl::StoreChange;
    use std::sync::Arc;

    let (mut store, _dir) = generate_store();
    let saver = Arc::new(StoreChangeSaver::default());
    let saver_clone = saver.clone();
    store.set_watcher(Arc::new(move |change: StoreChange| {
        saver_clone.changes.write().unwrap().push(change);
    }));

    let payment_hash = gen_rand_sha256_hash();
    let preimage = gen_rand_sha256_hash();

    store.insert_preimage(payment_hash, preimage);
    store.insert_preimage(payment_hash, preimage);

    let changes = saver.changes.read().unwrap();
    let put_preimage_count = changes
        .iter()
        .filter(
            |e| matches!(e, StoreChange::PutPreimage { payment_hash: h, payment_preimage: i } if h == &payment_hash && i == &preimage),
        )
        .count();
    assert_eq!(put_preimage_count, 2);
    assert_eq!(store.get_preimage(&payment_hash), Some(preimage));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_sample_channel_actor_state() {
    let samples = ChannelActorState::samples(42);
    assert!(!samples.is_empty());

    let path = TempDir::new("sample_channel_actor_state_store");
    let store = open_store(path).expect("create store failed");

    // Insert all samples
    for sample in &samples {
        assert!(store.get_channel_actor_state(&sample.id).is_none());
        let mut with_public_cache = sample.clone();
        with_public_cache
            .signing_context
            .v2_commitment_points
            .push((100, gen_rand_fiber_public_key()));
        with_public_cache
            .signing_context
            .v2_nonce_pool
            .push(musig2::SecNonce::build([42; 32]).build().public_nonce());
        store.insert_channel_actor_state(with_public_cache);
        // Decode the raw core to verify the write removed derivable material.
        let key = [
            &[fiber_types::schema::CHANNEL_ACTOR_STATE_PREFIX],
            sample.id.as_ref(),
        ]
        .concat();
        let raw = store.get(key).unwrap();
        let persisted: fiber_types::ChannelMutableData =
            deserialize_from(raw.as_ref(), "ChannelMutableData");
        if sample.signer.is_some() {
            assert!(persisted.signing_context.v2_commitment_points.is_empty());
            assert!(persisted.signing_context.v2_nonce_pool.is_empty());
        } else {
            assert!(persisted
                .signing_context
                .v2_commitment_points
                .iter()
                .any(|(number, _)| *number == 100));
        }
    }

    // Verify each sample can be queried back and key fields match
    for sample in &samples {
        let loaded = store
            .get_channel_actor_state(&sample.id)
            .expect("should find stored channel state");

        // Verify core fields roundtrip correctly
        assert_eq!(loaded.id, sample.id);
        assert_eq!(loaded.state, sample.state);
        assert_eq!(loaded.is_acceptor, sample.is_acceptor);
        assert_eq!(loaded.is_one_way, sample.is_one_way);
        assert_eq!(loaded.to_local_amount, sample.to_local_amount);
        assert_eq!(loaded.to_remote_amount, sample.to_remote_amount);
        assert_eq!(loaded.commitment_fee_rate, sample.commitment_fee_rate);
        assert_eq!(loaded.reestablishing, sample.reestablishing);
        assert_eq!(
            loaded.local_reserved_ckb_amount,
            sample.local_reserved_ckb_amount
        );
        assert_eq!(
            loaded.remote_reserved_ckb_amount,
            sample.remote_reserved_ckb_amount
        );
        assert_eq!(loaded.local_constraints, sample.local_constraints);
        assert_eq!(loaded.remote_constraints, sample.remote_constraints);
        assert_eq!(
            loaded.shutdown_transaction_hash,
            sample.shutdown_transaction_hash
        );

        // Verify pubkey index
        let remote_pubkey = sample.get_remote_pubkey();
        let channel_ids = store.get_channel_ids_by_pubkey(&remote_pubkey);
        assert!(
            channel_ids.contains(&sample.id),
            "pubkey index should contain the channel id"
        );
    }

    // Delete and verify removal
    for sample in &samples {
        store.delete_channel_actor_state(&sample.id);
        assert!(
            store.get_channel_actor_state(&sample.id).is_none(),
            "channel state should be deleted"
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_channel_open_record() {
    use crate::fiber::channel::ChannelOpenRecordStore;
    use crate::store::sample::deterministic_pubkey;
    use fiber_types::{ChannelOpenRecord, ChannelOpeningStatus};

    let samples = ChannelOpenRecord::samples(42);
    assert!(!samples.is_empty());

    let path = TempDir::new("channel_open_record_store");
    let store = open_store(path).expect("create store failed");

    // Initially no records
    assert!(store.get_channel_open_records().is_empty());

    // Insert all samples
    for sample in &samples {
        assert!(store.get_channel_open_record(&sample.channel_id).is_none());
        store.insert_channel_open_record(sample.clone());
    }

    // Query all
    assert_eq!(store.get_channel_open_records().len(), samples.len());

    // Query by channel_id
    for sample in &samples {
        let loaded = store
            .get_channel_open_record(&sample.channel_id)
            .expect("should find stored record");
        assert_eq!(loaded.channel_id, sample.channel_id);
        assert_eq!(loaded.status, sample.status);
        assert_eq!(loaded.failure_detail, sample.failure_detail);
    }

    // Delete and verify removal
    for sample in &samples {
        store.delete_channel_open_record(&sample.channel_id);
        assert!(store.get_channel_open_record(&sample.channel_id).is_none());
    }
    assert!(store.get_channel_open_records().is_empty());

    // Test update_status helper
    let mut record = ChannelOpenRecord::new(
        deterministic_hash256(42, 99),
        deterministic_pubkey(999, 0),
        100_0000_0000,
    );
    assert_eq!(record.status, ChannelOpeningStatus::WaitingForPeer);
    record.update_status(ChannelOpeningStatus::FundingTxBuilding);
    assert_eq!(record.status, ChannelOpeningStatus::FundingTxBuilding);
    record.fail("test failure".to_string());
    assert_eq!(record.status, ChannelOpeningStatus::Failed);
    assert_eq!(record.failure_detail.as_deref(), Some("test failure"));
}

#[cfg(not(target_arch = "wasm32"))]
fn deterministic_hash256(seed: u64, index: u32) -> fiber_types::Hash256 {
    crate::store::sample::deterministic_hash(seed, index).into()
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_get_broadcast_messages_reverse_excludes_cursor() {
    let (store, _dir) = generate_store();
    let first_timestamp = now_timestamp_as_millis_u64();
    let first_channel_announcement = mock_channel();
    let first_outpoint = first_channel_announcement.out_point().clone();
    store.save_channel_announcement(first_timestamp, first_channel_announcement.clone());

    let second_timestamp = first_timestamp + 1;
    let second_channel_announcement = mock_channel();
    let second_outpoint = second_channel_announcement.out_point().clone();
    store.save_channel_announcement(second_timestamp, second_channel_announcement.clone());

    let latest = store.get_broadcast_messages_reverse(None, 1);
    assert_eq!(
        latest,
        vec![BroadcastMessageWithTimestamp::ChannelAnnouncement(
            second_timestamp,
            second_channel_announcement.clone(),
        )],
    );

    let before_cursor = Cursor::new(
        second_timestamp,
        BroadcastMessageID::ChannelAnnouncement(second_outpoint),
    );
    let previous = store.get_broadcast_messages_reverse(Some(&before_cursor), 1);
    assert_eq!(
        previous,
        vec![BroadcastMessageWithTimestamp::ChannelAnnouncement(
            first_timestamp,
            first_channel_announcement,
        )],
    );

    let first_cursor = Cursor::new(
        first_timestamp,
        BroadcastMessageID::ChannelAnnouncement(first_outpoint),
    );
    assert!(store
        .get_broadcast_messages_reverse(Some(&first_cursor), 1)
        .is_empty());
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod store_actor_tests {
    use crate::actors::RootActor;
    use crate::store::actor::{StoreActor, StoreActorInitializationParameter, StoreActorMessage};
    use crate::tasks::{new_tokio_cancellation_token, new_tokio_task_tracker};
    use fiber_store::backend::TakeWhileFn;
    use fiber_store::{IteratorDirection, KVPair, StorageBackend, StoreError};
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::time::{advance, sleep};

    struct MockStore {
        backup_count: Arc<AtomicUsize>,
    }

    impl StorageBackend for MockStore {
        type Batch = <fiber_store::Store as StorageBackend>::Batch;

        fn get<K: AsRef<[u8]>>(&self, _key: K) -> Option<Vec<u8>> {
            todo!()
        }
        fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, _key: K, _value: V) {
            todo!()
        }
        fn delete<K: AsRef<[u8]>>(&self, _key: K) {
            todo!()
        }
        fn multi_get<K: AsRef<[u8]>>(&self, _keys: &[K]) -> Vec<Option<Vec<u8>>> {
            todo!()
        }
        fn batch(&self) -> Self::Batch {
            todo!()
        }
        fn collect_iterator(
            &self,
            _: Vec<u8>,
            _: IteratorDirection,
            _: TakeWhileFn,
            _: usize,
        ) -> Vec<KVPair> {
            Vec::new()
        }
        fn restore(&self, _: &Path, _: &Path) -> Result<(), StoreError> {
            todo!()
        }

        fn backup(&self, _path: &std::path::Path) -> Result<(), StoreError> {
            self.backup_count.fetch_add(1, SeqCst);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_store_actor_buffered_backup() {
        let backup_count = Arc::new(AtomicUsize::new(0));
        let mock_store = MockStore {
            backup_count: Arc::clone(&backup_count),
        };
        let temp_dir = tempdir().unwrap();

        let (tracker, token) = (new_tokio_task_tracker(), new_tokio_cancellation_token());
        let root_actor = RootActor::start_for_test(tracker, token).await;

        let args = StoreActorInitializationParameter {
            store: mock_store,
            backup_path: temp_dir.path().to_path_buf(),
            ckb_key_path: temp_dir.path().to_path_buf(),
            fiber_key_path: temp_dir.path().to_path_buf(),
            tenant_store_root: None,
            backup_interval_hours: 24,
        };

        let (store_actor, _handle) =
            ractor::Actor::spawn_linked(None, StoreActor::new(), args, root_actor.get_cell())
                .await
                .unwrap();

        // First request should trigger the backup immediately
        store_actor.cast(StoreActorMessage::RequestBackup).unwrap();
        tokio::task::yield_now().await;
        sleep(Duration::from_millis(100)).await;

        advance(Duration::from_secs(61)).await;
        sleep(Duration::from_millis(100)).await;

        assert_eq!(
            backup_count.load(SeqCst),
            1,
            "The backup request must be executed."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_store_actor_backup_throttling() {
        let backup_count = Arc::new(AtomicUsize::new(0));
        let mock_store = MockStore {
            backup_count: Arc::clone(&backup_count),
        };
        let temp_dir = tempdir().unwrap();
        let (tracker, token) = (new_tokio_task_tracker(), new_tokio_cancellation_token());
        let root_actor = RootActor::start_for_test(tracker, token).await;

        let (store_actor, _) = ractor::Actor::spawn_linked(
            None,
            StoreActor::new(),
            StoreActorInitializationParameter {
                store: mock_store,
                backup_path: temp_dir.path().to_path_buf(),
                ckb_key_path: temp_dir.path().to_path_buf(),
                fiber_key_path: temp_dir.path().to_path_buf(),
                tenant_store_root: None,
                backup_interval_hours: 24,
            },
            root_actor.get_cell(),
        )
        .await
        .unwrap();

        // Trigger initial backup
        store_actor.cast(StoreActorMessage::RequestBackup).unwrap();
        tokio::task::yield_now().await;

        // Send multiple requests within the 60-second cooldown period
        for _ in 0..20 {
            store_actor.cast(StoreActorMessage::RequestBackup).unwrap();
        }
        tokio::task::yield_now().await;

        advance(Duration::from_secs(61)).await;
        sleep(Duration::from_millis(100)).await;

        assert_eq!(
            backup_count.load(SeqCst),
            1,
            "Requests during cooldown should be throttled and not trigger new backups."
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_tenant_channel_opening_persists_before_and_after_final_id() {
    let path = TempDir::new("tenant_channel_opening");
    let created = fiber_lsp_sdk::RootSigner::in_memory().await.unwrap();
    let signer = created.root_signer.create_channel().await.unwrap();
    let material = signer.channel_open_material(false).await.unwrap();
    let request: fiber_json_types::OpenChannelWithExternalFundingParams = serde_json::from_value(serde_json::json!({
            "pubkey": material.base_public_keys.funding_pubkey, "public": false,
        "funding_amount":"0xba43b7400", "shutdown_script":ckb_jsonrpc_types::Script::from(Script::default()),
        "funding_lock_script":ckb_jsonrpc_types::Script::from(Script::default()),
        "external_channel_signer":fiber_lsp_sdk::json::open_material_to_rpc(&material),
    })).unwrap();
    let mut command = crate::rpc::channel::external_funding_command(&request).unwrap();
    command.funding_amount = u128::MAX;
    command.tlc_min_value = Some(u128::MAX);
    command.tlc_fee_proportional_millionths = Some(u128::MAX);
    command.max_tlc_value_in_flight = Some(u128::MAX);
    command.commitment_delay_epoch = Some(ckb_types::core::EpochNumberWithFraction::new(1, 0, 1));
    let temporary_id = Hash256::from([1; 32]);
    let final_id = Hash256::from([2; 32]);
    {
        let store = open_store(&path).unwrap();
        let tenant = store.namespaced(NodeNamespace::hosted_tenant("opening-owner"));
        tenant.put_tenant_channel_opening(TenantChannelOpening {
            command: command.clone(),
            channel_id: temporary_id,
            result: None,
            terminated: false,
        });
        assert!(store.get_tenant_channel_opening(&temporary_id).is_none());
        assert!(store
            .namespaced(NodeNamespace::hosted_tenant("other-tenant"))
            .get_tenant_channel_opening(&temporary_id)
            .is_none());
    }
    let result = fiber_json_types::OpenChannelWithExternalFundingResult {
        channel_id: final_id.into(),
        unsigned_funding_tx: Transaction::default().into(),
        opening_context: Some(fiber_json_types::TenantChannelOpeningContext {
            remote_funding_key: material.base_public_keys.funding_pubkey.into(),
            remote_settlement_key: material.base_public_keys.tlc_base_key.into(),
            remote_shutdown_script: Script::default().into(),
            commitment_delay_epoch: 1.into(),
            commitment_fee_rate: 1000,
            local_amount: 50_000_000_000,
            remote_amount: 50_000_000_000,
            local_reserved_ckb_amount: 9_900_000_000,
            remote_reserved_ckb_amount: 9_900_000_000,
        }),
    };
    {
        let store = open_store(&path)
            .unwrap()
            .namespaced(NodeNamespace::hosted_tenant("opening-owner"));
        let mut pending = store.get_tenant_channel_opening(&temporary_id).unwrap();
        assert_eq!(pending.channel_id, temporary_id);
        assert!(pending.result.is_none());
        assert_eq!(
            serde_json::to_value(&pending.command).unwrap(),
            serde_json::to_value(&command).unwrap()
        );
        store.delete_tenant_channel_opening(&temporary_id);
        pending.channel_id = final_id;
        pending.result = Some(result.clone());
        store.put_tenant_channel_opening(pending);
    }
    let store = open_store(&path)
        .unwrap()
        .namespaced(NodeNamespace::hosted_tenant("opening-owner"));
    let restored = store.get_tenant_channel_opening(&final_id).unwrap();
    assert_eq!(restored.channel_id, final_id);
    assert_eq!(
        serde_json::to_value(restored.result.unwrap()).unwrap(),
        serde_json::to_value(result).unwrap()
    );

    let second_id = Hash256::from([3; 32]);
    store.put_tenant_channel_opening(TenantChannelOpening {
        command: command.clone(),
        channel_id: second_id,
        result: None,
        terminated: false,
    });
    assert_eq!(store.list_tenant_channel_openings().len(), 2);
    assert!(store.get_tenant_channel_opening(&second_id).is_some());
    store.delete_tenant_channel_opening(&second_id);
    assert!(store.get_tenant_channel_opening(&second_id).is_none());
    assert_eq!(store.list_tenant_channel_openings().len(), 1);
}

#[test]
fn test_move_channel_open_records_atomically_rekeys_both_records() {
    use crate::fiber::channel::{ChannelOpenRecordStore, TenantChannelOpening};
    use crate::fiber::network::OpenChannelWithExternalFundingCommand;
    use crate::fiber::types::Privkey;
    use ckb_types::packed::Script;
    use fiber_types::{ChannelOpenRecord, ChannelOpeningStatus};

    let (store, _dir) = generate_store();
    let tenant = store.namespaced(NodeNamespace::hosted_tenant("atomic-move-owner"));
    let temporary_id = Hash256::from([0x11; 32]);
    let final_id = Hash256::from([0x22; 32]);
    let peer_pubkey = Privkey::from(&[2u8; 32]).pubkey();
    let command = OpenChannelWithExternalFundingCommand {
        pubkey: peer_pubkey,
        funding_amount: 100_000_000_000,
        public: false,
        shutdown_script: Script::default(),
        funding_lock_script: Script::default(),
        funding_lock_script_cell_deps: Vec::new(),
        funding_udt_type_script: None,
        commitment_fee_rate: None,
        commitment_delay_epoch: None,
        funding_fee_rate: None,
        tlc_expiry_delta: None,
        tlc_min_value: None,
        tlc_fee_proportional_millionths: None,
        max_tlc_value_in_flight: None,
        max_tlc_number_in_flight: None,
        external_channel_signer: None,
    };

    let mut record = ChannelOpenRecord::new(temporary_id, peer_pubkey, command.funding_amount);
    let mut opening = TenantChannelOpening {
        command,
        channel_id: temporary_id,
        result: None,
        terminated: false,
    };
    tenant.insert_channel_open_record(record.clone());
    tenant.put_tenant_channel_opening(opening.clone());

    record.channel_id = final_id;
    record.update_status(ChannelOpeningStatus::FundingTxBuilding);
    opening.channel_id = final_id;
    tenant.move_channel_open_records(&temporary_id, Some(record.clone()), Some(opening.clone()));

    assert!(tenant.get_channel_open_record(&temporary_id).is_none());
    assert!(tenant.get_tenant_channel_opening(&temporary_id).is_none());
    let moved_record = tenant
        .get_channel_open_record(&final_id)
        .expect("re-keyed open record");
    assert_eq!(moved_record.channel_id, final_id);
    assert_eq!(moved_record.status, ChannelOpeningStatus::FundingTxBuilding);
    let moved_opening = tenant
        .get_tenant_channel_opening(&final_id)
        .expect("re-keyed tenant opening");
    assert_eq!(moved_opening.channel_id, final_id);
    assert_eq!(tenant.list_tenant_channel_openings().len(), 1);

    // Same-ID update must keep both rows in place rather than deleting them.
    record.update_status(ChannelOpeningStatus::ChannelReady);
    opening.terminated = true;
    tenant.move_channel_open_records(&final_id, Some(record), Some(opening));
    assert_eq!(
        tenant.get_channel_open_record(&final_id).unwrap().status,
        ChannelOpeningStatus::ChannelReady
    );
    assert!(
        tenant
            .get_tenant_channel_opening(&final_id)
            .unwrap()
            .terminated
    );
}

#[test]
fn test_move_channel_open_records_atomically_with_channel_actor_state() {
    use crate::fiber::channel::{
        ChannelActorStateStore, ChannelOpenRecordStore, TenantChannelOpening,
    };
    use crate::fiber::network::OpenChannelWithExternalFundingCommand;
    use ckb_types::packed::Script;
    use fiber_types::{ChannelOpenRecord, ChannelOpeningStatus, NegotiatingFundingFlags};

    let (store, _dir) = generate_store();
    let tenant = store.namespaced(NodeNamespace::hosted_tenant("atomic-move-channel-owner"));
    let mut channel = sample_channel_actor_state(
        ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT),
        None,
    );
    let temporary_id = channel.id;
    let final_id = gen_rand_sha256_hash();
    let peer_pubkey = channel.remote_pubkey;
    let command = OpenChannelWithExternalFundingCommand {
        pubkey: peer_pubkey,
        funding_amount: 100_000_000_000,
        public: false,
        shutdown_script: Script::default(),
        funding_lock_script: Script::default(),
        funding_lock_script_cell_deps: Vec::new(),
        funding_udt_type_script: None,
        commitment_fee_rate: None,
        commitment_delay_epoch: None,
        funding_fee_rate: None,
        tlc_expiry_delta: None,
        tlc_min_value: None,
        tlc_fee_proportional_millionths: None,
        max_tlc_value_in_flight: None,
        max_tlc_number_in_flight: None,
        external_channel_signer: None,
    };

    tenant.insert_channel_actor_state(channel.clone());
    tenant.insert_channel_open_record(ChannelOpenRecord::new(
        temporary_id,
        peer_pubkey,
        command.funding_amount,
    ));
    tenant.put_tenant_channel_opening(TenantChannelOpening {
        command,
        channel_id: temporary_id,
        result: None,
        terminated: false,
    });

    channel.id = final_id;
    channel.state = ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT);
    tenant.move_channel_actor_state(&temporary_id, channel.clone());

    assert!(tenant.get_channel_actor_state(&temporary_id).is_none());
    assert!(tenant.get_channel_open_record(&temporary_id).is_none());
    assert!(tenant.get_tenant_channel_opening(&temporary_id).is_none());

    let moved_channel = tenant
        .get_channel_actor_state(&final_id)
        .expect("channel moved to final id");
    assert_eq!(moved_channel.id, final_id);
    let moved_record = tenant
        .get_channel_open_record(&final_id)
        .expect("open record moved with channel state");
    assert_eq!(moved_record.channel_id, final_id);
    assert_eq!(moved_record.status, ChannelOpeningStatus::FundingTxBuilding);
    let moved_opening = tenant
        .get_tenant_channel_opening(&final_id)
        .expect("tenant opening moved with channel state");
    assert_eq!(moved_opening.channel_id, final_id);
    assert_eq!(tenant.list_tenant_channel_openings().len(), 1);

    // Same-ID move must preserve existing open records unchanged.
    let mut ready_record = moved_record;
    ready_record.update_status(ChannelOpeningStatus::ChannelReady);
    tenant.insert_channel_open_record(ready_record);
    tenant.move_channel_actor_state(&final_id, channel);
    assert_eq!(
        tenant.get_channel_open_record(&final_id).unwrap().status,
        ChannelOpeningStatus::ChannelReady
    );
    assert_eq!(
        tenant
            .get_tenant_channel_opening(&final_id)
            .unwrap()
            .channel_id,
        final_id
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_archive_v1_channel_before_open_store() {
    use fiber_types_0100::sample::StoreSample;

    let path = TempDir::new("test-archive-v1-before-open");
    let id_bytes = {
        let store = fiber_store::Store::open_db(path.as_ref()).expect("raw store");
        store.put(
            fiber_store::migration::MIGRATION_VERSION_KEY,
            b"20260925120000",
        );
        let mut channel = fiber_types_0100::ChannelActorData::samples(1).remove(0);
        channel.state = fiber_types_0100::ChannelState::AwaitingChannelReady(
            fiber_types_0100::AwaitingChannelReadyFlags::empty(),
        );
        let id_bytes = <[u8; 32]>::try_from(channel.id.as_ref()).expect("channel id");
        store.put(
            [&[0u8][..], channel.id.as_ref()].concat(),
            bincode::serialize(&channel).expect("encode V1 channel"),
        );
        id_bytes
    };

    let id = fiber_types::Hash256::from(id_bytes);
    let summaries = crate::store::archive_v1_channels(path.as_ref(), &[id], true)
        .expect("archive V1 channel before opening the store");
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].state.contains("AwaitingChannelReady"));

    let store = open_store(path.as_ref()).expect("migration after archive");
    assert!(
        store.get_channel_actor_state(&id).is_none(),
        "archived channel must not be loaded after migration"
    );
}
