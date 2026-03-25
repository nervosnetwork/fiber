use crate::ckb::signer::LocalSigner;
use crate::fiber::channel::*;
use crate::fiber::gossip::{get_latest_startup_broadcast_message_cursor, GossipMessageStore};
use crate::fiber::network::get_chain_hash;
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
use crate::gen_rand_sha256_hash;
use crate::invoice::*;
use crate::now_timestamp_as_millis_u64;
use crate::store::open_store;
#[cfg(not(target_arch = "wasm32"))]
use crate::store::sample::StoreSample;
use crate::store::store_impl::deserialize_from;
use crate::store::store_impl::serialize_to_vec;
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
use fiber_types::protocol::AnnouncedNodeName;
use musig2::secp::MaybeScalar;
#[cfg(not(target_arch = "wasm32"))]
use musig2::CompactSignature;
use musig2::SecNonce;
use secp256k1::{Keypair, SECP256K1};
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use tentacle::secio::PeerId;

fn gen_rand_local_signer() -> LocalSigner {
    let keypair = Keypair::new(SECP256K1, &mut rand::thread_rng());
    LocalSigner::new(keypair.secret_key())
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
fn test_store_get_broadcast_messages_iter() {
    let (store, _dir) = generate_store();
    let timestamp = now_timestamp_as_millis_u64();
    let channel_announcement = mock_channel();
    let outpoint = channel_announcement.out_point().clone();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let default_cursor = Cursor::default();
    let mut iter = store
        .get_broadcast_messages_iter(&default_cursor)
        .into_iter();
    assert_eq!(
        iter.next(),
        Some(BroadcastMessageWithTimestamp::ChannelAnnouncement(
            timestamp,
            channel_announcement
        )),
    );
    assert_eq!(iter.next(), None);
    let cursor = Cursor::new(timestamp, BroadcastMessageID::ChannelAnnouncement(outpoint));
    let mut iter = store.get_broadcast_messages_iter(&cursor).into_iter();
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
    let result = store.get_broadcast_messages(&default_cursor, None);
    assert_eq!(
        result,
        vec![BroadcastMessageWithTimestamp::ChannelAnnouncement(
            timestamp,
            channel_announcement
        )],
    );
    let cursor = Cursor::new(timestamp, BroadcastMessageID::ChannelAnnouncement(outpoint));
    let result = store.get_broadcast_messages(&cursor, None);
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
        local_settlement_key.clone(),
        remote_settlement_key,
        local_funding_pubkey,
        remote_funding_pubkey,
        settlement_data.clone(),
    );
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_udt_type_script: None,
            local_settlement_key: local_settlement_key.clone(),
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            revocation_data: None,
            local_settlement_data: settlement_data.clone(),
            pending_remote_settlement_data: settlement_data.clone(),
            remote_settlement_data: settlement_data.clone(),
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
            local_settlement_key,
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            local_settlement_data: settlement_data.clone(),
            revocation_data: Some(revocation_data),
            pending_remote_settlement_data: settlement_data.clone(),
            remote_settlement_data: settlement_data,
        }]
    );

    store.remove_watch_channel(node_id, channel_id);
    assert_eq!(store.get_watch_channels(), vec![]);
}
#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_watchtower_preimage() {
    let path = TempDir::new("test-watchtower-store");
    let store = open_store(path).expect("created store failed");

    let node_id_a = NodeId::from_bytes(PeerId::random().into_bytes());
    let preimage_a = gen_rand_sha256_hash();
    let payment_hash_a = blake2b_256(preimage_a).into();

    let node_id_b = NodeId::local();
    let preimage_b = gen_rand_sha256_hash();
    let payment_hash_b = blake2b_256(preimage_b).into();

    let preimage_c = gen_rand_sha256_hash();
    let payment_hash_c = blake2b_256(preimage_c).into();

    store.insert_watch_preimage(node_id_a.clone(), payment_hash_a, preimage_a);
    store.insert_watch_preimage(node_id_b.clone(), payment_hash_b, preimage_b);

    assert!(
        store.get_preimage(&payment_hash_a).is_some(),
        "should return a watch preimage also"
    );
    assert_eq!(
        store.get_watch_preimage(&payment_hash_a).unwrap(),
        preimage_a,
        "query watch preimage"
    );

    // watch preimage should not return a node preimage
    store.insert_preimage(payment_hash_c, preimage_c);
    assert!(
        store.get_watch_preimage(&payment_hash_c).is_none(),
        "query non exist watch preimage"
    );

    assert!(
        store
            .search_preimage(&payment_hash_c.as_ref()[..20])
            .is_none(),
        "search a non exist watch preimage"
    );
    // search preimage only returns watch preimage
    assert_eq!(
        store
            .search_preimage(&payment_hash_a.as_ref()[..20])
            .unwrap(),
        preimage_a,
        "search"
    );

    // delete preimage with wrong node
    store.remove_watch_preimage(node_id_a, payment_hash_b);
    assert!(store.get_watch_preimage(&payment_hash_b).is_some(), "exist");

    store.remove_watch_preimage(node_id_b, payment_hash_b);
    assert!(
        store.get_watch_preimage(&payment_hash_b).is_none(),
        "removed"
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
        local_settlement_key.clone(),
        remote_settlement_key,
        local_funding_pubkey,
        remote_funding_pubkey,
        settlement_data.clone(),
    );
    let expected_value = vec![ChannelData {
        channel_id,
        funding_udt_type_script: None,
        local_settlement_key: local_settlement_key.clone(),
        remote_settlement_key,
        local_funding_pubkey,
        remote_funding_pubkey,
        revocation_data: None,
        local_settlement_data: settlement_data.clone(),
        pending_remote_settlement_data: settlement_data.clone(),
        remote_settlement_data: settlement_data.clone(),
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
        core: ChannelActorData {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
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
            local_channel_public_keys: ChannelBasePublicKeys {
                funding_pubkey: gen_rand_fiber_public_key(),
                tlc_base_key: gen_rand_fiber_public_key(),
            },
            signer,
            remote_channel_public_keys: Some(ChannelBasePublicKeys {
                funding_pubkey: gen_rand_fiber_public_key(),
                tlc_base_key: gen_rand_fiber_public_key(),
            }),
            commitment_numbers: Default::default(),
            remote_shutdown_script: Some(Script::default()),
            last_committed_remote_nonce: None,
            remote_revocation_nonce_for_verify: None,
            remote_revocation_nonce_for_send: None,
            remote_revocation_nonce_for_next: None,
            remote_commitment_points: vec![
                (0, gen_rand_fiber_public_key()),
                (1, gen_rand_fiber_public_key()),
            ],
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash: None,
            local_reserved_ckb_amount: 100,
            remote_reserved_ckb_amount: 100,
            latest_commitment_transaction: None,
            local_constraints: ChannelConstraints::default(),
            remote_constraints: ChannelConstraints::default(),
            reestablishing: false,
            last_revoke_ack_msg: None,
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
            created_at: SystemTime::now(),
        },
        waiting_peer_response: None,
        network: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        pending_reestablish_channel_ready: false,
        defer_peer_tlc_updates: false,
        deferred_peer_tlc_updates: Default::default(),
        ephemeral_config: Default::default(),
        private_key: None,
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

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_serde_channel_actor_state_ciborium() {
    let seed = [0u8; 32];
    let signer = InMemorySigner::generate_from_seed(&seed);

    let seckey = blake2b_hash_with_salt(
        signer.musig2_base_nonce.as_ref(),
        b"channel_announcement".as_slice(),
    );
    let sec_nonce = SecNonce::build(seckey).build();
    let pub_nonce = sec_nonce.public_nonce();

    let state = ChannelActorState {
        core: ChannelActorData {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
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
            id: gen_rand_sha256_hash(),
            tlc_state: Default::default(),
            retryable_tlc_operations: Default::default(),
            waiting_forward_tlc_tasks: Default::default(),
            local_shutdown_script: Script::default(),
            local_channel_public_keys: ChannelBasePublicKeys {
                funding_pubkey: gen_rand_fiber_public_key(),
                tlc_base_key: gen_rand_fiber_public_key(),
            },
            signer,
            remote_channel_public_keys: Some(ChannelBasePublicKeys {
                funding_pubkey: gen_rand_fiber_public_key(),
                tlc_base_key: gen_rand_fiber_public_key(),
            }),
            commitment_numbers: Default::default(),
            remote_shutdown_script: Some(Script::default()),
            last_committed_remote_nonce: None,
            remote_revocation_nonce_for_verify: None,
            remote_revocation_nonce_for_send: None,
            remote_revocation_nonce_for_next: None,
            remote_commitment_points: vec![
                (0, gen_rand_fiber_public_key()),
                (1, gen_rand_fiber_public_key()),
            ],
            local_shutdown_info: None,
            remote_shutdown_info: None,
            shutdown_transaction_hash: None,
            local_reserved_ckb_amount: 100,
            remote_reserved_ckb_amount: 100,
            latest_commitment_transaction: None,
            local_constraints: ChannelConstraints::default(),
            remote_constraints: ChannelConstraints::default(),
            reestablishing: false,
            last_revoke_ack_msg: None,
            pending_replay_updates: vec![],
            last_was_revoke: false,
            created_at: SystemTime::now(),
        },
        waiting_peer_response: None,
        network: None,
        scheduled_channel_update_handle: None,
        pending_notify_settle_tlcs: vec![],
        pending_reestablish_channel_ready: false,
        defer_peer_tlc_updates: false,
        deferred_peer_tlc_updates: Default::default(),
        ephemeral_config: Default::default(),
        private_key: None,
    };

    let mut serialized = Vec::new();
    ciborium::into_writer(&state, &mut serialized).unwrap();
    let _new_channel_state: ChannelActorState =
        ciborium::from_reader(serialized.as_slice()).expect("deserialize to new state");
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

    let changes = saver.changes.read().unwrap();
    assert!(changes.iter().any(
        |e| matches!(e, StoreChange::PutCkbInvoiceStatus { payment_hash: h, invoice_status: CkbInvoiceStatus::Open } if h == &payment_hash)
    ));
    assert!(changes.iter().any(
        |e| matches!(e, StoreChange::PutPreimage { payment_hash: h, payment_preimage: i } if h == &payment_hash && i == &preimage)
    ));
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
        store.insert_channel_actor_state(sample.clone());
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

#[cfg(not(target_arch = "wasm32"))]
fn make_forwarding_event(timestamp: u64, fee: u128) -> fiber_types::ForwardingEvent {
    fiber_types::ForwardingEvent {
        timestamp,
        incoming_channel_id: gen_rand_sha256_hash(),
        outgoing_channel_id: gen_rand_sha256_hash(),
        incoming_amount: 1000 + fee,
        outgoing_amount: 1000,
        fee,
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script: None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn query_all_forwarding_events(
    store: &impl PaymentEventStore,
    start_time: u64,
    end_time: u64,
    limit: usize,
    after: Option<ForwardingHistoryCursor>,
) -> (
    Vec<fiber_types::ForwardingEvent>,
    Option<ForwardingHistoryCursor>,
) {
    store
        .query_forwarding_events(ForwardingHistoryQuery {
            asset: AssetSelector::All,
            start_time,
            end_time,
            limit,
            after,
        })
        .expect("query all forwarding events")
}

#[cfg(not(target_arch = "wasm32"))]
fn query_all_payment_events(
    store: &impl PaymentEventStore,
    start_time: u64,
    end_time: u64,
    limit: usize,
    after: Option<PaymentHistoryCursor>,
) -> (Vec<fiber_types::PaymentEvent>, Option<PaymentHistoryCursor>) {
    store
        .query_payment_events(PaymentHistoryQuery {
            asset: AssetSelector::All,
            event_type: None,
            start_time,
            end_time,
            limit,
            after,
        })
        .expect("query all payment events")
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_insert_and_query() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Initially empty
    let (events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 100, None);
    assert!(events.is_empty());

    // Insert a single event
    let event = make_forwarding_event(1000, 5);
    store.insert_forwarding_event(event.clone());

    let (events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], event);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_time_range() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Insert events at different timestamps
    let e1 = make_forwarding_event(100, 1);
    let e2 = make_forwarding_event(200, 2);
    let e3 = make_forwarding_event(300, 3);
    let e4 = make_forwarding_event(400, 4);

    store.insert_forwarding_event(e1.clone());
    store.insert_forwarding_event(e2.clone());
    store.insert_forwarding_event(e3.clone());
    store.insert_forwarding_event(e4.clone());

    // Query all
    let (events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 4);

    // Query with start_time filter (inclusive)
    let (events, _) = query_all_forwarding_events(&store, 200, u64::MAX, 100, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], e2);
    assert_eq!(events[1], e3);
    assert_eq!(events[2], e4);

    // Query with end_time filter (inclusive)
    let (events, _) = query_all_forwarding_events(&store, 0, 300, 100, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], e1);
    assert_eq!(events[1], e2);
    assert_eq!(events[2], e3);

    // Query narrow range
    let (events, _) = query_all_forwarding_events(&store, 200, 300, 100, None);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], e2);
    assert_eq!(events[1], e3);

    // Query range that matches nothing
    let (events, _) = query_all_forwarding_events(&store, 500, 600, 100, None);
    assert!(events.is_empty());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_pagination() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..10 {
        store.insert_forwarding_event(make_forwarding_event(100 + i, i as u128));
    }

    // First page (limit 3, no cursor)
    let (events, cursor) = query_all_forwarding_events(&store, 0, u64::MAX, 3, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].timestamp, 100);
    assert_eq!(events[2].timestamp, 102);
    assert!(cursor.is_some());

    // Second page (limit 3, using cursor from first page)
    let (events, cursor2) = query_all_forwarding_events(&store, 0, u64::MAX, 3, cursor);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].timestamp, 103);
    assert_eq!(events[2].timestamp, 105);
    assert!(cursor2.is_some());

    // Skip to page starting at timestamp 108 (using cursor from second-to-last page)
    // Fetch page at offset 8 by chaining cursors
    let (events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 3, cursor2);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].timestamp, 106);
    assert_eq!(events[2].timestamp, 108);

    // Last page: limit larger than remaining
    let (events_first3, cursor_at_106) = query_all_forwarding_events(&store, 0, u64::MAX, 3, None);
    // Advance to just before timestamp 108 to test "limit larger than remaining"
    let _ = events_first3;
    // Directly test: fetch starting from offset 8 by seeking to just before it
    let (events_page1, c1) = query_all_forwarding_events(&store, 0, u64::MAX, 8, None);
    let _ = events_page1;
    let (events, last_cursor) = query_all_forwarding_events(&store, 0, u64::MAX, 100, c1);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].timestamp, 108);
    assert_eq!(events[1].timestamp, 109);
    assert!(last_cursor.is_none()); // no more data
    let _ = cursor_at_106;
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_ordering() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Insert out of order — store should still return sorted by timestamp
    // because keys use big-endian timestamp
    let e3 = make_forwarding_event(300, 3);
    let e1 = make_forwarding_event(100, 1);
    let e2 = make_forwarding_event(200, 2);

    store.insert_forwarding_event(e3.clone());
    store.insert_forwarding_event(e1.clone());
    store.insert_forwarding_event(e2.clone());

    let (events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].timestamp, 100);
    assert_eq!(events[1].timestamp, 200);
    assert_eq!(events[2].timestamp, 300);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_same_timestamp_different_hash() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Two events at the same timestamp but different payment_hash
    // (the key includes payment_hash for uniqueness)
    let mut e1 = make_forwarding_event(100, 5);
    let mut e2 = make_forwarding_event(100, 10);
    // Ensure different payment hashes (already random, but be explicit)
    e1.payment_hash = gen_rand_sha256_hash();
    e2.payment_hash = gen_rand_sha256_hash();

    store.insert_forwarding_event(e1.clone());
    store.insert_forwarding_event(e2.clone());

    let (events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 2);
    // Both should be present (order within same timestamp depends on payment_hash bytes)
    assert!(events.contains(&e1));
    assert!(events.contains(&e2));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_with_udt() {
    use crate::fiber::channel::PaymentEventStore;
    use ckb_types::packed::ScriptBuilder;
    use ckb_types::prelude::*;

    let (store, _dir) = generate_store();

    let udt_script = ScriptBuilder::default()
        .code_hash(ckb_types::packed::Byte32::new([0xab; 32]))
        .hash_type(ckb_types::core::ScriptHashType::Data)
        .build();

    // CKB event
    let ckb_event = make_forwarding_event(100, 5);

    // UDT event
    let udt_event = fiber_types::ForwardingEvent {
        timestamp: 200,
        incoming_channel_id: gen_rand_sha256_hash(),
        outgoing_channel_id: gen_rand_sha256_hash(),
        incoming_amount: 5000,
        outgoing_amount: 4900,
        fee: 100,
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script: Some(udt_script.clone()),
    };

    store.insert_forwarding_event(ckb_event.clone());
    store.insert_forwarding_event(udt_event.clone());

    let (events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 2);

    // Verify CKB event round-trips correctly
    let ckb = events.iter().find(|e| e.udt_type_script.is_none()).unwrap();
    assert_eq!(ckb, &ckb_event);

    // Verify UDT event round-trips correctly with script preserved
    let udt = events.iter().find(|e| e.udt_type_script.is_some()).unwrap();
    assert_eq!(udt, &udt_event);
    assert_eq!(udt.udt_type_script.as_ref().unwrap(), &udt_script);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_query_forwarding_events_by_asset() {
    use crate::fiber::channel::{AssetSelector, ForwardingHistoryQuery, PaymentEventStore};
    use ckb_types::packed::ScriptBuilder;
    use ckb_types::prelude::*;

    let (store, _dir) = generate_store();

    let udt_script = ScriptBuilder::default()
        .code_hash(ckb_types::packed::Byte32::new([0xee; 32]))
        .hash_type(ckb_types::core::ScriptHashType::Data)
        .build();

    let ckb_event = make_forwarding_event(100, 5);
    let udt_event = fiber_types::ForwardingEvent {
        timestamp: 200,
        incoming_channel_id: gen_rand_sha256_hash(),
        outgoing_channel_id: gen_rand_sha256_hash(),
        incoming_amount: 5000,
        outgoing_amount: 4900,
        fee: 100,
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script: Some(udt_script.clone()),
    };

    store.insert_forwarding_event(ckb_event.clone());
    store.insert_forwarding_event(udt_event.clone());

    let (ckb_events, _) = store
        .query_forwarding_events(ForwardingHistoryQuery {
            asset: AssetSelector::Ckb,
            start_time: 0,
            end_time: u64::MAX,
            limit: 100,
            after: None,
        })
        .unwrap();
    assert_eq!(ckb_events, vec![ckb_event.clone()]);

    let (udt_events, _) = store
        .query_forwarding_events(ForwardingHistoryQuery {
            asset: AssetSelector::Udt(udt_script.clone()),
            start_time: 0,
            end_time: u64::MAX,
            limit: 100,
            after: None,
        })
        .unwrap();
    assert_eq!(udt_events, vec![udt_event]);
}

// ─── PaymentEvent store tests ───────────────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
fn make_payment_event(
    timestamp: u64,
    amount: u128,
    event_type: fiber_types::PaymentEventType,
) -> fiber_types::PaymentEvent {
    fiber_types::PaymentEvent {
        event_type,
        timestamp,
        channel_id: gen_rand_sha256_hash(),
        amount,
        fee: if matches!(event_type, fiber_types::PaymentEventType::Send) {
            10
        } else {
            0
        },
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script: None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_insert_and_query() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Initially empty
    let (events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert!(events.is_empty());

    // Insert a single send event
    let event = make_payment_event(1000, 500, fiber_types::PaymentEventType::Send);
    store.insert_payment_event(event.clone());

    let (events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], event);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_time_range() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    let e1 = make_payment_event(100, 100, fiber_types::PaymentEventType::Send);
    let e2 = make_payment_event(200, 200, fiber_types::PaymentEventType::Receive);
    let e3 = make_payment_event(300, 300, fiber_types::PaymentEventType::Send);
    let e4 = make_payment_event(400, 400, fiber_types::PaymentEventType::Receive);

    store.insert_payment_event(e1.clone());
    store.insert_payment_event(e2.clone());
    store.insert_payment_event(e3.clone());
    store.insert_payment_event(e4.clone());

    // Query all
    let (events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 4);

    // Query with start_time filter (inclusive)
    let (events, _) = query_all_payment_events(&store, 200, u64::MAX, 100, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], e2);
    assert_eq!(events[1], e3);
    assert_eq!(events[2], e4);

    // Query with end_time filter (inclusive)
    let (events, _) = query_all_payment_events(&store, 0, 300, 100, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], e1);
    assert_eq!(events[1], e2);
    assert_eq!(events[2], e3);

    // Query narrow range
    let (events, _) = query_all_payment_events(&store, 200, 300, 100, None);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], e2);
    assert_eq!(events[1], e3);

    // Query range that matches nothing
    let (events, _) = query_all_payment_events(&store, 500, 600, 100, None);
    assert!(events.is_empty());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_pagination() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..10 {
        let event_type = if i % 2 == 0 {
            fiber_types::PaymentEventType::Send
        } else {
            fiber_types::PaymentEventType::Receive
        };
        store.insert_payment_event(make_payment_event(100 + i, i as u128 * 100, event_type));
    }

    // First page (limit 3, no cursor)
    let (events, cursor) = query_all_payment_events(&store, 0, u64::MAX, 3, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].timestamp, 100);
    assert_eq!(events[2].timestamp, 102);
    assert!(cursor.is_some());

    // Second page (limit 3, using cursor from first page)
    let (events, cursor2) = query_all_payment_events(&store, 0, u64::MAX, 3, cursor);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].timestamp, 103);
    assert_eq!(events[2].timestamp, 105);
    assert!(cursor2.is_some());

    // Last page: fetch remaining 2 events after all 8 (using cursor after 8th)
    let (_, c8) = query_all_payment_events(&store, 0, u64::MAX, 8, None);
    let (events, last_cursor) = query_all_payment_events(&store, 0, u64::MAX, 100, c8);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].timestamp, 108);
    assert_eq!(events[1].timestamp, 109);
    assert!(last_cursor.is_none()); // no more data
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_ordering() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Insert out of order — store should still return sorted by timestamp
    let e3 = make_payment_event(300, 300, fiber_types::PaymentEventType::Send);
    let e1 = make_payment_event(100, 100, fiber_types::PaymentEventType::Receive);
    let e2 = make_payment_event(200, 200, fiber_types::PaymentEventType::Send);

    store.insert_payment_event(e3.clone());
    store.insert_payment_event(e1.clone());
    store.insert_payment_event(e2.clone());

    let (events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].timestamp, 100);
    assert_eq!(events[1].timestamp, 200);
    assert_eq!(events[2].timestamp, 300);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_same_timestamp_different_hash() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Two events at the same timestamp but different payment_hash
    let mut e1 = make_payment_event(100, 500, fiber_types::PaymentEventType::Send);
    let mut e2 = make_payment_event(100, 600, fiber_types::PaymentEventType::Receive);
    e1.payment_hash = gen_rand_sha256_hash();
    e2.payment_hash = gen_rand_sha256_hash();

    store.insert_payment_event(e1.clone());
    store.insert_payment_event(e2.clone());

    let (events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 2);
    assert!(events.contains(&e1));
    assert!(events.contains(&e2));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_with_udt() {
    use crate::fiber::channel::PaymentEventStore;
    use ckb_types::packed::ScriptBuilder;
    use ckb_types::prelude::*;

    let (store, _dir) = generate_store();

    let udt_script = ScriptBuilder::default()
        .code_hash(ckb_types::packed::Byte32::new([0xcd; 32]))
        .hash_type(ckb_types::core::ScriptHashType::Data)
        .build();

    // CKB payment event
    let ckb_event = make_payment_event(100, 500, fiber_types::PaymentEventType::Send);

    // UDT payment event
    let udt_event = fiber_types::PaymentEvent {
        event_type: fiber_types::PaymentEventType::Receive,
        timestamp: 200,
        channel_id: gen_rand_sha256_hash(),
        amount: 5000,
        fee: 0,
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script: Some(udt_script.clone()),
    };

    store.insert_payment_event(ckb_event.clone());
    store.insert_payment_event(udt_event.clone());

    let (events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 2);

    // Verify CKB event round-trips correctly
    let ckb = events.iter().find(|e| e.udt_type_script.is_none()).unwrap();
    assert_eq!(ckb, &ckb_event);

    // Verify UDT event round-trips correctly with script preserved
    let udt = events.iter().find(|e| e.udt_type_script.is_some()).unwrap();
    assert_eq!(udt, &udt_event);
    assert_eq!(udt.udt_type_script.as_ref().unwrap(), &udt_script);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_query_payment_events_by_asset_and_type() {
    use crate::fiber::channel::{AssetSelector, PaymentEventStore, PaymentHistoryQuery};
    use ckb_types::packed::ScriptBuilder;
    use ckb_types::prelude::*;

    let (store, _dir) = generate_store();

    let udt_script = ScriptBuilder::default()
        .code_hash(ckb_types::packed::Byte32::new([0xdc; 32]))
        .hash_type(ckb_types::core::ScriptHashType::Data)
        .build();

    let send_ckb = make_payment_event(100, 500, fiber_types::PaymentEventType::Send);
    let recv_udt = fiber_types::PaymentEvent {
        event_type: fiber_types::PaymentEventType::Receive,
        timestamp: 200,
        channel_id: gen_rand_sha256_hash(),
        amount: 5000,
        fee: 0,
        payment_hash: gen_rand_sha256_hash(),
        udt_type_script: Some(udt_script.clone()),
    };

    store.insert_payment_event(send_ckb.clone());
    store.insert_payment_event(recv_udt.clone());

    let (recv_events, _) = store
        .query_payment_events(PaymentHistoryQuery {
            asset: AssetSelector::All,
            event_type: Some(fiber_types::PaymentEventType::Receive),
            start_time: 0,
            end_time: u64::MAX,
            limit: 100,
            after: None,
        })
        .unwrap();
    assert_eq!(recv_events, vec![recv_udt.clone()]);

    let (udt_events, _) = store
        .query_payment_events(PaymentHistoryQuery {
            asset: AssetSelector::Udt(udt_script.clone()),
            event_type: Some(fiber_types::PaymentEventType::Receive),
            start_time: 0,
            end_time: u64::MAX,
            limit: 100,
            after: None,
        })
        .unwrap();
    assert_eq!(udt_events, vec![recv_udt]);

    let (ckb_events, _) = store
        .query_payment_events(PaymentHistoryQuery {
            asset: AssetSelector::Ckb,
            event_type: Some(fiber_types::PaymentEventType::Send),
            start_time: 0,
            end_time: u64::MAX,
            limit: 100,
            after: None,
        })
        .unwrap();
    assert_eq!(ckb_events, vec![send_ckb]);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_mixed_types() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Insert both send and receive events
    let send_event = make_payment_event(100, 1000, fiber_types::PaymentEventType::Send);
    let recv_event = make_payment_event(200, 2000, fiber_types::PaymentEventType::Receive);

    store.insert_payment_event(send_event.clone());
    store.insert_payment_event(recv_event.clone());

    let (events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(events.len(), 2);

    // get_payment_events returns all types; filtering is done at the RPC layer
    let send = events
        .iter()
        .find(|e| e.event_type == fiber_types::PaymentEventType::Send)
        .unwrap();
    assert_eq!(send.amount, 1000);
    assert_eq!(send.fee, 10); // Send events have fee

    let recv = events
        .iter()
        .find(|e| e.event_type == fiber_types::PaymentEventType::Receive)
        .unwrap();
    assert_eq!(recv.amount, 2000);
    assert_eq!(recv.fee, 0); // Receive events have 0 fee
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_independent_of_forwarding_events() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    // Insert a forwarding event
    let fwd = make_forwarding_event(100, 5);
    store.insert_forwarding_event(fwd);

    // Insert a payment event
    let pay = make_payment_event(100, 500, fiber_types::PaymentEventType::Send);
    store.insert_payment_event(pay.clone());

    // They should be in separate namespaces
    let (fwd_events, _) = query_all_forwarding_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(fwd_events.len(), 1);

    let (pay_events, _) = query_all_payment_events(&store, 0, u64::MAX, 100, None);
    assert_eq!(pay_events.len(), 1);
    assert_eq!(pay_events[0], pay);
}

// ── Forwarding event cursor-based pagination edge cases ──────────────────────

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_cursor_is_exclusive() {
    // The cursor returned by page N must be excluded from page N+1.
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..5u64 {
        store.insert_forwarding_event(make_forwarding_event(100 + i, i as u128));
    }

    let (page1, cursor1) = query_all_forwarding_events(&store, 0, u64::MAX, 2, None);
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].timestamp, 100);
    assert_eq!(page1[1].timestamp, 101);
    assert!(cursor1.is_some());

    let (page2, cursor2) = query_all_forwarding_events(&store, 0, u64::MAX, 2, cursor1);
    assert_eq!(page2.len(), 2);
    // Must NOT contain the cursor item (timestamp 101)
    assert_eq!(page2[0].timestamp, 102);
    assert_eq!(page2[1].timestamp, 103);
    assert!(cursor2.is_some());

    let (page3, cursor3) = query_all_forwarding_events(&store, 0, u64::MAX, 2, cursor2);
    assert_eq!(page3.len(), 1);
    assert_eq!(page3[0].timestamp, 104);
    // Last page: fewer results than limit → no cursor
    assert!(cursor3.is_none());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_cursor_with_end_time() {
    // end_time filter must still apply when cursor is given.
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..6u64 {
        store.insert_forwarding_event(make_forwarding_event(100 + i, i as u128));
    }

    // First page: limit 2, end_time 103 (only timestamps 100-103 qualify)
    let (page1, cursor1) = query_all_forwarding_events(&store, 0, 103, 2, None);
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].timestamp, 100);
    assert_eq!(page1[1].timestamp, 101);
    assert!(cursor1.is_some());

    // Second page using cursor — should only see 102..=103, not 104+
    // page2 exactly exhausts the window so cursor must be None (no spurious extra round-trip).
    let (page2, cursor2) = query_all_forwarding_events(&store, 0, 103, 2, cursor1);
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].timestamp, 102);
    assert_eq!(page2[1].timestamp, 103);
    assert!(cursor2.is_none());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_limit1_full_scan() {
    // Walk through all events one-by-one using limit=1 to verify each cursor
    // advances exactly one position.
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    let n = 5u64;
    for i in 0..n {
        store.insert_forwarding_event(make_forwarding_event(100 + i, i as u128));
    }

    let mut cursor = None;
    for expected_ts in 100..100 + n {
        let (events, next_cursor) = query_all_forwarding_events(&store, 0, u64::MAX, 1, cursor);
        assert_eq!(events.len(), 1, "expected 1 event at ts={expected_ts}");
        assert_eq!(events[0].timestamp, expected_ts);
        if expected_ts < 100 + n - 1 {
            assert!(next_cursor.is_some());
        } else {
            assert!(next_cursor.is_none());
        }
        cursor = next_cursor;
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_forwarding_event_cursor_exhausted_returns_no_cursor() {
    // When the page exactly covers the last items, last_cursor must be None.
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..4u64 {
        store.insert_forwarding_event(make_forwarding_event(100 + i, i as u128));
    }

    // Fetch exactly 4 events in one page
    let (events, cursor) = query_all_forwarding_events(&store, 0, u64::MAX, 4, None);
    assert_eq!(events.len(), 4);
    assert!(cursor.is_none()); // no remainder

    // Fetch 4 events two at a time; second page also ends exactly
    let (_, c1) = query_all_forwarding_events(&store, 0, u64::MAX, 2, None);
    let (events2, cursor2) = query_all_forwarding_events(&store, 0, u64::MAX, 2, c1);
    assert_eq!(events2.len(), 2);
    assert!(cursor2.is_none());
}

// ── Payment event cursor-based pagination edge cases ─────────────────────────

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_cursor_is_exclusive() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..5u64 {
        store.insert_payment_event(make_payment_event(
            100 + i,
            1000,
            fiber_types::PaymentEventType::Send,
        ));
    }

    let (page1, cursor1) = query_all_payment_events(&store, 0, u64::MAX, 2, None);
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].timestamp, 100);
    assert_eq!(page1[1].timestamp, 101);
    assert!(cursor1.is_some());

    let (page2, cursor2) = query_all_payment_events(&store, 0, u64::MAX, 2, cursor1);
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].timestamp, 102);
    assert_eq!(page2[1].timestamp, 103);
    assert!(cursor2.is_some());

    let (page3, cursor3) = query_all_payment_events(&store, 0, u64::MAX, 2, cursor2);
    assert_eq!(page3.len(), 1);
    assert_eq!(page3[0].timestamp, 104);
    assert!(cursor3.is_none());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_cursor_with_end_time() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..6u64 {
        store.insert_payment_event(make_payment_event(
            100 + i,
            1000,
            fiber_types::PaymentEventType::Receive,
        ));
    }

    let (page1, cursor1) = query_all_payment_events(&store, 0, 103, 2, None);
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].timestamp, 100);
    assert_eq!(page1[1].timestamp, 101);
    assert!(cursor1.is_some());

    let (page2, cursor2) = query_all_payment_events(&store, 0, 103, 2, cursor1);
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].timestamp, 102);
    assert_eq!(page2[1].timestamp, 103);
    // page2 exactly exhausts the window so cursor must be None (no spurious extra round-trip).
    assert!(cursor2.is_none());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_limit1_full_scan() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    let n = 5u64;
    for i in 0..n {
        store.insert_payment_event(make_payment_event(
            100 + i,
            500,
            fiber_types::PaymentEventType::Send,
        ));
    }

    let mut cursor = None;
    for expected_ts in 100..100 + n {
        let (events, next_cursor) = query_all_payment_events(&store, 0, u64::MAX, 1, cursor);
        assert_eq!(events.len(), 1, "expected 1 event at ts={expected_ts}");
        assert_eq!(events[0].timestamp, expected_ts);
        if expected_ts < 100 + n - 1 {
            assert!(next_cursor.is_some());
        } else {
            assert!(next_cursor.is_none());
        }
        cursor = next_cursor;
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_payment_event_cursor_exhausted_returns_no_cursor() {
    use crate::fiber::channel::PaymentEventStore;

    let (store, _dir) = generate_store();

    for i in 0..4u64 {
        store.insert_payment_event(make_payment_event(
            100 + i,
            1000,
            fiber_types::PaymentEventType::Send,
        ));
    }

    let (events, cursor) = query_all_payment_events(&store, 0, u64::MAX, 4, None);
    assert_eq!(events.len(), 4);
    assert!(cursor.is_none());

    let (_, c1) = query_all_payment_events(&store, 0, u64::MAX, 2, None);
    let (events2, cursor2) = query_all_payment_events(&store, 0, u64::MAX, 2, c1);
    assert_eq!(events2.len(), 2);
    assert!(cursor2.is_none());
}
