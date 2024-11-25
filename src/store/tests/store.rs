use crate::fiber::channel::*;
use crate::fiber::config::AnnouncedNodeName;
use crate::fiber::config::DEFAULT_TLC_EXPIRY_DELTA;
use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::graph::*;
use crate::fiber::history::TimedResult;
use crate::fiber::network::SendPaymentData;
use crate::fiber::tests::test_utils::*;
use crate::fiber::types::*;
use crate::invoice::*;
use crate::store::schema::*;
use crate::store::Store;
use crate::watchtower::*;
use ckb_hash::new_blake2b;
use ckb_jsonrpc_types::JsonBytes;
use ckb_types::packed::*;
use ckb_types::prelude::*;
use core::cmp::Ordering;
use musig2::secp::MaybeScalar;
use musig2::CompactSignature;
use musig2::SecNonce;
use secp256k1::{Keypair, Secp256k1};
use std::time::SystemTime;
use tempfile::tempdir;

fn mock_node() -> (Pubkey, NodeInfo) {
    let node_id: Pubkey = gen_rand_public_key();
    let node = NodeInfo {
        node_id,
        anouncement_msg: NodeAnnouncement::new_unsigned(
            AnnouncedNodeName::from_str("node1").expect("invalid name"),
            vec![],
            node_id,
            1,
            0,
        ),
        timestamp: 0,
    };
    (node_id, node)
}

fn mock_channel() -> ChannelInfo {
    let node1: Pubkey = gen_rand_public_key();
    let node2: Pubkey = gen_rand_public_key();
    let secp = Secp256k1::new();
    let keypair = Keypair::new(&secp, &mut rand::thread_rng());
    let (xonly, _parity) = keypair.x_only_public_key();
    let rand_hash256 = gen_sha256_hash();
    ChannelInfo {
        funding_tx_block_number: 0,
        funding_tx_index: 0,
        timestamp: 0,
        node1_to_node2: None,
        node2_to_node1: None,
        announcement_msg: ChannelAnnouncement::new_unsigned(
            &node1,
            &node2,
            OutPoint::new_builder()
                .tx_hash(rand_hash256.into())
                .index(0u32.pack())
                .build(),
            Hash256::default(),
            &xonly,
            0,
            None,
        ),
    }
}

#[test]
fn test_store_invoice() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invoice_store");
    let store = Store::new(path).expect("created store failed");

    let preimage = gen_sha256_hash();
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

    let invalid_hash = gen_sha256_hash();
    assert_eq!(store.get_invoice_preimage(&invalid_hash), None);

    assert_eq!(store.get_invoice_status(hash), Some(CkbInvoiceStatus::Open));
    assert_eq!(store.get_invoice_status(&gen_sha256_hash()), None);

    let status = CkbInvoiceStatus::Paid;
    store.update_invoice_status(hash, status).unwrap();
    assert_eq!(store.get_invoice_status(hash), Some(status));
}

#[test]
fn test_store_channels() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invoice_store");
    let store = Store::new(path).expect("created store failed");

    let mut channels = vec![];
    for _ in 0..10 {
        let channel = mock_channel();
        store.insert_channel(channel.clone());
        channels.push(channel);
    }

    // sort by out_point
    channels.sort_by_key(|a| a.out_point());

    let outpoint_0 = channels[0].out_point();
    assert_eq!(
        store.get_channels(Some(outpoint_0)),
        vec![channels[0].clone()]
    );
    let (res, last_cursor) = store.get_channels_with_params(1, None, None);
    assert_eq!(res, vec![channels[0].clone()]);
    assert_eq!(res.len(), 1);

    let mut key = Vec::with_capacity(37);
    key.push(CHANNEL_INFO_PREFIX);
    key.extend_from_slice(channels[0].out_point().as_slice());
    assert_eq!(last_cursor, JsonBytes::from_bytes(key.to_vec().into()));

    let (res, _last_cursor) = store.get_channels_with_params(3, Some(last_cursor), None);
    assert_eq!(res, channels[1..=3]);
}

#[test]
fn test_store_nodes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invoice_store");
    let store = Store::new(path).expect("created store failed");

    let mut nodes = vec![];
    for _ in 0..10 {
        let (_, node) = mock_node();
        store.insert_node(node.clone());
        nodes.push(node);
    }

    // sort by node pubkey
    nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    let node_id = nodes[0].node_id;
    assert_eq!(store.get_nodes(Some(node_id)), vec![nodes[0].clone()]);
    let (res, last_cursor) = store.get_nodes_with_params(1, None, None);
    assert_eq!(res, vec![nodes[0].clone()]);
    assert_eq!(res.len(), 1);
    let mut key = Vec::with_capacity(34);
    key.push(NODE_INFO_PREFIX);
    key.extend_from_slice(nodes[0].node_id.serialize().as_ref());
    assert_eq!(last_cursor, JsonBytes::from_bytes(key.to_vec().into()));

    let (res, _last_cursor) = store.get_nodes_with_params(3, Some(last_cursor), None);
    assert_eq!(res, nodes[1..=3]);
}

#[test]
fn test_compact_signature() {
    let revocation_data = RevocationData {
        commitment_number: 0,
        x_only_aggregated_pubkey: [0u8; 32],
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        output: CellOutput::default(),
        output_data: Bytes::default(),
    };
    let bincode_encoded = bincode::serialize(&revocation_data).unwrap();
    let revocation_data: RevocationData = bincode::deserialize(&bincode_encoded).unwrap();
    assert_eq!(revocation_data, revocation_data);
}

