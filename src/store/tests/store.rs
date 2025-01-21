use crate::fiber::channel::*;
use crate::fiber::config::AnnouncedNodeName;
use crate::fiber::config::DEFAULT_TLC_EXPIRY_DELTA;
use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::graph::*;
use crate::fiber::history::Direction;
use crate::fiber::history::TimedResult;
use crate::fiber::network::SendPaymentData;
use crate::fiber::tests::test_utils::*;
use crate::fiber::types::*;
use crate::gen_rand_fiber_private_key;
use crate::gen_rand_fiber_public_key;
use crate::gen_rand_sha256_hash;
use crate::invoice::*;
use crate::now_timestamp_as_millis_u64;
use crate::store::store::deserialize_from;
use crate::store::store::serialize_to_vec;
use crate::store::Store;
use crate::watchtower::*;
use ckb_hash::new_blake2b;
use ckb_types::packed::*;
use ckb_types::prelude::*;
use ckb_types::H256;
use core::cmp::Ordering;
use musig2::secp::MaybeScalar;
use musig2::CompactSignature;
use musig2::SecNonce;
use secp256k1::SecretKey;
use secp256k1::{Keypair, Secp256k1};
use std::time::SystemTime;

fn gen_rand_key_pair() -> Keypair {
    let secp = Secp256k1::new();
    Keypair::new(&secp, &mut rand::thread_rng())
}

fn gen_rand_private_key() -> SecretKey {
    gen_rand_key_pair().secret_key()
}

fn mock_node() -> (Privkey, NodeAnnouncement) {
    let sk: Privkey = gen_rand_private_key().into();
    (
        sk.clone(),
        NodeAnnouncement::new(
            AnnouncedNodeName::from_str("node1").expect("invalid name"),
            vec![],
            &sk,
            now_timestamp_as_millis_u64(),
            0,
        ),
    )
}

fn mock_channel() -> ChannelAnnouncement {
    let sk1: Privkey = gen_rand_private_key().into();
    let sk2: Privkey = gen_rand_private_key().into();
    let keypair = gen_rand_key_pair();
    let (xonly, _parity) = keypair.x_only_public_key();
    let rand_hash256 = gen_rand_sha256_hash();
    ChannelAnnouncement::new_unsigned(
        &sk1.pubkey(),
        &sk2.pubkey(),
        OutPoint::new_builder()
            .tx_hash(rand_hash256.into())
            .index(0u32.pack())
            .build(),
        &xonly,
        0,
        None,
    )
}

#[test]
fn test_store_invoice() {
    let path = TempDir::new("invoice_store");

    let store = Store::new(path).expect("created store failed");

    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_preimage(preimage)
        .fallback_address("address".to_string())
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .build()
        .unwrap();

    let hash = invoice.payment_hash();
    store
        .insert_invoice(invoice.clone(), Some(preimage))
        .unwrap();
    assert_eq!(store.get_invoice(hash), Some(invoice.clone()));
    assert_eq!(store.get_invoice_preimage(hash), Some(preimage));

    let invalid_hash = gen_rand_sha256_hash();
    assert_eq!(store.get_invoice_preimage(&invalid_hash), None);

    assert_eq!(store.get_invoice_status(hash), Some(CkbInvoiceStatus::Open));
    assert_eq!(store.get_invoice_status(&gen_rand_sha256_hash()), None);

    let status = CkbInvoiceStatus::Paid;
    store.update_invoice_status(hash, status).unwrap();
    assert_eq!(store.get_invoice_status(hash), Some(status));
}

#[test]
fn test_store_get_broadcast_messages_iter() {
    let path = TempDir::new("test-gossip-store");
    let store = Store::new(path).expect("created store failed");

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

#[test]
fn test_store_get_broadcast_messages() {
    let path = TempDir::new("test-gossip-store");
    let store = Store::new(path).expect("created store failed");

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

#[test]
fn test_store_save_channel_announcement() {
    let path = TempDir::new("test-gossip-store");
    let store = Store::new(path).expect("created store failed");

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

#[test]
fn test_store_save_channel_update() {
    let path = TempDir::new("test-gossip-store");
    let store = Store::new(path).expect("created store failed");

    let flags_for_update_of_node1 = ChannelUpdateMessageFlags::UPDATE_OF_NODE1;
    let channel_update_of_node1 = ChannelUpdate::new_unsigned(
        OutPoint::new_builder()
            .tx_hash(gen_rand_sha256_hash().into())
            .index(0u32.pack())
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

#[test]
fn test_store_save_node_announcement() {
    let path = TempDir::new("test-gossip-store");
    let store = Store::new(path).expect("created store failed");

    let (sk, node_announcement) = mock_node();
    let pk = sk.pubkey();
    store.save_node_announcement(node_announcement.clone());
    let new_node_announcement = store.get_latest_node_announcement(&pk);
    assert_eq!(new_node_announcement, Some(node_announcement));
}

#[test]
fn test_store_wacthtower() {
    let path = TempDir::new("test-watchtower-store");
    let store = Store::new(path).expect("created store failed");

    let channel_id = gen_rand_sha256_hash();
    let funding_tx_lock = Script::default();

    let settlement_data = SettlementData {
        x_only_aggregated_pubkey: [0u8; 32],
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        to_local_output: CellOutput::default(),
        to_local_output_data: Bytes::default(),
        to_remote_output: CellOutput::default(),
        to_remote_output_data: Bytes::default(),
    };

    store.insert_watch_channel(channel_id, funding_tx_lock.clone(), settlement_data.clone());
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_tx_lock: funding_tx_lock.clone(),
            revocation_data: None,
            local_settlement_data: None,
            remote_settlement_data: settlement_data.clone(),
        }]
    );

    let revocation_data = RevocationData {
        commitment_number: 0,
        x_only_aggregated_pubkey: [0u8; 32],
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        output: CellOutput::default(),
        output_data: Bytes::default(),
    };

    store.update_revocation(channel_id, revocation_data.clone(), settlement_data.clone());
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_tx_lock,
            local_settlement_data: None,
            revocation_data: Some(revocation_data),
            remote_settlement_data: settlement_data,
        }]
    );

    store.remove_watch_channel(channel_id);
    assert_eq!(store.get_watch_channels(), vec![]);
}

#[test]
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

fn blake2b_hash_with_salt(data: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(salt);
    hasher.update(data);
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);
    result
}

#[test]
fn test_channel_actor_state_store() {
    let seed = [0u8; 32];
    let signer = InMemorySigner::generate_from_seed(&seed);

    let seckey = blake2b_hash_with_salt(
        signer.musig2_base_nonce.as_ref(),
        b"channel_announcement".as_slice(),
    );
    let sec_nonce = SecNonce::build(seckey).build();
    let pub_nonce = sec_nonce.public_nonce();

    let state = ChannelActorState {
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
            tlc_maximum_value: 0,
        },
        remote_tlc_info: None,
        local_pubkey: gen_rand_fiber_public_key(),
        remote_pubkey: gen_rand_fiber_public_key(),
        funding_tx: Some(Transaction::default()),
        funding_tx_confirmed_at: Some((H256::default(), 1, 1)),
        is_acceptor: true,
        funding_udt_type_script: Some(Script::default()),
        to_local_amount: 100,
        to_remote_amount: 100,
        commitment_fee_rate: 100,
        commitment_delay_epoch: 100,
        funding_fee_rate: 100,
        id: gen_rand_sha256_hash(),
        tlc_state: Default::default(),
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
        last_revoke_and_ack_remote_nonce: None,
        last_commitment_signed_remote_nonce: None,
        remote_commitment_points: vec![
            (0, gen_rand_fiber_public_key()),
            (1, gen_rand_fiber_public_key()),
        ],
        local_shutdown_info: None,
        remote_shutdown_info: None,
        local_reserved_ckb_amount: 100,
        remote_reserved_ckb_amount: 100,
        latest_commitment_transaction: None,
        local_constraints: ChannelConstraints::default(),
        remote_constraints: ChannelConstraints::default(),
        reestablishing: false,
        created_at: SystemTime::now(),
    };

    let bincode_encoded = bincode::serialize(&state).unwrap();
    let _new_state: ChannelActorState = bincode::deserialize(&bincode_encoded).unwrap();

    let path = TempDir::new("channel_actore_store");

    let store = Store::new(path).expect("create store failed");
    assert!(store.get_channel_actor_state(&state.id).is_none());
    store.insert_channel_actor_state(state.clone());

    let get_state = store.get_channel_actor_state(&state.id);
    assert!(get_state.is_some());
    assert_eq!(get_state.unwrap().is_tlc_forwarding_enabled(), false);

    let remote_peer_id = state.get_remote_peer_id();
    assert_eq!(
        store.get_channel_ids_by_peer(&remote_peer_id),
        vec![state.id.clone()]
    );
    let channel_point = state.must_get_funding_transaction_outpoint();
    assert!(store
        .get_channel_state_by_outpoint(&channel_point)
        .is_some());

    store.delete_channel_actor_state(&state.id);
    assert!(store.get_channel_actor_state(&state.id).is_none());
    assert_eq!(store.get_channel_ids_by_peer(&remote_peer_id), vec![]);
    let channel_point = state.must_get_funding_transaction_outpoint();
    assert!(store
        .get_channel_state_by_outpoint(&channel_point)
        .is_none());
}