#[test]
fn test_store_wacthtower() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("watchtower_store");
    let store = Store::new(path).expect("created store failed");

    let channel_id = gen_sha256_hash();
    let funding_tx_lock = Script::default();
    store.insert_watch_channel(channel_id, funding_tx_lock.clone());
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_tx_lock: funding_tx_lock.clone(),
            revocation_data: None
        }]
    );

    let revocation_data = RevocationData {
        commitment_number: 0,
        x_only_aggregated_pubkey: [0u8; 32],
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        output: CellOutput::default(),
        output_data: Bytes::default(),
    };

    store.update_revocation(channel_id, revocation_data.clone());
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_tx_lock,
            revocation_data: Some(revocation_data)
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
            enabled: false,
            tlc_fee_proportional_millionths: Some(123),
            tlc_max_value: Some(1),
            tlc_min_value: Some(2),
            tlc_expiry_delta: Some(3),
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
        local_pubkey: generate_pubkey().into(),
        remote_pubkey: generate_pubkey().into(),
        funding_tx: None,
        funding_tx_confirmed_at: Some((1.into(), 1)),
        is_acceptor: true,
        funding_udt_type_script: Some(Script::default()),
        to_local_amount: 100,
        to_remote_amount: 100,
        commitment_fee_rate: 100,
        commitment_delay_epoch: 100,
        funding_fee_rate: 100,
        id: gen_sha256_hash(),
        tlc_ids: Default::default(),
        tlcs: Default::default(),
        local_shutdown_script: Script::default(),
        local_channel_public_keys: ChannelBasePublicKeys {
            funding_pubkey: generate_pubkey().into(),
            tlc_base_key: generate_pubkey().into(),
        },
        signer,
        remote_channel_public_keys: Some(ChannelBasePublicKeys {
            funding_pubkey: generate_pubkey().into(),
            tlc_base_key: generate_pubkey().into(),
        }),
        commitment_numbers: Default::default(),
        remote_shutdown_script: Some(Script::default()),
        previous_remote_nonce: Some(pub_nonce.clone()),
        remote_nonce: Some(pub_nonce.clone()),
        remote_commitment_points: vec![generate_pubkey().into(), generate_pubkey().into()],
        local_shutdown_info: None,
        remote_shutdown_info: None,
        local_reserved_ckb_amount: 100,
        remote_reserved_ckb_amount: 100,
        latest_commitment_transaction: None,
        max_tlc_value_in_flight: 100,
        max_tlc_number_in_flight: 100,
        reestablishing: false,
        created_at: SystemTime::now(),
    };

    let bincode_encoded = bincode::serialize(&state).unwrap();
    let _new_state: ChannelActorState = bincode::deserialize(&bincode_encoded).unwrap();

    let store = Store::new(tempdir().unwrap().path().join("store")).expect("create store failed");
    assert!(store.get_channel_actor_state(&state.id).is_none());
    store.insert_channel_actor_state(state.clone());
    assert!(store.get_channel_actor_state(&state.id).is_some());
    store.delete_channel_actor_state(&state.id);
    assert!(store.get_channel_actor_state(&state.id).is_none());
}

#[test]
fn test_store_payment_session() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("payment_history_store");
    let store = Store::new(path).expect("created store failed");
    let payment_hash = gen_sha256_hash();
    let payment_data = SendPaymentData {
        target_pubkey: gen_rand_public_key(),
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
    let pubkey = gen_rand_public_key();
    let target = gen_rand_public_key();
    let result = TimedResult {
        fail_amount: 1,
        fail_time: 2,
        success_time: 3,
        success_amount: 4,
    };
    store.insert_payment_history_result(pubkey.into(), target.clone(), result.clone());
    assert_eq!(
        store.get_payment_history_results(),
        vec![(pubkey.into(), target.clone(), result)]
    );

    fn sort_results(results: &mut Vec<(Pubkey, Pubkey, TimedResult)>) {
        results.sort_by(|a, b| match a.0.cmp(&b.0) {
            Ordering::Equal => a.1.cmp(&b.1),
            other => other,
        });
    }

    let target_2 = gen_rand_public_key();
    let result_2 = TimedResult {
        fail_amount: 2,
        fail_time: 3,
        success_time: 4,
        success_amount: 5,
    };
    store.insert_payment_history_result(pubkey.into(), target_2.clone(), result_2.clone());
    let mut r1 = store.get_payment_history_results();
    sort_results(&mut r1);
    let mut r2: Vec<(Pubkey, Pubkey, TimedResult)> = vec![
        (pubkey.into(), target, result),
        (pubkey.into(), target_2, result_2),
    ];
    sort_results(&mut r2);
    assert_eq!(r1, r2);

    let pubkey_3 = gen_rand_public_key();
    let target_3 = gen_rand_public_key();
    let result_3 = TimedResult {
        fail_amount: 3,
        fail_time: 4,
        success_time: 5,
        success_amount: 6,
    };
    store.insert_payment_history_result(pubkey_3.into(), target_3.clone(), result_3.clone());
    let mut r1 = store.get_payment_history_results();
    sort_results(&mut r1);

    let mut r2: Vec<(Pubkey, Pubkey, TimedResult)> = vec![
        (pubkey.into(), target, result),
        (pubkey.into(), target_2, result_2),
        (pubkey_3.into(), target_3, result_3),
    ];
    sort_results(&mut r2);
    assert_eq!(r1, r2);
}