#[test]
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
            tlc_maximum_value: 0,
        },
        remote_tlc_info: None,
        local_pubkey: gen_rand_fiber_public_key(),
        remote_pubkey: gen_rand_fiber_public_key(),
        funding_tx: Some(Transaction::default()),
        funding_tx_confirmed_at: Some((H256::default(), 1, 1)),
        is_acceptor: true,
        funding_udt_type_script: Some(Script::default()),
        to_local_amount: 100,
        to_remote_amount: 100,
        commitment_fee_rate: 100,
        commitment_delay_epoch: 100,
        funding_fee_rate: 100,
        id: gen_rand_sha256_hash(),
        tlc_state: Default::default(),
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
        last_revoke_and_ack_remote_nonce: None,
        last_commitment_signed_remote_nonce: None,
        remote_commitment_points: vec![
            (0, gen_rand_fiber_public_key()),
            (1, gen_rand_fiber_public_key()),
        ],
        local_shutdown_info: None,
        remote_shutdown_info: None,
        local_reserved_ckb_amount: 100,
        remote_reserved_ckb_amount: 100,
        latest_commitment_transaction: None,
        local_constraints: ChannelConstraints::default(),
        remote_constraints: ChannelConstraints::default(),
        reestablishing: false,
        created_at: SystemTime::now(),
    };

    let mut serialized = Vec::new();
    ciborium::into_writer(&state, &mut serialized).unwrap();
    let _new_channel_state: ChannelActorState =
        ciborium::from_reader(serialized.as_slice()).expect("deserialize to new state");
}

#[test]
fn test_store_payment_session() {
    let path = TempDir::new("payment-history-store-test");
    let store = Store::new(path).expect("created store failed");
    let payment_hash = gen_rand_sha256_hash();
    let payment_data = SendPaymentData {
        target_pubkey: gen_rand_fiber_public_key(),
        amount: 100,
        payment_hash,
        invoice: None,
        final_tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
        tlc_expiry_limit: MAX_PAYMENT_TLC_EXPIRY_LIMIT,
        timeout: Some(10),
        max_fee_amount: Some(1000),
        max_parts: None,
        keysend: false,
        udt_type_script: None,
        preimage: None,
        allow_self_payment: false,
        hop_hints: vec![],
        dry_run: false,
    };
    let payment_session = PaymentSession::new(payment_data.clone(), 10);
    store.insert_payment_session(payment_session.clone());
    let res = store.get_payment_session(payment_hash).unwrap();
    assert_eq!(res.payment_hash(), payment_hash);
    assert_eq!(res.request.max_fee_amount, Some(1000));
    assert_eq!(res.status, PaymentSessionStatus::Created);
}

#[test]
fn test_store_payment_history() {
    let mut store = generate_store();
    let result = TimedResult {
        fail_amount: 1,
        fail_time: 2,
        success_time: 3,
        success_amount: 4,
    };
    let channel_outpoint = OutPoint::default();
    let direction = Direction::Forward;
    store.insert_payment_history_result(channel_outpoint.clone(), direction, result.clone());
    assert_eq!(
        store.get_payment_history_results(),
        vec![(channel_outpoint.clone(), direction, result)]
    );

    fn sort_results(results: &mut Vec<(OutPoint, Direction, TimedResult)>) {
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
    store.insert_payment_history_result(channel_outpoint.clone(), direction_2, result_2.clone());
    let mut r1 = store.get_payment_history_results();
    sort_results(&mut r1);
    let mut r2: Vec<(OutPoint, Direction, TimedResult)> = vec![
        (channel_outpoint.clone(), direction, result),
        (channel_outpoint.clone(), direction_2, result_2),
    ];
    sort_results(&mut r2);
    assert_eq!(r1, r2);

    let outpoint_3 = OutPoint::new_builder()
        .tx_hash(gen_rand_sha256_hash().into())
        .index(1u32.pack())
        .build();
    let direction_3 = Direction::Forward;
    let result_3 = TimedResult {
        fail_amount: 3,
        fail_time: 4,
        success_time: 5,
        success_amount: 6,
    };

    store.insert_payment_history_result(outpoint_3.clone(), direction_3, result_3.clone());
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

#[test]
fn test_serde_node_announcement_as_broadcast_message() {
    let privkey = gen_rand_fiber_private_key();
    let node_announcement = NodeAnnouncement::new(
        AnnouncedNodeName::from_str("node1").expect("valid name"),
        vec![],
        &privkey,
        now_timestamp_as_millis_u64(),
        0,
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
