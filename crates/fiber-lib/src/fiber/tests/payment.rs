#![allow(clippy::needless_range_loop)]
use crate::fiber::channel::*;
use crate::fiber::config::DEFAULT_FINAL_TLC_EXPIRY_DELTA;
use crate::fiber::config::DEFAULT_TLC_EXPIRY_DELTA;
use crate::fiber::config::DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS;
use crate::fiber::config::MAX_PAYMENT_TLC_EXPIRY_LIMIT;
use crate::fiber::config::MIN_TLC_EXPIRY_DELTA;
use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::network::*;
use crate::fiber::onchain_tlc_reconcile::OnChainTlcSettlement;
use crate::fiber::payment::*;
use crate::fiber::types::*;
use crate::fiber::ChannelConnectivityState;
use crate::fiber::NetworkActorCommand;
use crate::fiber::NetworkActorMessage;
use crate::fiber::{
    AddTlcCommand, ChannelState, CloseFlags, Hash256, PaymentHopData, PaymentStatus,
    PeeledPaymentOnionPacket, SendPaymentData, ShuttingDownFlags, TLCId, TlcErrorCode,
};
use crate::gen_rand_channel_outpoint;
use crate::gen_rand_fiber_public_key;
use crate::gen_rand_secp256k1_keypair_tuple;
use crate::gen_rand_sha256_hash;
use crate::invoice::CkbInvoice;
use crate::invoice::CkbInvoiceStatus;
use crate::invoice::Currency;
use crate::invoice::InvoiceBuilder;
#[cfg(not(target_arch = "wasm32"))]
use crate::invoice::PreimageStore;
use crate::now_timestamp_as_millis_u64;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::invoice::NewInvoiceParams;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::payment::{GetPaymentCommandResult, SendPaymentWithRouterParams};
use crate::tasks::cancel_tasks_and_wait_for_completion;
use crate::test_utils::init_tracing;
use crate::tests::test_utils::*;
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerStore;
use crate::NetworkServiceEvent;
use bech32::{encode, u5, Variant};
use ckb_sdk::core::TransactionBuilder;
use ckb_types::packed::Script;
use ckb_types::{core::tx_pool::TxStatus, packed::OutPoint};
#[cfg(not(target_arch = "wasm32"))]
use fiber_json_types::RouterHop as JsonRouterHop;
use fiber_sphinx::OnionSharedSecretIter;
#[cfg(not(target_arch = "wasm32"))]
use fiber_types::Hash256 as InternalHash256;
use fiber_types::HashAlgorithm;
use fiber_types::HopHint;
use fiber_types::OutboundTlcStatus;
use fiber_types::RemoveTlcFulfill;
use fiber_types::RouterHop;
use fiber_types::SessionRoute;
use fiber_types::TlcErrPacket;
use fiber_types::TlcInfo;
use fiber_types::SIGNATURE_U5_SIZE;
use fiber_types::{Attempt, AttemptStatus, PrevTlcInfo, TrampolineContext};
use ractor::call;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use std::collections::{HashMap, HashSet};
use std::panic;
use std::time::{Duration, SystemTime};
use tracing::{debug, error, info};

#[cfg(feature = "watchtower")]
fn insert_onchain_preimage<S: WatchtowerStore + ChannelActorStateStore>(
    store: &S,
    channel_id: &Hash256,
    payment_hash: Hash256,
    preimage: Hash256,
) {
    let channel_state = store
        .get_channel_actor_state(channel_id)
        .expect("channel state exists");
    let tlc = channel_state
        .tlc_state
        .all_tlcs()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .expect("TLC with payment hash exists");
    store.insert_watch_preimage(fiber_types::NodeId::local(), payment_hash, preimage);
    store.insert_onchain_tlc_settlement(
        &fiber_types::NodeId::local(),
        channel_id,
        tlc.tlc_id,
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: tlc.hash_algorithm,
            preimage: Some(preimage),
            tx_hash: gen_rand_sha256_hash(),
            tlc_index: 0,
        },
    );
}

struct RemoveTlcFailEventFixture {
    node: NetworkNode,
    _peers: Vec<NetworkNode>,
    payment_hash: Hash256,
    attempt_id: u64,
    shared_secrets: Vec<[u8; 32]>,
    node_b: Pubkey,
    node_c: Pubkey,
    channel_ab: OutPoint,
    channel_bc: OutPoint,
    channel_cd: OutPoint,
}

async fn setup_remove_tlc_fail_event_fixture() -> RemoveTlcFailEventFixture {
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node, node_b, node_c, node_d] = nodes.try_into().expect("4 nodes");
    let node_a = node.get_public_key();
    let node_b_pubkey = node_b.get_public_key();
    let node_c_pubkey = node_c.get_public_key();
    let node_d_pubkey = node_d.get_public_key();
    let amount = 700;
    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_d_pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_d.private_key.0))
        .expect("build hold invoice");
    node_d.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    let payment = node
        .send_payment(SendPaymentCommand {
            amount: Some(amount),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    assert_eq!(payment.payment_hash, payment_hash);
    node.wait_until_inflight(payment_hash).await;

    let session = node
        .get_payment_session(payment_hash)
        .expect("payment session exists");
    let attempts = session.attempts().cloned().collect::<Vec<_>>();
    assert_eq!(attempts.len(), 1);
    let attempt = attempts.into_iter().next().expect("payment attempt exists");
    assert!(attempt.is_inflight());
    let attempt_id = attempt.id;

    let route_nodes = &attempt.route.nodes;
    assert_eq!(
        route_nodes
            .iter()
            .map(|node| node.pubkey)
            .collect::<Vec<_>>(),
        vec![node_a, node_b_pubkey, node_c_pubkey, node_d_pubkey]
    );
    let hops_pubkeys: Vec<PublicKey> = attempt
        .hops_public_keys()
        .iter()
        .map(|key| PublicKey::from_slice(&key.0).expect("valid pubkey"))
        .collect();
    let shared_secrets = OnionSharedSecretIter::new(
        hops_pubkeys.iter(),
        SecretKey::from_slice(&attempt.session_key).unwrap(),
        SECP256K1,
    )
    .collect::<Result<Vec<_>, _>>()
    .expect("valid shared secrets");

    let channel_ab = route_nodes[0].channel_outpoint.clone();
    let channel_bc = route_nodes[1].channel_outpoint.clone();
    let channel_cd = route_nodes[2].channel_outpoint.clone();

    RemoveTlcFailEventFixture {
        node,
        _peers: vec![node_b, node_c, node_d],
        payment_hash,
        attempt_id,
        shared_secrets,
        node_b: node_b_pubkey,
        node_c: node_c_pubkey,
        channel_ab,
        channel_bc,
        channel_cd,
    }
}

async fn send_remove_tlc_fail_event(fixture: &RemoveTlcFailEventFixture, packet: TlcErrPacket) {
    fixture
        .node
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::TlcRemoveReceived(
                fixture.payment_hash,
                Some(fixture.attempt_id),
                RemoveTlcReason::RemoveTlcFail(packet),
            ),
        ))
        .expect("network actor alive");

    wait_until_timeout(10_000, || {
        fixture
            .node
            .get_payment_session(fixture.payment_hash)
            .is_some_and(|session| {
                session
                    .attempts()
                    .any(|attempt| attempt.id == fixture.attempt_id && attempt.last_error.is_some())
            })
    })
    .await;
}

fn failed_history_outpoints(fixture: &RemoveTlcFailEventFixture) -> HashSet<OutPoint> {
    fixture
        .node
        .store
        .get_payment_history_results()
        .into_iter()
        .filter_map(|(outpoint, _direction, result)| (result.fail_time != 0).then_some(outpoint))
        .collect()
}

fn test_payment_session(request: SendPaymentData, now: u64) -> PaymentSession {
    PaymentSession {
        request,
        last_error: None,
        last_error_code: None,
        try_limit: 3,
        status: PaymentStatus::Created,
        created_at: now,
        last_updated_at: now,
        cached_attempts: vec![],
    }
}

fn test_attempt(
    id: u64,
    payment_hash: Hash256,
    source: Pubkey,
    target: Pubkey,
    route_hops: Vec<PaymentHopData>,
    now: u64,
) -> Attempt {
    Attempt {
        id,
        hash: payment_hash,
        try_limit: 3,
        tried_times: 1,
        payment_hash,
        route: SessionRoute::new(source, target, &route_hops),
        route_hops,
        session_key: [0; 32],
        preimage: None,
        created_at: now,
        last_updated_at: now,
        last_error: None,
        status: AttemptStatus::Created,
    }
}

#[test]
fn test_sender_side_trampoline_retry_does_not_reuse_visible_route_amount() {
    init_tracing();

    let final_amount = 1000;
    let amount_to_trampoline = 1200;
    let source = gen_rand_fiber_public_key();
    let trampoline = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let payment_hash = gen_rand_sha256_hash();
    let request = SendPaymentDataBuilder::new(target, final_amount, payment_hash)
        .max_fee_amount(Some(amount_to_trampoline - final_amount))
        .trampoline_hops(Some(vec![trampoline]))
        .build()
        .expect("valid trampoline payment request");

    let now = now_timestamp_as_millis_u64();
    let route_hops = vec![
        PaymentHopData {
            amount: amount_to_trampoline,
            expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
            hash_algorithm: HashAlgorithm::CkbHash,
            funding_tx_hash: gen_rand_sha256_hash(),
            next_hop: Some(trampoline),
            ..Default::default()
        },
        PaymentHopData {
            amount: amount_to_trampoline,
            expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
            hash_algorithm: HashAlgorithm::CkbHash,
            ..Default::default()
        },
    ];
    let mut attempt = Attempt {
        id: 1,
        hash: payment_hash,
        try_limit: 3,
        tried_times: 1,
        payment_hash,
        route: SessionRoute::new(source, target, &route_hops),
        route_hops,
        session_key: [0; 32],
        preimage: None,
        created_at: now,
        last_updated_at: now,
        last_error: Some("temporary trampoline failure".to_string()),
        status: AttemptStatus::Retrying,
    };
    assert_eq!(attempt.route.receiver_amount(), amount_to_trampoline);

    let mut session = PaymentSession {
        request,
        last_error: None,
        last_error_code: None,
        try_limit: 3,
        status: PaymentStatus::Created,
        created_at: now,
        last_updated_at: now,
        cached_attempts: vec![attempt.clone()],
    };
    assert_eq!(attempt.route.receiver_amount(), amount_to_trampoline);
    assert_eq!(session.remain_amount(), 0);
    assert_eq!(
        session
            .remain_amount()
            .checked_add(attempt.route.receiver_amount()),
        Some(amount_to_trampoline)
    );
    assert_eq!(session.retry_amount(&attempt), Some(final_amount));

    attempt.set_success_status();
    session.cached_attempts = vec![attempt];
    assert_eq!(session.calc_payment_status(), PaymentStatus::Success);
}

#[test]
fn test_sender_side_trampoline_with_mpp_invoice_stays_single_attempt() {
    init_tracing();

    let final_amount = 1000;
    let amount_to_trampoline = 1200;
    let source = gen_rand_fiber_public_key();
    let trampoline = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let payment_hash = gen_rand_sha256_hash();
    let request = SendPaymentDataBuilder::new(target, final_amount, payment_hash)
        .max_fee_amount(Some(amount_to_trampoline - final_amount))
        .allow_mpp(true)
        .max_parts(Some(4))
        .trampoline_hops(Some(vec![trampoline]))
        .build()
        .expect("valid sender-side trampoline MPP payment request");

    assert!(request.allow_mpp());
    assert!(request.use_trampoline_routing());

    let now = now_timestamp_as_millis_u64();
    let route_hops = vec![
        PaymentHopData {
            amount: amount_to_trampoline,
            expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
            hash_algorithm: HashAlgorithm::CkbHash,
            funding_tx_hash: gen_rand_sha256_hash(),
            next_hop: Some(trampoline),
            ..Default::default()
        },
        PaymentHopData {
            amount: amount_to_trampoline,
            expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
            hash_algorithm: HashAlgorithm::CkbHash,
            ..Default::default()
        },
    ];
    let attempt = test_attempt(1, payment_hash, source, target, route_hops, now);
    let mut session = test_payment_session(request, now);

    assert_eq!(session.max_parts(), 1);
    assert!(session.allow_more_attempts());

    session.append_attempt(attempt.clone());

    assert_eq!(session.attempts_count(), 1);
    assert_eq!(session.remain_amount(), 0);
    assert_eq!(session.retry_amount(&attempt), Some(final_amount));
    assert!(!session.allow_more_attempts());
}

#[test]
fn test_trampoline_context_mpp_counts_each_shard_receiver_amount() {
    init_tracing();

    let total_amount = 1000;
    let first_shard_amount = 400;
    let second_shard_amount = 600;
    let source = gen_rand_fiber_public_key();
    let middle = gen_rand_fiber_public_key();
    let target = gen_rand_fiber_public_key();
    let payment_hash = gen_rand_sha256_hash();
    let trampoline_context = TrampolineContext {
        remaining_trampoline_onion: vec![1, 2, 3],
        previous_tlcs: vec![PrevTlcInfo::new_with_shared_secret(
            gen_rand_sha256_hash(),
            1,
            0,
            [0; 32],
        )],
        hash_algorithm: HashAlgorithm::CkbHash,
        max_outgoing_tlc_expiry: None,
    };
    let request = SendPaymentDataBuilder::new(target, total_amount, payment_hash)
        .max_fee_amount(Some(200))
        .allow_mpp(true)
        .max_parts(Some(2))
        .trampoline_context(Some(trampoline_context))
        .build()
        .expect("valid trampoline forwarding MPP payment request");

    assert!(request.allow_mpp());
    assert!(!request.use_trampoline_routing());
    assert!(request.trampoline_context.is_some());

    let now = now_timestamp_as_millis_u64();
    let first_attempt = test_attempt(
        1,
        payment_hash,
        source,
        target,
        vec![
            PaymentHopData {
                amount: first_shard_amount + 10,
                expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                hash_algorithm: HashAlgorithm::CkbHash,
                funding_tx_hash: gen_rand_sha256_hash(),
                next_hop: Some(middle),
                ..Default::default()
            },
            PaymentHopData {
                amount: first_shard_amount,
                expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                hash_algorithm: HashAlgorithm::CkbHash,
                ..Default::default()
            },
        ],
        now,
    );
    let second_attempt = test_attempt(
        2,
        payment_hash,
        source,
        target,
        vec![
            PaymentHopData {
                amount: second_shard_amount + 20,
                expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                hash_algorithm: HashAlgorithm::CkbHash,
                funding_tx_hash: gen_rand_sha256_hash(),
                next_hop: Some(middle),
                ..Default::default()
            },
            PaymentHopData {
                amount: second_shard_amount,
                expiry: now + DEFAULT_FINAL_TLC_EXPIRY_DELTA,
                hash_algorithm: HashAlgorithm::CkbHash,
                ..Default::default()
            },
        ],
        now,
    );

    let mut session = test_payment_session(request, now);
    assert_eq!(session.max_parts(), 2);
    assert_eq!(first_attempt.route.receiver_amount(), first_shard_amount);

    session.append_attempt(first_attempt.clone());
    assert_eq!(session.remain_amount(), second_shard_amount);
    assert_eq!(session.retry_amount(&first_attempt), Some(total_amount));
    assert!(session.allow_more_attempts());

    assert_eq!(second_attempt.route.receiver_amount(), second_shard_amount);
    session.append_attempt(second_attempt);
    assert_eq!(session.remain_amount(), 0);
    assert!(!session.allow_more_attempts());
}

#[tokio::test]
async fn test_remove_tlc_fail_event_decode_fallback_does_not_record_history() {
    init_tracing();

    let fixture = setup_remove_tlc_fail_event_fixture().await;
    let packet = TlcErrPacket {
        onion_packet: vec![1, 2, 3],
    };

    send_remove_tlc_fail_event(&fixture, packet).await;

    let session = fixture
        .node
        .get_payment_session(fixture.payment_hash)
        .expect("payment session exists");
    assert_eq!(session.status, PaymentStatus::Failed);
    assert_eq!(
        session.last_error_code,
        Some(TlcErrorCode::InvalidOnionError)
    );
    assert!(failed_history_outpoints(&fixture).is_empty());
}

#[tokio::test]
async fn test_remove_tlc_fail_event_rejects_forged_attribution() {
    init_tracing();

    let fixture = setup_remove_tlc_fail_event_fixture().await;
    let forged_error = TlcErr::new_channel_fail(
        TlcErrorCode::PermanentChannelFailure,
        fixture.node_c,
        fixture.channel_cd.clone(),
        None,
    );
    let packet = TlcErrPacket::new(forged_error, &fixture.shared_secrets[0]);

    send_remove_tlc_fail_event(&fixture, packet).await;

    let failed_outpoints = failed_history_outpoints(&fixture);
    assert!(failed_outpoints.contains(&fixture.channel_ab));
    assert!(!failed_outpoints.contains(&fixture.channel_bc));
    assert!(!failed_outpoints.contains(&fixture.channel_cd));
}

#[tokio::test]
async fn test_remove_tlc_fail_event_records_authenticated_downstream_channel_failure() {
    init_tracing();

    let fixture = setup_remove_tlc_fail_event_fixture().await;
    let channel_error = TlcErr::new_channel_fail(
        TlcErrorCode::PermanentChannelFailure,
        fixture.node_c,
        fixture.channel_cd.clone(),
        None,
    );
    let packet = TlcErrPacket::new(channel_error, &fixture.shared_secrets[1])
        .backward(&fixture.shared_secrets[0])
        .expect("backward encrypted error");

    send_remove_tlc_fail_event(&fixture, packet).await;

    let failed_outpoints = failed_history_outpoints(&fixture);
    assert!(!failed_outpoints.contains(&fixture.channel_ab));
    assert!(!failed_outpoints.contains(&fixture.channel_bc));
    assert!(failed_outpoints.contains(&fixture.channel_cd));
}

#[tokio::test]
async fn test_remove_tlc_fail_event_first_hop_incorrect_tlc_expiry_records_reporting_channel() {
    init_tracing();

    let fixture = setup_remove_tlc_fail_event_fixture().await;
    let channel_error = TlcErr::new_channel_fail(
        TlcErrorCode::IncorrectTlcExpiry,
        fixture.node_b,
        fixture.channel_ab.clone(),
        None,
    );
    let packet = TlcErrPacket::new(channel_error, &fixture.shared_secrets[0]);

    send_remove_tlc_fail_event(&fixture, packet).await;

    let failed_outpoints = failed_history_outpoints(&fixture);
    assert!(failed_outpoints.contains(&fixture.channel_ab));
    assert!(!failed_outpoints.contains(&fixture.channel_bc));
    assert!(!failed_outpoints.contains(&fixture.channel_cd));
}

#[tokio::test]
async fn test_remove_tlc_fail_event_incorrect_tlc_expiry_records_reporting_channel() {
    init_tracing();

    let fixture = setup_remove_tlc_fail_event_fixture().await;
    let channel_error = TlcErr::new_channel_fail(
        TlcErrorCode::IncorrectTlcExpiry,
        fixture.node_c,
        fixture.channel_bc.clone(),
        None,
    );
    let packet = TlcErrPacket::new(channel_error, &fixture.shared_secrets[1])
        .backward(&fixture.shared_secrets[0])
        .expect("backward encrypted error");

    send_remove_tlc_fail_event(&fixture, packet).await;

    let failed_outpoints = failed_history_outpoints(&fixture);
    assert!(!failed_outpoints.contains(&fixture.channel_ab));
    assert!(failed_outpoints.contains(&fixture.channel_bc));
    assert!(!failed_outpoints.contains(&fixture.channel_cd));
}

#[tokio::test]
async fn test_send_payment_custom_records() {
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let data: HashMap<_, _> = vec![
        (1, "hello".to_string().into_bytes()),
        (2, "world".to_string().into_bytes()),
    ]
    .into_iter()
    .collect();
    let custom_records = PaymentCustomRecords { data };
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_final_status(payment_hash).await;

    assert_eq!(
        source_node.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );
    let got_custom_records = node_1
        .get_payment_custom_records(&payment_hash)
        .expect("custom records");
    assert_eq!(got_custom_records, custom_records);

    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_custom_records_with_limit_error() {
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let long_value = "a".repeat(MAX_CUSTOM_RECORDS_SIZE + 1);
    let data: HashMap<_, _> = vec![(1, long_value.into_bytes())].into_iter().collect();
    let custom_records = PaymentCustomRecords { data };
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        })
        .await;

    let err = res.unwrap_err().to_string();
    assert!(err.contains("custom_records encoded size"));

    // normal case
    let long_value = "a".repeat(1024);
    let data: HashMap<_, _> = vec![(1, long_value.into_bytes())].into_iter().collect();
    let custom_records = PaymentCustomRecords { data };
    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        })
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;
    let got_custom_records = node_1
        .get_payment_custom_records(&payment_hash)
        .expect("custom records");
    assert_eq!(got_custom_records, custom_records);
    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_receive_payment_rejects_oversized_custom_records() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let amount = 1000;
    let hash_algorithm = HashAlgorithm::Sha256;
    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = hash_algorithm.hash(payment_preimage).into();
    let expiry = now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA;
    let custom_records = PaymentCustomRecords {
        data: vec![(1, vec![1; MAX_CUSTOM_RECORDS_SIZE + 1])]
            .into_iter()
            .collect(),
    };

    let hops_infos = vec![
        PaymentHopData {
            amount,
            expiry,
            next_hop: Some(node_1.pubkey),
            hash_algorithm,
            ..Default::default()
        },
        PaymentHopData {
            amount,
            expiry,
            payment_preimage: Some(payment_preimage),
            hash_algorithm,
            custom_records: Some(custom_records),
            ..Default::default()
        },
    ];

    let packet = PeeledPaymentOnionPacket::create(
        node_0.get_private_key().clone(),
        hops_infos,
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result = ractor::call!(node_0.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount,
                        hash_algorithm,
                        payment_hash,
                        expiry,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        is_trampoline_hop: false,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive");
    assert!(add_tlc_result.is_ok());

    let target_pubkey = node_1.pubkey;
    node_1
        .expect_event(|event| match event {
            NetworkServiceEvent::DebugEvent(DebugEvent::AddTlcFailed(
                pubkey,
                failed_payment_hash,
                err,
            )) => {
                pubkey == &target_pubkey
                    && failed_payment_hash == &payment_hash
                    && err.error_code == TlcErrorCode::IncorrectOrUnknownPaymentDetails
            }
            _ => false,
        })
        .await;
    assert!(node_1.get_payment_custom_records(&payment_hash).is_none());
}

// This test will send two payments from node_0 to node_1, the first payment will run
// with dry_run, the second payment will run without dry_run. Both payments will be successful.
// But only one payment balance will be deducted from node_0.
#[tokio::test]
async fn test_send_payment_for_direct_channel_and_dry_run() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/359

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let channel = channels[0];
    let source_node = &node_0;

    let res = source_node
        .send_payment_keysend(&node_1, 10000000000, true)
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = source_node
        .send_payment_keysend(&node_1, 10000000000, false)
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    let node_0_balance = source_node.get_local_balance_from_channel(channel);
    let node_1_balance = node_1.get_local_balance_from_channel(channel);

    // A -> B: 10000000000 use the first channel
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_prefer_newer_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // We are using the second (newer) channel, so the first channel's balances are unchanged.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);

    // We are using the second (newer) channel, so the second channel's balances are changed.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_keysend_without_max_fee() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node_0, _node_1, node_2] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_2.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000),
            keysend: Some(true),
            dry_run: true,
            ..Default::default()
        })
        .await
        .unwrap();

    eprintln!("res: {:?}", res);

    assert_eq!(res.fee, 10000);

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(10000000),
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;
    let payment = source_node.get_payment_result(payment_hash).await;

    eprintln!("payment info: {:?}", payment);
}

#[tokio::test]
async fn test_send_payment_prefer_channels_with_larger_balance() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            // These two channels have the same overall capacity, but the second channel has more balance for node_0.
            (
                (0, 1),
                (MIN_RESERVED_CKB + 5000000000, MIN_RESERVED_CKB + 5000000000),
            ),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(5000000000),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    source_node.wait_until_success(payment_hash).await;

    // We are using the second channel (with larger balance), so the first channel's balances are unchanged.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 5000000000);
    assert_eq!(node_1_balance, 5000000000);

    // We are using the second channel (with larger balance), so the second channel's balances are changed.
    let node_0_balance = source_node.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, 5000000000);
    assert_eq!(node_1_balance, 5000000000);
    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_with_tool_large_fee_and_amount() {
    init_tracing();

    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let target_pubkey = node_1.pubkey;

    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(1),
            max_fee_amount: Some(u128::MAX),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    // will succeed because of default max_fee_rate is 0.5%
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_fee_rate() {
    init_tracing();
    let [mut node_0, mut node_1, mut node_2] = NetworkNode::new_n_interconnected_nodes().await;

    let (_new_channel_id, funding_tx_hash_0) = establish_channel_between_nodes(
        &mut node_0,
        &mut node_1,
        ChannelParameters {
            public: true,
            node_a_funding_amount: MIN_RESERVED_CKB + 1_000_000_000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            a_tlc_fee_proportional_millionths: Some(1_000_000),
            b_tlc_fee_proportional_millionths: Some(2_000_000),
            ..Default::default()
        },
    )
    .await;
    let funding_tx_0 = node_0
        .get_transaction_view_from_hash(funding_tx_hash_0)
        .await
        .expect("get funding tx");
    node_2.submit_tx(funding_tx_0).await;

    let (_new_channel_id, funding_tx_hash_1) = establish_channel_between_nodes(
        &mut node_1,
        &mut node_2,
        ChannelParameters {
            public: true,
            node_a_funding_amount: MIN_RESERVED_CKB + 1_000_000_000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            a_tlc_fee_proportional_millionths: Some(3_000_000),
            b_tlc_fee_proportional_millionths: Some(4_000_000),
            ..Default::default()
        },
    )
    .await;
    let funding_tx_1 = node_1
        .get_transaction_view_from_hash(funding_tx_hash_1)
        .await
        .expect("get funding tx");
    node_0.submit_tx(funding_tx_1).await;

    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(10_000_000),
            keysend: Some(true),
            // use a high max fee rate to make sure payment success
            max_fee_rate: Some(5000),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    assert!(res.fee > 0);
    let nodes = &res.routers[0].nodes;
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[2].amount, 10_000_000);
    assert_eq!(nodes[1].amount, 10_000_000);
    // The fee is 10_000_000 * 3_000_000 (fee rate) / 1_000_000 = 30_000_000
    assert_eq!(nodes[0].amount, 40_000_000);
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    assert_eq!(node_0.get_inflight_payment_count().await, 0);

    let res = node_2
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_0.pubkey),
            amount: Some(1_000_000),
            keysend: Some(true),
            // use a high max fee rate to make sure payment success
            max_fee_rate: Some(5000),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    assert!(res.fee > 0);
    let nodes = &res.routers[0].nodes;
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[2].amount, 1_000_000);
    assert_eq!(nodes[1].amount, 1_000_000);
    // The fee is 1_000_000 * 2_000_000 (fee rate) / 1_000_000 = 2_000_000
    assert_eq!(nodes[0].amount, 3_000_000);

    let payment_hash = res.payment_hash;
    node_2.wait_until_success(payment_hash).await;
    assert_eq!(node_2.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_over_private_channel() {
    async fn test(amount_to_send: u128, is_payment_ok: bool) {
        let (nodes, _channels) = create_n_nodes_network(
            &[((1, 2), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB))],
            3,
        )
        .await;
        let [mut node1, mut node2, node3] = nodes.try_into().expect("3 nodes");

        let (_new_channel_id, _funding_tx) = establish_channel_between_nodes(
            &mut node1,
            &mut node2,
            ChannelParameters {
                public: false,
                node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
                node_b_funding_amount: MIN_RESERVED_CKB,
                ..Default::default()
            },
        )
        .await;

        let source_node = &mut node1;
        let target_pubkey = node3.pubkey;

        let res = source_node
            .send_payment(SendPaymentCommand {
                target_pubkey: Some(target_pubkey),
                amount: Some(amount_to_send),
                keysend: Some(true),
                ..Default::default()
            })
            .await;

        eprintln!("res: {:?}", res);
        if is_payment_ok {
            assert!(res.is_ok());
            source_node
                .wait_until_success(res.unwrap().payment_hash)
                .await;
        } else {
            assert!(res.is_err());
        }
        assert_eq!(source_node.get_inflight_payment_count().await, 0);
    }

    test(10000000000, true).await;
    test(30000000000, false).await;
}

#[tokio::test]
async fn test_send_payment_for_pay_self() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/362

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((2, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // now node_0 -> node_2 will be ok only with node_1, so the fee is larger than 0
    let res = node_0.send_payment_keysend(&node_2, 60000000, true).await;

    assert!(res.unwrap().fee > 0);

    // node_0 -> node_0 will be ok for dry_run if `allow_self_payment` is true
    let res = node_0.send_payment_keysend_to_self(60000000, false).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    assert_eq!(node_0_balance1, 10000000000 - 60000000 - res.fee);
    assert_eq!(node_0_balance2, 60000000);

    eprintln!(
        "node1 left: {:?}, right: {:?}",
        node_1.get_local_balance_from_channel(channels[0]),
        node_1.get_local_balance_from_channel(channels[1])
    );

    let node_1_new_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_new_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    let node1_fee = (node_1_new_channel0_balance - node_1_channel0_balance)
        - (node_1_channel1_balance - node_1_new_channel1_balance);
    assert!(node1_fee > 0);

    let node2_fee = (node_2_new_channel1_balance - node_2_channel1_balance)
        - (node_2_channel2_balance - node_2_new_channel2_balance);
    assert!(node2_fee > 0);
    assert_eq!(node1_fee + node2_fee, res.fee);

    // node_0 -> node_2 will be ok with direct channel2,
    // since after payself this channel now have enough balance, so the fee is 0
    let res = node_0.send_payment_keysend(&node_2, 60000000, true).await;

    eprintln!("res: {:?}", res);
    assert_eq!(res.unwrap().fee, 0);
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_for_pay_self_with_two_nodes() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/355

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 0), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);

    // node_0 -> node_0 will be ok for dry_run if `allow_self_payment` is true
    let res = node_0.send_payment_keysend_to_self(60000000, false).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[1]);

    assert_eq!(node_0_balance1, 10000000000 - 60000000 - res.fee);
    assert_eq!(node_0_balance2, 60000000);

    let new_node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let new_node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);

    let node1_fee = (new_node_1_channel0_balance - node_1_channel0_balance)
        - (node_1_channel1_balance - new_node_1_channel1_balance);
    eprintln!("fee: {:?}", res.fee);
    assert_eq!(node1_fee, res.fee);
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_for_pay_self_with_invoice() {
    init_tracing();
    let (nodes, channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
            (
                (2, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: MIN_RESERVED_CKB + 10000000000,
                    node_b_funding_amount: MIN_RESERVED_CKB,
                    ..Default::default()
                },
            ),
        ],
        3,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, _node_1, _node_2] = nodes.try_into().expect("3 nodes");

    let old_node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let old_node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    let invoice = node_0
        .gen_invoice(NewInvoiceParams {
            amount: 100,
            description: Some("test invoice".to_string()),
            expiry: None,
            ..Default::default()
        })
        .await;

    // node_0 -> node_0 will be ok for pay_self with invoice
    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            allow_self_payment: true,
            max_fee_rate: Some(1000),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    let node_0_sent = old_node_0_balance1 - node_0.get_local_balance_from_channel(channels[0]);
    let node_0_received = node_0.get_local_balance_from_channel(channels[2]) - old_node_0_balance2;
    let fee = res.fee;
    assert_eq!(
        node_0_sent,
        node_0_received + fee,
        "node_0 balance should be changed by fee only"
    );
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_with_normal_invoice_workflow() {
    init_tracing();
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[(
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        )],
        2,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let invoice = node_1
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("test invoice".to_string()),
            final_expiry_delta: Some(2048),
            ..Default::default()
        })
        .await;

    // node_0 -> node_1 will be ok for normal invoice
    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_with_more_capacity_for_payself() {
    init_tracing();

    // from https://github.com/nervosnetwork/fiber/issues/362

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // node_0 -> node_0 will be ok if `allow_self_payment` is true
    let res = node_0.send_payment_keysend_to_self(60000000, false).await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    // sleep for a while
    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    eprintln!("fee: {:?}", res.fee);
    // for node0 pay to self, only the fee will be deducted
    assert!(node_0_balance1 + node_0_balance2 == 10000000000 + 10000000000 - res.fee);

    eprintln!(
        "node1 left: {:?}, right: {:?}",
        node_1.get_local_balance_from_channel(channels[0]),
        node_1.get_local_balance_from_channel(channels[1])
    );

    let node_1_new_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_new_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // we may route to self from
    //     node0 -> node1 -> node2 -> node0
    // or  node0 -> node2 -> node1 -> node0
    // so the assertion need to be more complex
    let node1_fee = if node_1_new_channel0_balance > node_1_channel0_balance {
        (node_1_new_channel0_balance - node_1_channel0_balance)
            - (node_1_channel1_balance - node_1_new_channel1_balance)
    } else {
        (node_1_new_channel1_balance - node_1_channel1_balance)
            - (node_1_channel0_balance - node_1_new_channel0_balance)
    };
    assert!(node1_fee > 0);

    let node2_fee = if node_2_new_channel1_balance > node_2_channel1_balance {
        (node_2_new_channel1_balance - node_2_channel1_balance)
            - (node_2_channel2_balance - node_2_new_channel2_balance)
    } else {
        (node_2_new_channel2_balance - node_2_channel2_balance)
            - (node_2_channel1_balance - node_2_new_channel1_balance)
    };
    assert_eq!(node1_fee + node2_fee, res.fee);
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_with_private_channel_hints() {
    async fn test(amount_to_send: u128, is_payment_ok: bool) {
        let (nodes, _channels) = create_n_nodes_network(
            &[((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB))],
            3,
        )
        .await;
        let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

        let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
            &mut node2,
            &mut node3,
            ChannelParameters {
                public: false,
                node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
                node_b_funding_amount: MIN_RESERVED_CKB,
                ..Default::default()
            },
        )
        .await;
        let funding_tx = node2
            .get_transaction_view_from_hash(funding_tx_hash)
            .await
            .expect("get funding tx");

        let outpoint = funding_tx.output_pts_iter().next().unwrap();

        let source_node = &mut node1;
        let target_pubkey = node3.pubkey;

        let res = source_node
            .send_payment(SendPaymentCommand {
                target_pubkey: Some(target_pubkey),
                amount: Some(amount_to_send),
                keysend: Some(true),
                hop_hints: Some(vec![HopHint {
                    pubkey: node2.pubkey,
                    channel_outpoint: outpoint,
                    fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                    tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
                }]),
                ..Default::default()
            })
            .await;

        assert!(res.is_ok(), "Send payment failed: {:?}", res);
        let res = res.unwrap();
        let payment_hash = res.payment_hash;
        if is_payment_ok {
            source_node.wait_until_success(payment_hash).await;
        } else {
            source_node.wait_until_failed(payment_hash).await;
        }

        assert_eq!(source_node.get_inflight_payment_count().await, 0);
    }

    test(10000000000, true).await;
    test(30000000000, false).await;
}

#[test]
fn test_send_payment_rejects_hop_hints_when_invoice_disallows() {
    let (private_key, public_key) = gen_rand_secp256k1_keypair_tuple();
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(preimage)
        .payee_pub_key(public_key)
        .allow_trampoline_routing(false)
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &private_key))
        .expect("build invoice");

    let hop_hint = HopHint {
        pubkey: gen_rand_fiber_public_key(),
        channel_outpoint: OutPoint::default(),
        fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
        tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
    };

    let err = SendPaymentData::new(SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        hop_hints: Some(vec![hop_hint]),
        ..Default::default()
    })
    .unwrap_err();

    assert!(
        err.contains("invoice does not support hop hints"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_send_payment_rejects_unsigned_invoice_by_default() {
    let payee_pubkey = gen_rand_fiber_public_key();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(gen_rand_sha256_hash())
        .payee_pub_key(payee_pubkey.into())
        .build()
        .expect("build unsigned invoice");

    let err = SendPaymentData::new(SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        ..Default::default()
    })
    .unwrap_err();

    assert!(
        err.contains("invoice is not signed"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_send_payment_with_router_rejects_unsigned_invoice() {
    let payee_pubkey = gen_rand_fiber_public_key();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(gen_rand_sha256_hash())
        .payee_pub_key(payee_pubkey.into())
        .build()
        .expect("build unsigned invoice");
    let router = vec![RouterHop {
        target: payee_pubkey,
        channel_outpoint: gen_rand_channel_outpoint(),
        amount_received: 1000,
        incoming_tlc_expiry: DEFAULT_TLC_EXPIRY_DELTA,
    }];

    let err = SendPaymentWithRouterCommand {
        invoice: Some(invoice.to_string()),
        router,
        ..Default::default()
    }
    .build_send_payment_data(gen_rand_fiber_public_key())
    .unwrap_err();

    let crate::Error::InvalidParameter(message) = err else {
        panic!("unexpected error: {err}");
    };
    assert!(
        message.contains("invoice is not signed"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_send_payment_with_too_large_hop_hint_fee_rate() {
    init_tracing();
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (u64::MAX as u128 / 3, MIN_RESERVED_CKB))], 3).await;
    let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

    let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node2,
        &mut node3,
        ChannelParameters {
            public: false,
            node_a_funding_amount: u64::MAX as u128 / 3,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node2
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    let outpoint = funding_tx.output_pts_iter().next().unwrap();

    let source_node = &mut node1;
    let target_pubkey = node3.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some((u64::MAX / 4) as u128),
            keysend: Some(true),
            hop_hints: Some(vec![HopHint {
                pubkey: node2.pubkey,
                channel_outpoint: outpoint,
                fee_rate: u64::MAX, // too large fee rate
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_err(), "Expect send payment failed: {:?}", res);
    assert!(res.unwrap_err().to_string().contains("no path found"));
    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_hophint_for_middle_channels_does_not_work() {
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((2, 3), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node1, mut node2, mut node3, node4] = nodes.try_into().expect("4 nodes");

    // create a private channel between node2 and node3
    let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node2,
        &mut node3,
        ChannelParameters {
            public: false,
            node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node2
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    let private_channel_outpoint = funding_tx.output_pts_iter().next().unwrap();

    let res = node1
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node4.pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            hop_hints: Some(vec![HopHint {
                pubkey: node2.pubkey,
                channel_outpoint: private_channel_outpoint.clone(),
                fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();

    // the router is wrong with node1 -> node2 -> node4
    // the second channel is private_channel_outpoint
    assert_eq!(
        res.routers[0].nodes[1].channel_outpoint,
        private_channel_outpoint
    );
    let payment_hash = res.payment_hash;

    // this router will not payment succeeded
    wait_until_async_timeout(|| async {
        node1.get_payment_status(payment_hash).await == PaymentStatus::Failed
    })
    .await;
    let res = node1.get_payment_result(payment_hash).await;
    eprintln!("res: {:?}", res);
    assert!(res.failed_error.unwrap().contains("InvalidOnionError"));
    assert_eq!(node1.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_hophint_for_mixed_channels_with_udt() {
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    public: true,
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    public: true,
                    ..Default::default()
                },
            ),
            (
                (2, 3),
                ChannelParameters {
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    public: true, // not a private channel
                    funding_udt_type_script: Some(Script::default()), // a UDT channel
                    ..Default::default()
                },
            ),
        ],
        4,
        None,
    )
    .await;
    let [node1, _node2, node3, node4] = nodes.try_into().expect("4 nodes");

    let channel_outpoint = node3.get_channel_outpoint(&_channels[2]).unwrap();

    let res = node1
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node4.pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            // hop hints will be ignored because of find_path can get channel_info
            hop_hints: Some(vec![HopHint {
                pubkey: node3.pubkey,
                channel_outpoint,
                fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_err());
    assert_eq!(node1.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_with_private_channel_hints_fallback() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

    let (_new_channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node2,
        &mut node3,
        ChannelParameters {
            public: false,
            node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;

    let outpoint = node2.get_channel_outpoint(&_new_channel_id).unwrap();
    let channel1_outpoint = node1.get_channel_outpoint(&_channels[0]).unwrap();
    let channel2_outpoint = node2.get_channel_outpoint(&_channels[1]).unwrap();

    debug!("channel1 outpoint: {:?}", channel1_outpoint);
    debug!("channel2 outpoint: {:?}", channel2_outpoint);
    debug!("private_channel outpoint: {:?}", outpoint);

    let source_node = &mut node1;
    let target_pubkey = node3.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(30000000000),
            keysend: Some(true),
            allow_self_payment: true,
            custom_records: None,
            hop_hints: Some(vec![HopHint {
                pubkey: node2.pubkey,
                channel_outpoint: outpoint,
                fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    let res = res.unwrap();
    let payment_hash = res.payment_hash;

    // the actual capacity of private channel is not enough for this payment
    // will first use the private channel, then send payment retry will fallback to public channel
    source_node.wait_until_success(payment_hash).await;
    source_node
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(2))
        .await;
    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_payself_with_private_channel_cycle() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node1, _node2, mut node3] = nodes.try_into().expect("3 nodes");

    let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node3,
        &mut node1,
        ChannelParameters {
            public: false,
            node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;
    let _funding_tx = node3
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    let source_node = &mut node1;

    let res = source_node
        .send_payment_keysend_to_self(30000000000, false)
        .await;

    assert!(res.is_err());
    assert_eq!(source_node.get_inflight_payment_count().await, 0);

    let res = source_node
        .send_payment_keysend_to_self(10000000000, false)
        .await;

    assert!(res.is_ok(), "Send payment failed: {:?}", res);
    assert_eq!(source_node.get_inflight_payment_count().await, 1);
}

#[tokio::test]
async fn test_send_payment_with_private_multiple_channel_hints_fallback() {
    init_tracing();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

    async fn create_channel(
        node2: &mut NetworkNode,
        node3: &mut NetworkNode,
        amount: u128,
    ) -> OutPoint {
        let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
            node2,
            node3,
            ChannelParameters {
                public: false,
                node_a_funding_amount: MIN_RESERVED_CKB + amount,
                node_b_funding_amount: MIN_RESERVED_CKB,
                ..Default::default()
            },
        )
        .await;
        node2
            .get_transaction_view_from_hash(funding_tx_hash)
            .await
            .expect("get funding tx")
            .output_pts_iter()
            .next()
            .unwrap()
    }

    let outpoint1 = create_channel(&mut node2, &mut node3, 20000000000).await;
    let outpoint2 = create_channel(&mut node2, &mut node3, 40000000000).await;

    let source_node = &mut node1;
    let target_pubkey = node3.pubkey;

    let res = source_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(target_pubkey),
            amount: Some(30000000000),
            keysend: Some(true),
            hop_hints: Some(vec![
                HopHint {
                    pubkey: node2.pubkey,
                    channel_outpoint: outpoint1,
                    fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                    tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
                },
                HopHint {
                    pubkey: node2.pubkey,
                    channel_outpoint: outpoint2,
                    fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                    tlc_expiry_delta: DEFAULT_TLC_EXPIRY_DELTA,
                },
            ]),
            ..Default::default()
        })
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    source_node.wait_until_success(payment_hash).await;
    let payment_session = source_node.get_payment_session(payment_hash).unwrap();
    assert_eq!(payment_session.retry_times(), 2);
    assert_eq!(source_node.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_build_router_basic() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let router_nodes: Vec<_> = router.router_hops.iter().map(|x| x.target).collect();
    eprintln!("router_nodes: {:?}", router_nodes);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(router_nodes, vec![node_1.pubkey]);
    assert_eq!(amounts, vec![1]);

    let payment = node_0.send_payment_keysend(&node_2, 1, true).await;
    eprintln!("payment: {:?}", payment);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let router_nodes: Vec<_> = router.router_hops.iter().map(|x| x.target).collect();
    eprintln!("router_nodes: {:?}", router_nodes);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(router_nodes, vec![node_1.pubkey, node_2.pubkey]);
    assert_eq!(amounts, vec![2, 1]);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: gen_rand_fiber_public_key(),
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: gen_rand_fiber_public_key(),
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    assert!(router.is_err());
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_build_router_multiple_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(amounts, vec![2, 1]);

    let channel_2_funding_tx = node_0.get_channel_funding_tx(&channels[2]).unwrap();
    assert_eq!(
        channel_2_funding_tx,
        router.router_hops[1].channel_outpoint.tx_hash().into(),
    );

    let channel_1_funding_tx = node_0.get_channel_funding_tx(&channels[1]).unwrap();
    let channel_1_outpoint = OutPoint::new(channel_1_funding_tx.into(), 0);
    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(channel_1_outpoint),
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    assert_eq!(amounts, vec![2, 1]);

    assert_eq!(
        channel_1_funding_tx,
        router.router_hops[1].channel_outpoint.tx_hash().into(),
    );
}

#[tokio::test]
async fn test_send_payment_build_router_pay_self() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: None,
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_0.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    eprintln!("result: {:?}", router);
    let amounts: Vec<_> = router
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();
    eprintln!("amounts: {:?}", amounts);
    assert_eq!(amounts, vec![3, 2, 1]);

    let router_nodes: Vec<_> = router.router_hops.iter().map(|x| x.target).collect();
    eprintln!("router_nodes: {:?}", router_nodes);
    assert_eq!(
        router_nodes,
        vec![node_1.pubkey, node_2.pubkey, node_0.pubkey]
    );

    let channel_1_funding_tx = node_0.get_channel_funding_tx(&channels[0]).unwrap();
    let channel_2_funding_tx = node_0.get_channel_funding_tx(&channels[2]).unwrap();
    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();
    assert_eq!(
        vec![
            channel_1_funding_tx,
            channel_2_funding_tx,
            channel_3_funding_tx
        ],
        router
            .router_hops
            .iter()
            .map(|x| x.channel_outpoint.tx_hash().into())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn test_send_payment_build_router_amount_range() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 1000, MIN_RESERVED_CKB + 1000)),
            ((1, 2), (MIN_RESERVED_CKB + 1000, MIN_RESERVED_CKB + 1000)),
            ((2, 3), (MIN_RESERVED_CKB + 1000, MIN_RESERVED_CKB + 1000)),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, _] = nodes.try_into().expect("3 nodes");

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(0), // too small
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(1001), // too large
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(1000), // add 1 as fee is too large for channel balance
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(999), // add 1 as fee is ok for channel balance
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_ok());
    let amounts: Vec<_> = router
        .unwrap()
        .router_hops
        .iter()
        .map(|x| x.amount_received)
        .collect();

    assert_eq!(amounts, vec![1000, 999]);
}

#[tokio::test]
async fn test_send_payment_with_route_to_self_with_specified_router() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    eprintln!("node_0: {:?}", node_0.pubkey);
    eprintln!("node_1: {:?}", node_1.pubkey);
    eprintln!("node_2: {:?}", node_2.pubkey);

    let node_1_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_0.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    eprintln!("result: {:?}", router);

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());

    let res = res.unwrap();
    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .assert_payment_status(payment_hash, PaymentStatus::Success, Some(1))
        .await;

    let node_0_balance1 = node_0.get_local_balance_from_channel(channels[0]);
    let node_0_balance2 = node_0.get_local_balance_from_channel(channels[2]);

    eprintln!("fee: {:?}", res.fee);
    // for node0 pay to self, only the fee will be deducted
    assert!(node_0_balance1 + node_0_balance2 == 10000000000 + 10000000000 - res.fee);

    eprintln!(
        "node1 left: {:?}, right: {:?}",
        node_1.get_local_balance_from_channel(channels[0]),
        node_1.get_local_balance_from_channel(channels[1])
    );

    let node_1_new_channel0_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_new_channel1_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel1_balance = node_2.get_local_balance_from_channel(channels[1]);
    let node_2_new_channel2_balance = node_2.get_local_balance_from_channel(channels[2]);

    // node0 can only route to self from
    // node0 -> node2 -> node1 -> node0
    let node1_fee = (node_1_new_channel1_balance - node_1_channel1_balance)
        - (node_1_channel0_balance - node_1_new_channel0_balance);

    assert!(node1_fee > 0);

    let node2_fee = (node_2_new_channel2_balance - node_2_channel2_balance)
        - (node_2_channel1_balance - node_2_new_channel1_balance);

    assert_eq!(node1_fee + node2_fee, res.fee);
}

#[tokio::test]
async fn test_send_payment_with_route_with_invalid_parameters() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 3),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, node_3] = nodes.try_into().expect("3 nodes");

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_3.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap()
        .router_hops;

    // pay to node_3 with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.clone(),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    node_0.wait_until_success(res.unwrap().payment_hash).await;

    // now we change the fee of the first channel
    let mut copy_router = router.clone();
    copy_router[1].amount_received = copy_router[0].amount_received;
    // pay to node_3 with router will be failed
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: copy_router,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    let err = res.expect_err("invalid router amount should be rejected before sending TLC");
    assert!(
        err.contains("route hop amount_received is too small for forwarding fee"),
        "unexpected error: {err}"
    );

    // ================================================================
    // now we change the expiry delta in the middle hop
    let mut copy_router = router.clone();
    copy_router[1].incoming_tlc_expiry = copy_router[0].incoming_tlc_expiry;
    // pay to node_3 with router will be failed
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: copy_router,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    let err = res.expect_err("invalid router expiry should be rejected before sending TLC");
    assert!(
        err.contains("route hop incoming_tlc_expiry is too small"),
        "unexpected error: {err}"
    );
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn test_send_payment_with_router_rpc_rejects_overflowing_onion_expiry() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters::new(
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                ChannelParameters::new(
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        3,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    node_0
        .with_network_graph_mut(|graph| graph.set_fixed_rand_expiry_delta(0))
        .await;
    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap()
        .router_hops;

    let overflow_margin = DEFAULT_TLC_EXPIRY_DELTA / 2;
    let overflowing_incoming_expiry = u64::MAX
        .checked_sub(now_timestamp_as_millis_u64())
        .and_then(|expiry| expiry.checked_sub(overflow_margin))
        .expect("current timestamp leaves room below u64::MAX");
    let mut router = router;
    router
        .get_mut(1)
        .expect("A -> B -> C route has a forwarding hop")
        .incoming_tlc_expiry = overflowing_incoming_expiry;
    let router = router.into_iter().map(JsonRouterHop::from).collect();

    let err = node_0
        .send_rpc_request::<_, GetPaymentCommandResult>(
            "send_payment_with_router",
            SendPaymentWithRouterParams {
                payment_hash: None,
                router,
                invoice: None,
                custom_records: None,
                keysend: Some(true),
                udt_type_script: None,
                dry_run: None,
            },
        )
        .await
        .expect_err("overflowing explicit router expiry should be rejected before sending TLC");
    assert!(
        err.to_string().contains("route hop incoming_tlc_expiry"),
        "unexpected error: {err:?}"
    );
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_with_route_will_not_consider_prob() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 90000, HUGE_CKB_AMOUNT)),
            ((1, 2), (MIN_RESERVED_CKB + 10000, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [mut node_0, mut node_1, mut node_2] = nodes.try_into().expect("3 nodes");

    let payment = node_0
        .send_payment_keysend(&node_2, 9000, false)
        .await
        .unwrap();
    node_0.wait_until_success(payment.payment_hash).await;

    let payment = node_0.send_payment_keysend(&node_2, 9000, false).await;
    node_0
        .wait_until_failed(payment.unwrap().payment_hash)
        .await;

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(9000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    // we don't consider the probability evaluated result
    assert!(router.is_ok());
    eprintln!("result: {:?}", router);

    // if we specify a channel, we will not consider the probability evaluated result
    // as it's user's responsibility to ensure the channel is available
    let channel_0_funding_tx = node_0.get_channel_funding_tx(&channels[0]).unwrap();
    let channel_0_outpoint = OutPoint::new(channel_0_funding_tx.into(), 0);
    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(9000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: Some(channel_0_outpoint.clone()),
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    eprintln!("result: {:?}", router);
    assert!(router.is_ok());

    let router = router.unwrap();
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_failed(payment_hash).await;

    // now we build another router from node_1 to node_2, so the capacity
    // will be enough for the payment in this network, build_router will find correct path
    let (channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node_1,
        &mut node_2,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node_1
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    // all the other nodes submit_tx
    let res = node_0.submit_tx(funding_tx.clone()).await;
    assert!(matches!(res, TxStatus::Committed(..)));
    node_0.add_channel_tx(channel_id, funding_tx_hash);

    wait_for_network_graph_update(&node_0, 3).await;

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(9000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: Some(channel_0_outpoint),
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;
    eprintln!("result: {:?}", router);
    assert!(router.is_ok());

    let router = router.unwrap();
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_with_router_with_multiple_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            // there are 3 channels from node1 -> node2
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
                },
                HopRequire {
                    pubkey: node_3.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    eprintln!("result: {:?}", router);

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;

    let payment_session = node_0
        .get_payment_session(payment_hash)
        .expect("get payment");
    eprintln!("payment_session: {:?}", &payment_session);
    let used_channels: Vec<Hash256> = payment_session
        .attempts()
        .next()
        .unwrap()
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();
    eprintln!("used_channels: {:?}", used_channels);
    assert_eq!(used_channels.len(), 4);
    assert_eq!(used_channels[1], channel_3_funding_tx);

    // try channel_2
    let channel_2_funding_tx = node_0.get_channel_funding_tx(&channels[2]).unwrap();
    eprintln!("channel_2_funding_tx: {:?}", channel_2_funding_tx);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_2_funding_tx.into(), 0)),
                },
                HopRequire {
                    pubkey: node_3.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    eprintln!("result: {:?}", router);

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    eprintln!("payment_hash: {:?}", payment_hash);
    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    eprintln!("payment_session: {:?}", &payment_session);
    let used_channels: Vec<Hash256> = payment_session
        .attempts()
        .next()
        .unwrap()
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();
    eprintln!("used_channels: {:?}", used_channels);
    assert_eq!(used_channels.len(), 4);
    assert_eq!(used_channels[1], channel_2_funding_tx);

    let wrong_channel_hash = Hash256::from([0u8; 32]);
    // if we specify a wrong funding_tx, the payment will fail
    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: Some(OutPoint::new(wrong_channel_hash.into(), 0)),
                },
                HopRequire {
                    pubkey: node_2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router
        .unwrap_err()
        .to_string()
        .contains("PathFind error: no path found"));
}

#[tokio::test]
async fn test_send_payment_two_nodes_with_router_and_multiple_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((0, 1), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((1, 0), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((1, 0), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let channel_1_funding_tx = node_0.get_channel_funding_tx(&channels[1]).unwrap();
    let channel_3_funding_tx = node_0.get_channel_funding_tx(&channels[3]).unwrap();
    let old_balance = node_0.get_local_balance_from_channel(channels[1]);
    let old_node1_balance = node_1.get_local_balance_from_channel(channels[3]);

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![
                HopRequire {
                    pubkey: node_1.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_1_funding_tx.into(), 0)),
                },
                HopRequire {
                    pubkey: node_0.pubkey,
                    channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    // pay to self with router will be OK
    let res = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    let payment_session = node_0
        .get_payment_session(payment_hash)
        .expect("get payment");

    let used_channels: Vec<Hash256> = payment_session
        .attempts()
        .next()
        .unwrap()
        .route
        .nodes
        .iter()
        .map(|x| x.channel_outpoint.tx_hash().into())
        .collect();

    assert_eq!(used_channels.len(), 3);
    assert_eq!(used_channels[0], channel_1_funding_tx);
    assert_eq!(used_channels[1], channel_3_funding_tx);

    node_0.wait_until_success(payment_hash).await;
    assert_eq!(node_0.get_inflight_payment_count().await, 0);

    let balance = node_0.get_local_balance_from_channel(channels[1]);
    assert_eq!(balance, old_balance - 60000000 - res.fee);

    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_1_balance, 60000000 + res.fee);

    let balance = node_0.get_local_balance_from_channel(channels[3]);
    assert_eq!(balance, 60000000);

    let node_1_balance = node_1.get_local_balance_from_channel(channels[3]);
    assert_eq!(node_1_balance, old_node1_balance - 60000000);
}

#[tokio::test]
async fn test_send_payment_send_with_wrong_hop() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (1, 2),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (2, 3),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
            (
                (3, 0),
                (
                    MIN_RESERVED_CKB + 10000000000,
                    MIN_RESERVED_CKB + 10000000000,
                ),
            ),
        ],
        4,
    )
    .await;
    let [node_0, node_1, _node_2, node_3] = nodes.try_into().expect("3 nodes");

    let channel_3_funding_tx = node_3.get_channel_funding_tx(&channels[3]).unwrap();

    // can not build a invalid router from node3 -> node_1
    let router = node_3
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    assert!(router.is_err());

    // build a router from node3 -> node_0
    let router = node_3
        .build_router(BuildRouterCommand {
            amount: Some(60000000),
            hops_info: vec![HopRequire {
                pubkey: node_0.pubkey,
                channel_outpoint: Some(OutPoint::new(channel_3_funding_tx.into(), 0)),
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    // pay the above router with node_3 will be ok
    let res = node_3
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops.clone(),
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

    node_3.wait_until_success(res.payment_hash).await;
    assert_eq!(node_3.get_inflight_payment_count().await, 0);

    // pay the above router with node_1 will failed
    let res = node_1
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("Failed to build route, PathFind error: no path found"));
    assert_eq!(node_1.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_network_send_payment_randomly_send_each_other() {
    init_tracing();

    let node_a_funding_amount = 100000000000;
    let node_b_funding_amount = 100000000000;

    let (node_a, node_b, new_channel_id) =
        create_nodes_with_established_channel(node_a_funding_amount, node_b_funding_amount, true)
            .await;
    let node_a_old_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let node_b_old_balance = node_b.get_local_balance_from_channel(new_channel_id);

    let mut node_a_sent = 0;
    let mut node_b_sent = 0;
    let mut all_sent = vec![];
    for _i in 1..8 {
        let rand_wait_time = rand::random::<u64>() % 100;
        tokio::time::sleep(tokio::time::Duration::from_millis(rand_wait_time)).await;

        let rand_num = rand::random::<u64>() % 2;
        let amount = rand::random::<u128>() % 10000 + 1;
        eprintln!("generated amount: {}", amount);
        let (source, target) = if rand_num == 0 {
            (&node_a, &node_b)
        } else {
            (&node_b, &node_a)
        };

        let res = source
            .send_payment_keysend(target, amount, false)
            .await
            .expect("send payment success");

        if rand_num == 0 {
            all_sent.push((true, amount, res.payment_hash, res.status));
        } else {
            all_sent.push((false, amount, res.payment_hash, res.status));
        }
    }

    // wait for all payments to be settled
    for (a_send, _, payment_hash, _) in all_sent.iter() {
        let sender = if *a_send { &node_a } else { &node_b };
        sender.wait_until_success(*payment_hash).await;
    }

    for (a_sent, amount, payment_hash, create_status) in all_sent {
        let node = if a_sent { &node_a } else { &node_b };
        let res = node.get_payment_result(payment_hash).await;
        if res.status == PaymentStatus::Success {
            assert!(matches!(
                create_status,
                PaymentStatus::Created | PaymentStatus::Inflight
            ));
            eprintln!(
                "{} payment_hash: {:?} success with amount: {} create_status: {:?}",
                if a_sent { "a -> b" } else { "b -> a" },
                payment_hash,
                amount,
                create_status
            );
            if a_sent {
                node_a_sent += amount;
            } else {
                node_b_sent += amount;
            }
        }
    }

    eprintln!(
        "node_a_old_balance: {}, node_b_old_balance: {}",
        node_a_old_balance, node_b_old_balance
    );
    eprintln!("node_a_sent: {}, node_b_sent: {}", node_a_sent, node_b_sent);
    let new_node_a_balance = node_a.get_local_balance_from_channel(new_channel_id);
    let new_node_b_balance = node_b.get_local_balance_from_channel(new_channel_id);

    eprintln!(
        "new_node_a_balance: {}, new_node_b_balance: {}",
        new_node_a_balance, new_node_b_balance
    );

    assert_eq!(
        node_a_old_balance + node_b_old_balance,
        new_node_a_balance + new_node_b_balance
    );
    assert_eq!(
        new_node_a_balance,
        node_a_old_balance - node_a_sent + node_b_sent
    );
    assert_eq!(
        new_node_b_balance,
        node_b_old_balance - node_b_sent + node_a_sent
    );
}

#[tokio::test]
async fn test_network_three_nodes_two_channels_send_each_other() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    let node_b_old_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_old_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);

    let amount_a_to_c = 60000;
    let res = node_a
        .send_payment_keysend(&node_c, amount_a_to_c, false)
        .await
        .unwrap();
    let payment_hash1 = res.payment_hash;
    let fee1 = res.fee;
    eprintln!("payment_hash1: {:?}", payment_hash1);

    let amount_c_to_a = 50000;
    let res = node_c
        .send_payment_keysend(&node_a, amount_c_to_a, false)
        .await
        .unwrap();

    let payment_hash2 = res.payment_hash;
    let fee2 = res.fee;
    eprintln!("payment_hash2: {:?}", payment_hash2);

    node_a.wait_until_success(payment_hash1).await;
    node_c.wait_until_success(payment_hash2).await;

    let new_node_b_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let new_node_b_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);

    let node_b_fee = new_node_b_balance_channel_0 + new_node_b_balance_channel_1
        - node_b_old_balance_channel_0
        - node_b_old_balance_channel_1;

    eprintln!("node_b_fee: {}", node_b_fee);
    eprintln!("fee1: {}, fee2: {}", fee1, fee2);
    assert_eq!(node_b_fee, fee1 + fee2);
}

#[tokio::test]
async fn test_network_three_nodes_send_each_other() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 0), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    // Wait for the channel announcement to be broadcasted
    let node_b_old_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let node_b_old_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);
    let node_b_old_balance_channel_2 = node_b.get_local_balance_from_channel(channels[2]);
    let node_b_old_balance_channel_3 = node_b.get_local_balance_from_channel(channels[3]);

    eprintln!(
        "node_b_old_balance_channel_0: {}, node_b_old_balance_channel_1: {}",
        node_b_old_balance_channel_0, node_b_old_balance_channel_1
    );
    eprintln!(
        "node_b_old_balance_channel_2: {}, node_b_old_balance_channel_3: {}",
        node_b_old_balance_channel_2, node_b_old_balance_channel_3
    );

    let amount_a_to_c = 60000;
    let res = node_a
        .send_payment_keysend(&node_c, amount_a_to_c, false)
        .await
        .expect("send payment ok");
    let payment_hash1 = res.payment_hash;
    let fee1 = res.fee;
    eprintln!("payment_hash1: {:?}", payment_hash1);

    let amount_c_to_a = 60000;
    let res = node_c
        .send_payment_keysend(&node_a, amount_c_to_a, false)
        .await
        .expect("send payment ok");

    let payment_hash2 = res.payment_hash;
    let fee2 = res.fee;
    eprintln!("payment_hash2: {:?}", payment_hash2);

    node_a.wait_until_success(payment_hash1).await;
    node_c.wait_until_success(payment_hash2).await;

    let new_node_b_balance_channel_0 = node_b.get_local_balance_from_channel(channels[0]);
    let new_node_b_balance_channel_1 = node_b.get_local_balance_from_channel(channels[1]);
    let new_node_b_balance_channel_2 = node_b.get_local_balance_from_channel(channels[2]);
    let new_node_b_balance_channel_3 = node_b.get_local_balance_from_channel(channels[3]);

    let node_b_fee = new_node_b_balance_channel_0
        + new_node_b_balance_channel_1
        + new_node_b_balance_channel_2
        + new_node_b_balance_channel_3
        - node_b_old_balance_channel_0
        - node_b_old_balance_channel_1
        - node_b_old_balance_channel_2
        - node_b_old_balance_channel_3;

    eprintln!("node_b_fee: {}", node_b_fee);
    assert_eq!(node_b_fee, fee1 + fee2);
}

#[tokio::test]
async fn test_send_payment_bench_test() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, node_1, node_2, node_3] = nodes.try_into().expect("3 nodes");

    let mut all_sent = HashSet::new();

    for i in 1..=15 {
        assert!(node_0.get_triggered_unexpected_events().await.is_empty());
        assert!(node_1.get_triggered_unexpected_events().await.is_empty());
        assert!(node_2.get_triggered_unexpected_events().await.is_empty());
        assert!(node_3.get_triggered_unexpected_events().await.is_empty());

        if let Ok(payment) = node_0.send_payment_keysend(&node_3, 100, false).await {
            eprintln!("payment: {:?}", payment);
            all_sent.insert(payment.payment_hash);
            info!("send: {} payment_hash: {:?} sent", i, payment.payment_hash);
        }
    }

    let time = std::time::Instant::now();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        for payment_hash in all_sent.clone().iter() {
            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("got payment: {:?} status: {:?}", payment_hash, status);
            if status == PaymentStatus::Success {
                eprintln!("payment_hash: {:?} success", payment_hash);
                all_sent.remove(payment_hash);
                info!(
                    "payment_hash: {:?} success, left: {:?}",
                    payment_hash,
                    all_sent.len()
                );
            }
        }

        if all_sent.is_empty() {
            break;
        }
        if time.elapsed().as_secs() >= 300 {
            panic!("timeout, not all payments are settled");
        }
    }
}

#[tokio::test]
async fn test_send_payment_three_nodes_wait_succ_bench_test() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let mut all_sent = vec![];

    for i in 1..=10 {
        let payment = node_0
            .send_payment_keysend(&node_2, 1000, false)
            .await
            .unwrap();
        all_sent.push(payment.payment_hash);
        eprintln!(
            "send: {} payment_hash: {:?} sentxx",
            i, payment.payment_hash
        );

        node_0.wait_until_success(payment.payment_hash).await;
    }
}

#[tokio::test]
async fn test_send_payment_three_nodes_send_each_other_bench_test() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let mut all_sent = vec![];

    for i in 1..=5 {
        let payment1 = node_0
            .send_payment_keysend(&node_2, 1000, false)
            .await
            .unwrap();
        all_sent.push(payment1.payment_hash);
        eprintln!("send: {} payment_hash: {:?} sent", i, payment1.payment_hash);

        let payment2 = node_2
            .send_payment_keysend(&node_0, 1000, false)
            .await
            .unwrap();
        all_sent.push(payment2.payment_hash);
        eprintln!("send: {} payment_hash: {:?} sent", i, payment2.payment_hash);

        node_0.wait_until_success(payment1.payment_hash).await;
        node_2.wait_until_success(payment2.payment_hash).await;
    }
}

#[tokio::test]
async fn test_send_payment_three_nodes_send_each_other_no_wait() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;

    let mut all_sent = vec![];
    let node_0_balance = nodes[0].get_local_balance_from_channel(channels[0]);
    let node_2_balance = nodes[2].get_local_balance_from_channel(channels[1]);

    let amount = 100000;
    let mut node_0_sent_fee = 0;
    let mut node_0_sent_amount = 0;
    let mut node_2_sent_fee = 0;
    let mut node_2_sent_amount = 0;
    for _i in 0..4 {
        for _k in 0..3 {
            let payment1 = nodes[0]
                .send_payment_keysend(&nodes[2], amount, false)
                .await
                .unwrap();
            eprintln!(
                "send: {} payment_hash: {:?} sent, fee: {:?}",
                _i, payment1.payment_hash, payment1.fee
            );
            node_0_sent_fee += payment1.fee;
            node_0_sent_amount += amount;
            all_sent.push((0, payment1.payment_hash));
        }

        let payment2 = nodes[2]
            .send_payment_keysend(&nodes[0], amount, false)
            .await
            .unwrap();
        all_sent.push((2, payment2.payment_hash));
        eprintln!(
            "send: {} payment_hash: {:?} sent, fee: {:?}",
            _i, payment2.payment_hash, payment2.fee
        );
        node_2_sent_fee += payment2.fee;
        node_2_sent_amount += amount;
    }

    loop {
        for (node_index, payment_hash) in all_sent.clone().iter() {
            let node = &nodes[*node_index];
            node.wait_until_success(*payment_hash).await;
            all_sent.retain(|x| x.1 != *payment_hash);
        }
        if all_sent.is_empty() {
            break;
        }
    }
    let new_node_0_balance = nodes[0].get_local_balance_from_channel(channels[0]);
    let new_node_2_balance = nodes[2].get_local_balance_from_channel(channels[1]);
    eprintln!(
        "node_0_balance: {}, new_node_0_balance: {}, node_0_sent_amount: {}, node_0_sent_fee: {}",
        node_0_balance, new_node_0_balance, node_0_sent_amount, node_0_sent_fee,
    );
    eprintln!(
        "node_2_balance: {}, new_node_2_balance: {}, node_2_sent_amount: {}, node_2_sent_fee: {}",
        node_2_balance, new_node_2_balance, node_2_sent_amount, node_2_sent_fee
    );
    assert_eq!(
        new_node_0_balance,
        node_0_balance - node_0_sent_fee - 8 * amount
    );
    assert_eq!(
        new_node_2_balance,
        node_2_balance - node_2_sent_fee + 8 * amount
    );
}

#[tokio::test]
async fn test_send_payment_three_nodes_bench_test() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;

    let mut all_sent = HashSet::new();
    let mut node_2_got_fee = 0;
    let mut node1_got_amount = 0;
    let mut node_1_sent_fee = 0;
    let mut node3_got_amount = 0;
    let mut node_3_sent_fee = 0;
    let mut node_2_ch1_sent_amount = 0;
    let mut node_2_ch2_sent_amount = 0;

    let old_node_1_amount = nodes[0].get_local_balance_from_channel(channels[0]);
    let old_node_2_chnnale1_amount = nodes[1].get_local_balance_from_channel(channels[0]);
    let old_node_2_chnnale2_amount = nodes[1].get_local_balance_from_channel(channels[1]);
    let old_node_3_amount = nodes[2].get_local_balance_from_channel(channels[1]);

    for i in 1..=4 {
        let payment1 = nodes[0]
            .send_payment_keysend(&nodes[2], 1000, false)
            .await
            .unwrap();
        all_sent.insert((1, payment1.payment_hash, payment1.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment1.payment_hash);
        node_1_sent_fee += payment1.fee;
        node_2_got_fee += payment1.fee;

        let payment2 = nodes[1]
            .send_payment_keysend(&nodes[2], 1000, false)
            .await
            .unwrap();
        all_sent.insert((2, payment2.payment_hash, payment2.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment2.payment_hash);
        node_2_ch1_sent_amount += 1000;
        node1_got_amount += 1000;

        let payment3 = nodes[1]
            .send_payment_keysend(&nodes[0], 1000, false)
            .await
            .unwrap();
        all_sent.insert((2, payment3.payment_hash, payment3.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment3.payment_hash);
        node_2_ch2_sent_amount += 1000;
        node3_got_amount += 1000;

        let payment4 = nodes[2]
            .send_payment_keysend(&nodes[0], 1000, false)
            .await
            .unwrap();
        all_sent.insert((3, payment4.payment_hash, payment4.fee));
        eprintln!("send: {} payment_hash: {:?} sent", i, payment4.payment_hash);
        assert!(payment4.fee > 0);
        node_3_sent_fee += payment4.fee;
        node_2_got_fee += payment4.fee;
    }

    loop {
        for (node_index, payment_hash, fee) in all_sent.clone().iter() {
            nodes[*node_index - 1]
                .wait_until_success(*payment_hash)
                .await;
            all_sent.remove(&(*node_index, *payment_hash, *fee));
        }
        let res = nodes[0].node_info().await;
        eprintln!("node1 node_info: {:?}", res);
        let res = nodes[1].node_info().await;
        eprintln!("node2 node_info: {:?}", res);
        let res = nodes[2].node_info().await;
        eprintln!("node3 node_info: {:?}", res);
        if all_sent.is_empty() {
            break;
        }
    }

    eprintln!("node_2_got_fee: {}", node_2_got_fee);
    eprintln!("node1_got_amount: {}", node1_got_amount);
    eprintln!("node3_got_amount: {}", node3_got_amount);

    // node1: sent 4 fee to node2, got 4000 from node2
    // node3: sent 4 fee to node2, got 4000 from node2
    // node2: got 8 from node1 and node3, sent 8000 to node1 and node3

    let node_1_amount = nodes[0].get_local_balance_from_channel(channels[0]);
    let node_2_chnnale1_amount = nodes[1].get_local_balance_from_channel(channels[0]);
    let node_2_chnnale2_amount = nodes[1].get_local_balance_from_channel(channels[1]);
    let node_3_amount = nodes[2].get_local_balance_from_channel(channels[1]);

    let node_1_amount_diff = node_1_amount - old_node_1_amount;
    let node_2_chnnale1_amount_diff = old_node_2_chnnale1_amount - node_2_chnnale1_amount;
    let node_2_chnnale2_amount_diff = old_node_2_chnnale2_amount - node_2_chnnale2_amount;
    let node_3_amount_diff = node_3_amount - old_node_3_amount;

    assert_eq!(node_1_amount_diff, node1_got_amount - node_1_sent_fee);
    // got 3996

    assert_eq!(
        node_2_chnnale1_amount_diff,
        node_2_ch1_sent_amount - node_1_sent_fee
    );
    // sent 3996

    assert_eq!(
        node_2_chnnale2_amount_diff,
        node_2_ch2_sent_amount - node_3_sent_fee
    );
    // sent 3996

    assert_eq!(node_3_amount_diff, node3_got_amount - node_3_sent_fee);
    // got 3996
}

#[tokio::test]
async fn test_send_payment_middle_hop_stopped() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        5,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3, mut node_4] = nodes.try_into().expect("5 nodes");

    // dry run node_0 -> node_3 will select  0 -> 4 -> 3
    let res = node_0
        .send_payment_keysend(&node_3, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 1);

    // node_4 stopped
    node_4.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // when node_4 stopped, node 0 learned that channel 0 -> 4 was not available
    // so it will try another path 0 -> 1 -> 2 -> 3
    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 3);

    node_0.wait_until_success(res.payment_hash).await;

    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_middle_hop_stopped_retry_longer_path() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((5, 6), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((6, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        7,
    )
    .await;
    let [node_0, _node_1, mut node_2, mut node_3, _node_4, _node_5, _node_6] =
        nodes.try_into().expect("7 nodes");

    // dry run node_0 -> node_3 will select  0 -> 1 -> 2 -> 3
    let res = node_0
        .send_payment_keysend(&node_3, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 3);
    node_0.expect_router_used_channel(&res, channels[1]).await;

    // node_2 stopped
    node_2.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let res = node_0
        .send_payment_keysend(&node_3, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    // when node_2 stopped, the first try path is still 0 -> 1 -> 2 -> 3
    // so the fee is 3
    assert_eq!(res.fee, 3);
    node_0.expect_router_used_channel(&res, channels[1]).await;

    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 3);

    node_0.wait_until_success(res.payment_hash).await;

    let payment = node_0.get_payment_result(res.payment_hash).await;
    eprintln!("payment: {:?}", payment);

    // payment success with a longer path 0 -> 4 -> 5 -> 6 -> 3
    assert_eq!(payment.fee, 5);
    node_0
        .expect_payment_used_channel(res.payment_hash, channels[5])
        .await;

    // node_3 stopped, payment will fail
    node_3.stop().await;
    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();

    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 5);

    node_0.wait_until_failed(res.payment_hash).await;
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_max_value_in_flight_in_first_hop() {
    // https://github.com/nervosnetwork/fiber/issues/450

    init_tracing();

    let nodes = NetworkNode::new_interconnected_nodes(2, None).await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");
    let (_channel_id, _funding_tx_hash) = {
        establish_channel_between_nodes(
            &mut node_0,
            &mut node_1,
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                b_max_tlc_value_in_flight: Some(100000000),
                ..Default::default()
            },
        )
        .await
    };

    let res = node_0
        .send_payment_keysend(&node_1, 100000000 + 1, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 0);

    let payment_hash = res.payment_hash;
    node_0.wait_until_failed(payment_hash).await;

    // now we can not send payment with amount 100000000 + 1 with dry_run
    // since there is already payment history data
    let res = node_0
        .send_payment_keysend(&node_1, 100000000 + 1, true)
        .await;
    eprintln!("res: {:?}", res);
    assert!(res.unwrap_err().to_string().contains("no path found"));

    // if we build a nother channel with higher max_value_in_flight
    // we can send payment with amount 100000000 + 1 with this new channel
    let (channel_id, _funding_tx_hash) = {
        establish_channel_between_nodes(
            &mut node_0,
            &mut node_1,
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                b_max_tlc_value_in_flight: Some(100000000 + 2),
                ..Default::default()
            },
        )
        .await
    };

    let res = node_0
        .send_payment_keysend(&node_1, 100000000 + 1, false)
        .await
        .unwrap();

    let payment_hash = res.payment_hash;
    node_0.wait_until_success(payment_hash).await;
    node_0
        .expect_payment_used_channel(payment_hash, channel_id)
        .await;
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_with_router_to_offline_channel_fails_fast() {
    init_tracing();

    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(1000),
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .expect("build direct router");

    node_1.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let error = node_0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(error.contains("Failed to build route, PathFind error: no path found"));
    assert_eq!(node_0.get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_target_hop_stopped() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((3, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        5,
    )
    .await;
    let [node_0, _node_1, _node_2, _node_3, mut node_4] = nodes.try_into().expect("5 nodes");

    // dry run node_0 -> node_4 will select  0 -> 1 -> 2 -> 3 -> 4
    let res = node_0
        .send_payment_keysend(&node_4, 1000, true)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    assert_eq!(res.fee, 5);

    // node_4 stopped
    node_4.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let res = node_0
        .send_payment_keysend(&node_4, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    // when node_4 stopped, the first try path is still 0 -> 1 -> 2 -> 3 -> 4
    // so the fee is 5
    assert_eq!(res.fee, 5);

    node_0.wait_until_failed(res.payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_middle_hop_balance_is_not_enough() {
    // https://github.com/nervosnetwork/fiber/issues/286
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (MIN_RESERVED_CKB, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, _node_2, node_3] = nodes.try_into().expect("3 nodes");

    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);

    // path is still 0 -> 1 -> 2 -> 3,
    // 2 -> 3 don't have enough balance
    node_0.wait_until_failed(res.payment_hash).await;
    let result = node_0.get_payment_result(res.payment_hash).await;
    eprintln!("debug result: {:?}", result);
    assert!(result
        .failed_error
        .expect("got error")
        .contains("Failed to build route"));
}

#[tokio::test]
async fn test_send_payment_middle_hop_update_fee_send_payment_failed() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    // node_2 update fee rate to a higher one, so the payment will fail
    let res = node_0
        .send_payment_keysend(&node_3, 1000, false)
        .await
        .unwrap();
    eprintln!("res: {:?}", res);
    let payment_hash = res.payment_hash;

    node_2
        .update_channel_with_command(
            channels[2],
            UpdateCommand {
                enabled: None,
                tlc_expiry_delta: None,
                tlc_minimum_value: None,
                tlc_fee_proportional_millionths: Some(100000),
            },
        )
        .await;

    node_0.wait_until_failed(payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_middle_hop_update_fee_multiple_payments() {
    // https://github.com/nervosnetwork/fiber/issues/480
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();

    for _i in 0..5 {
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], 1000, false)
            .await
            .unwrap();
        all_sent.insert(res.payment_hash);
    }

    nodes[2]
        .update_channel_with_command(
            channels[2],
            UpdateCommand {
                enabled: None,
                tlc_expiry_delta: None,
                tlc_minimum_value: None,
                tlc_fee_proportional_millionths: Some(100000),
            },
        )
        .await;

    loop {
        for i in 0..4 {
            assert!(nodes[i].get_triggered_unexpected_events().await.is_empty());
        }

        for payment_hash in all_sent.clone().iter() {
            nodes[0].wait_until_final_status(*payment_hash).await;
            let status = nodes[0].get_payment_status(*payment_hash).await;
            //eprintln!("got payment: {:?} status: {:?}", payment_hash, status);
            if status == PaymentStatus::Failed || status == PaymentStatus::Success {
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                all_sent.remove(payment_hash);
            }
        }
        if all_sent.is_empty() {
            break;
        }
    }
}

#[tokio::test]
async fn test_send_payment_middle_hop_update_fee_should_recovery() {
    // a variant test from
    // https://github.com/nervosnetwork/fiber/issues/480
    // in this test, we will make sure the payment should recovery after the fee is updated by the middle hop
    // there are two channels between node_1 and node_2, they are with the same fee rate
    // path finding will pick the channel with latest time, so channels[2] will be picked
    // but we will update the fee rate of channels[2] to a higher one
    // so the payment will fail, but after the payment failed, the path finding should pick the channels[1] in the next try
    // in the end, all the payments should success
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let mut all_sent = HashSet::new();

    let tx_count = 6;
    for _i in 0..tx_count {
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], 1000, false)
            .await
            .unwrap();
        all_sent.insert(res.payment_hash);
    }

    nodes[1]
        .update_channel_with_command(
            channels[2],
            UpdateCommand {
                enabled: None,
                tlc_expiry_delta: None,
                tlc_minimum_value: None,
                tlc_fee_proportional_millionths: Some(100000),
            },
        )
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let mut succ_count = 0;
    loop {
        for i in 0..4 {
            assert!(nodes[i].get_triggered_unexpected_events().await.is_empty());
        }

        for payment_hash in all_sent.clone().iter() {
            nodes[0].wait_until_final_status(*payment_hash).await;
            let status = nodes[0].get_payment_status(*payment_hash).await;
            if status == PaymentStatus::Success || status == PaymentStatus::Failed {
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                all_sent.remove(payment_hash);
                if status == PaymentStatus::Success {
                    succ_count += 1;
                }
            }
        }
        if all_sent.is_empty() {
            break;
        }
    }

    assert_eq!(succ_count, tx_count);
    let channel_state = nodes[0].get_channel_actor_state(channels[0]);
    assert_eq!(channel_state.get_offered_tlc_balance().unwrap(), 0);
}

async fn run_complex_network_with_params(
    funding_amount: u128,
    payment_amount_gen: impl Fn() -> u128,
) -> Vec<(Hash256, PaymentStatus)> {
    init_tracing();

    let nodes_num = 6;
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            ((3, 4), (funding_amount, funding_amount)),
            ((4, 5), (funding_amount, funding_amount)),
            ((0, 3), (funding_amount, funding_amount)),
            ((1, 4), (funding_amount, funding_amount)),
            ((2, 5), (funding_amount, funding_amount)),
        ],
        nodes_num,
    )
    .await;

    let mut all_sent = HashSet::new();
    for _k in 0..2 {
        for i in 0..nodes_num {
            let payment_amount = payment_amount_gen();
            let res = nodes[i]
                .send_payment_keysend_to_self(payment_amount, false)
                .await;
            if let Ok(res) = res {
                let payment_hash = res.payment_hash;
                all_sent.insert((i, payment_hash));
            }
        }
    }

    let mut result = vec![];
    loop {
        for i in 0..nodes_num {
            let unexpected_events = nodes[i].get_triggered_unexpected_events().await;
            if !unexpected_events.is_empty() {
                eprintln!("node_{} got unexpected events: {:?}", i, unexpected_events);
                unreachable!("unexpected events");
            }
        }

        for (i, payment_hash) in all_sent.clone().into_iter() {
            nodes[i].wait_until_final_status(payment_hash).await;
            let status = nodes[i].get_payment_status(payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if matches!(status, PaymentStatus::Success | PaymentStatus::Failed) {
                result.push((payment_hash, status));
                all_sent.remove(&(i, payment_hash));
            }
        }
        if all_sent.is_empty() {
            break;
        }
    }

    // make sure all the channels are still workable with small accounts
    for i in 0..nodes_num {
        if let Ok(res) = nodes[i].send_payment_keysend_to_self(500, false).await {
            nodes[i].wait_until_success(res.payment_hash).await;
        }

        assert_eq!(nodes[i].get_inflight_payment_count().await, 0);
    }

    result
}

#[tokio::test]
async fn test_send_payment_self_with_two_nodes() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (funding_amount, funding_amount)),
            ((1, 0), (funding_amount, funding_amount)),
        ],
        2,
    )
    .await;

    let old_balance0 = nodes[0].get_local_balance_from_channel(channels[0]);
    let old_balance1 = nodes[0].get_local_balance_from_channel(channels[1]);
    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    nodes[0].wait_until_success(payment_hash).await;
    let balance0 = nodes[0].get_local_balance_from_channel(channels[0]);
    let balance1 = nodes[0].get_local_balance_from_channel(channels[1]);

    eprintln!("old_balance: {}, new_balance: {}", old_balance0, balance0);
    eprintln!("old_balance1: {}, new_balance1: {}", old_balance1, balance1);
    let fee = old_balance0 + old_balance1 - balance0 - balance1;
    assert_eq!(fee, 1);

    // single edge network payself will fail
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (funding_amount, funding_amount))], 2).await;
    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_err());
    assert_eq!(nodes[0].get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_self_with_mixed_channel() {
    // #678, payself with mixed channel got wrong
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_err());

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let res = nodes[0].send_payment_keysend_to_self(1000, false).await;
    assert!(res.is_ok());
    nodes[0].wait_until_success(res.unwrap().payment_hash).await;

    // all UDT channels
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    funding_udt_type_script: Some(Script::default()),
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let _res = nodes[0]
        .assert_send_payment_success(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            udt_type_script: Some(Script::default()),
            ..Default::default()
        })
        .await;
    assert_eq!(nodes[0].get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_with_invalid_tlc_expiry() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[(
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: funding_amount,
                node_b_funding_amount: funding_amount,
                ..Default::default()
            },
        )],
        2,
        None,
    )
    .await;

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[1].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            tlc_expiry_limit: Some(10), // too small than MIN_TLC_EXPIRY_DELTA
            ..Default::default()
        })
        .await;
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("tlc_expiry_limit is too small"));

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[1].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            tlc_expiry_limit: Some(MIN_TLC_EXPIRY_DELTA + 1), // still too small
            ..Default::default()
        })
        .await;
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("tlc_expiry_limit is too small"));

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[1].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            tlc_expiry_limit: Some(DEFAULT_FINAL_TLC_EXPIRY_DELTA + DEFAULT_TLC_EXPIRY_DELTA),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    nodes[0].wait_until_success(res.unwrap().payment_hash).await;
    assert_eq!(nodes[0].get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payself_with_invalid_tlc_expiry() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    b_tlc_expiry_delta: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // a too large value
                    ..Default::default()
                },
            ),
            (
                (1, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_expiry_delta: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // a too large value
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    // no tlc_expiry_limit will also fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            tlc_expiry_limit: Some(DEFAULT_FINAL_TLC_EXPIRY_DELTA),
            ..Default::default()
        })
        .await;

    assert!(res.unwrap_err().to_string().contains("no path found"));
    assert_eq!(nodes[0].get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payself_with_single_limit_tlc_expiry() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    ..Default::default()
                },
            ),
            (
                (1, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_expiry_delta: Some(DEFAULT_TLC_EXPIRY_DELTA + 1), // a large value
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            allow_self_payment: true,
            tlc_expiry_limit: Some(MAX_PAYMENT_TLC_EXPIRY_LIMIT),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    assert_eq!(nodes[0].get_inflight_payment_count().await, 1);
}

#[tokio::test]
async fn test_send_payself_with_small_min_tlc_value() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    b_tlc_min_value: Some(100), // a small value
                    ..Default::default()
                },
            ),
            (
                (1, 0),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_min_value: Some(100), // a small value
                    ..Default::default()
                },
            ),
        ],
        2,
        None,
    )
    .await;

    // too small amount will fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(99),
            keysend: Some(true),
            allow_self_payment: true,
            ..Default::default()
        })
        .await;

    assert!(res.unwrap_err().to_string().contains("no path found"));
    assert_eq!(nodes[0].get_inflight_payment_count().await, 0);

    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[0].pubkey),
            amount: Some(100),
            keysend: Some(true),
            allow_self_payment: true,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    assert_eq!(nodes[0].get_inflight_payment_count().await, 1);
}

#[tokio::test]
async fn test_send_payment_with_middle_hop_with_min_tlc_value() {
    init_tracing();

    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_min_value: Some(100),
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: funding_amount,
                    node_b_funding_amount: funding_amount,
                    a_tlc_min_value: Some(50),
                    ..Default::default()
                },
            ),
        ],
        3,
        None,
    )
    .await;

    // too small amount will fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[2].pubkey),
            amount: Some(40),
            keysend: Some(true),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    // too small amount will fail
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[2].pubkey),
            amount: Some(60),
            keysend: Some(true),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_err());

    // normal amount will success
    let res = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[2].pubkey),
            amount: Some(110),
            keysend: Some(true),
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    assert_eq!(nodes[0].get_inflight_payment_count().await, 0);
}

#[tokio::test]
async fn test_send_payment_complex_network_payself_all_succeed() {
    // from issue 475
    // channel amount is enough, so all payments should success
    let res = run_complex_network_with_params(MIN_RESERVED_CKB + 100000000, || 1000).await;
    let failed_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentStatus::Failed)
        .count();

    assert_eq!(failed_count, 0);
}

#[tokio::test]
async fn test_send_payment_complex_network_payself_amount_exceeded() {
    // variant from issue 475
    // the channel amount is not enough, so payments maybe be failed
    let ckb_unit = 100_000_000;
    let res = run_complex_network_with_params(MIN_RESERVED_CKB + 1000 * ckb_unit, || {
        (400_u128 + (rand::random::<u64>() % 100) as u128) * ckb_unit
    })
    .await;

    // some may failed and some may success
    let failed_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentStatus::Failed)
        .count();
    assert!(failed_count > 0);
    let succ_count = res
        .iter()
        .filter(|(_, status)| *status == PaymentStatus::Success)
        .count();
    assert!(succ_count > 0);
}

#[tokio::test]
async fn test_send_payment_with_one_node_stop() {
    // make sure part of the payments will fail, since the node is stopped
    // TLC forwarding will fail and proper error will be returned
    // There is also a probability that RemoveTlc can not be passed backwardly,
    // since the node is stopped, so the payment will be Inflight state.
    init_tracing();

    let (mut nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();
    for i in 0..10 {
        let res = nodes[0].send_payment_keysend(&nodes[3], 1000, false).await;
        if let Ok(send_payment_res) = res {
            if i > 5 {
                all_sent.insert(send_payment_res.payment_hash);
            }
        }

        if i == 5 {
            let _ = nodes[3].stop().await;
        }
    }

    let mut failed_count = 0;
    let mut check_count = 0;
    while check_count < 100 {
        for payment_hash in all_sent.clone().iter() {
            let res = nodes[0].get_payment_result(*payment_hash).await;
            eprintln!("payment_hash: {:?} status: {:?}", payment_hash, res.status);
            if res.status == PaymentStatus::Failed {
                failed_count += 1;
                all_sent.remove(payment_hash);
            }
        }
        check_count += 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if all_sent.is_empty() {
            break;
        }
    }
    assert!(failed_count >= 4);
}

#[tokio::test]
async fn test_send_payment_shutdown_with_force() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let mut all_sent = HashSet::new();
    for i in 0..10 {
        let res = node_0.send_payment_keysend(&node_3, 1000, false).await;
        if let Ok(send_payment_res) = res {
            if i > 5 {
                all_sent.insert(send_payment_res.payment_hash);
            }
        }

        if i == 5 {
            let _ = node_3.send_shutdown(channels[2], true).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            node_3
                .send_channel_shutdown_tx_confirmed_event(node_2.pubkey, channels[2], true)
                .await;
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let channel_actor_state = node_3.get_channel_actor_state(channels[2]);
            assert!(matches!(
                channel_actor_state.state,
                ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
            ));
        }
    }

    // make sure the later payments will fail
    // because network actor will find out the inactive channels and shutdown channel forcefully
    let mut wait_time = 0;
    while wait_time < PEER_CHANNEL_RESPONSE_TIMEOUT + 3 {
        let channel_state = node_2.get_channel_actor_state(channels[2]);
        if matches!(
            channel_state.state,
            ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
        ) {
            break;
        } else {
            assert!(matches!(
                channel_state.state,
                ChannelState::ChannelReady | ChannelState::ShuttingDown(_)
            ));
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
            wait_time += 1000;
        }
    }
}

#[tokio::test]
async fn test_send_payment_shutdown_channel_actor_may_already_stopped() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    for i in 0..2 {
        let _ = nodes[i].send_shutdown(channels[i], true).await;

        // send multiple shutdown transaction confirmed events
        for _k in 0..5 {
            nodes[i]
                .send_channel_shutdown_tx_confirmed_event(nodes[i + 1].pubkey, channels[i], true)
                .await;
        }
        let channel_actor_state = nodes[i].get_channel_actor_state(channels[i]);
        assert!(matches!(
            channel_actor_state.state,
            ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
        ));
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_closed_channel_upstream_settlement_does_not_depend_on_check_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build hold invoice");
    node_2.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    let payment = node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    assert_eq!(payment.payment_hash, payment_hash);
    node_0.wait_until_inflight(payment_hash).await;

    wait_until(|| {
        node_1
            .get_channel_actor_state(channels[1])
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .any(|tlc| tlc.payment_hash == payment_hash)
    })
    .await;

    let mut closed_downstream_state = node_1.get_channel_actor_state(channels[1]);
    let downstream_tlc = closed_downstream_state
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .cloned()
        .expect("downstream tlc exists");
    let (previous_channel_id, previous_tlc_id) = downstream_tlc
        .forwarding_tlc
        .expect("downstream tlc should track the upstream forwarding tlc");
    assert_eq!(previous_channel_id, channels[0]);

    node_1
        .send_shutdown(channels[1], true)
        .await
        .expect("force shutdown downstream channel");

    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    closed_downstream_state = node_1.get_channel_actor_state(channels[1]);
    closed_downstream_state
        .tlc_state
        .get_mut(&TLCId::Offered(downstream_tlc.id()))
        .expect("closed downstream tlc exists")
        .expiry = now_timestamp_as_millis_u64().saturating_sub(1);
    node_1
        .update_channel_actor_state(
            closed_downstream_state,
            Some(ReloadParams {
                notify_changes: false,
            }),
        )
        .await;

    node_1.store.insert_onchain_tlc_settlement(
        &fiber_types::NodeId::local(),
        &channels[1],
        TLCId::Offered(downstream_tlc.id()),
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: gen_rand_sha256_hash(),
            tlc_index: 0,
        },
    );

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    tokio::time::timeout(
        Duration::from_millis(800),
        node_0.wait_until_failed(payment_hash),
    )
    .await
    .expect("closed channel actor should fail the upstream payment without CheckChannels");

    assert_eq!(
        node_0.get_payment_status(payment_hash).await,
        PaymentStatus::Failed
    );
    assert!(matches!(
        node_1
            .get_tlc(channels[0], TLCId::Received(previous_tlc_id))
            .and_then(|tlc| tlc.removed_reason),
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));
}

// When the downstream force-closed channel reveals a preimage on-chain, the watchtower stores it
// in the watch-preimage table. The forwarding node must read that preimage and fulfill (not fail)
// the upstream TLC so the payment succeeds. The success path keys off the on-chain preimage marker
// only and must not depend on the `WithoutPreimage` on-chain fail marker.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_closed_channel_upstream_fulfillment_from_onchain_preimage() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    // Use a hold invoice so node_2 keeps the downstream TLC pending until we force-close, leaving an
    // offered TLC on node_1's downstream channel that can only be resolved on-chain.
    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build hold invoice");
    node_2.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    let payment = node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    assert_eq!(payment.payment_hash, payment_hash);
    node_0.wait_until_inflight(payment_hash).await;

    wait_until(|| {
        node_1
            .get_channel_actor_state(channels[1])
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .any(|tlc| tlc.payment_hash == payment_hash)
    })
    .await;

    let closed_downstream_state = node_1.get_channel_actor_state(channels[1]);
    let downstream_tlc = closed_downstream_state
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .cloned()
        .expect("downstream tlc exists");
    let (previous_channel_id, previous_tlc_id) = downstream_tlc
        .forwarding_tlc
        .expect("downstream tlc should track the upstream forwarding tlc");
    assert_eq!(previous_channel_id, channels[0]);

    node_1
        .send_shutdown(channels[1], true)
        .await
        .expect("force shutdown downstream channel");

    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    // Simulate watchtower on-chain preimage discovery: only the preimage is stored, NOT the
    // `WithoutPreimage` (no-preimage) marker. The two are mutually exclusive; writing the settled
    // marker here would instead trip the fail path and drop the preimage.
    insert_onchain_preimage(&node_1.store, &channels[1], payment_hash, hold_preimage);

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    // node_0 (the payer) only reaches Success once it receives a RemoveTlcFulfill on channels[0]
    // from node_1, which proves the forwarding node fulfilled (not failed) the upstream TLC using
    // the on-chain preimage.
    wait_until_timeout(30_000, || {
        node_0
            .get_payment_session(payment_hash)
            .is_some_and(|session| session.status == PaymentStatus::Success)
    })
    .await;

    assert_eq!(
        node_0.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );
    // The upstream TLC must never be failed; once the fulfill handshake confirms, the TLC is pruned
    // from state, so a remaining record (if any) must carry the fulfill reason.
    assert!(matches!(
        node_1
            .get_tlc(channels[0], TLCId::Received(previous_tlc_id))
            .and_then(|tlc| tlc.removed_reason),
        None | Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));
}

// When the payer's own channel is force-closed and the downstream hop claims the offered TLC
// on-chain by revealing the preimage, the payer's watchtower stores it. The payer must read that
// preimage, mark its offered TLC fulfilled in channel state, and drive the payment session to
// Success (rather than letting it linger until it fails).
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_payer_payment_success_from_onchain_preimage() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    // Hold invoice keeps the TLC pending so the payer's offered TLC can only be resolved on-chain.
    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_1.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;
    wait_for_tlc_sync(&node_0, &node_1, channels[0], 1).await;

    let offered_tlc_id = node_0
        .get_channel_actor_state(channels[0])
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| tlc.tlc_id)
        .expect("payer offered tlc exists");
    assert!(matches!(offered_tlc_id, TLCId::Offered(_)));
    assert_ne!(
        node_0
            .get_tlc(channels[0], offered_tlc_id)
            .expect("payer offered tlc exists")
            .outbound_status(),
        fiber_types::OutboundTlcStatus::LocalAnnounced,
        "TLC must be committed before force-close so on-chain fulfillment can settle it"
    );

    node_0
        .send_shutdown(channels[0], true)
        .await
        .expect("force shutdown payer channel");
    wait_until(|| {
        matches!(
            node_0.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    // Simulate the watchtower observing the preimage on-chain on the payer's own channel.
    insert_onchain_preimage(&node_0.store, &channels[0], payment_hash, hold_preimage);

    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_0
            .get_payment_session(payment_hash)
            .is_some_and(|session| session.status == PaymentStatus::Success)
    })
    .await;
    assert_eq!(
        node_0.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );

    // The offered TLC is marked fulfilled in the payer's (force-closed) channel state.
    assert!(matches!(
        node_0
            .get_tlc(channels[0], offered_tlc_id)
            .and_then(|tlc| tlc.removed_reason),
        Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));

    let preimage_record = crate::store::store_impl::KeyValue::Preimage(payment_hash, hold_preimage);
    let persisted_preimage = fiber_store::backend::StorageBackend::get(
        &node_0.store,
        crate::store::store_impl::StoreKeyValue::key(&preimage_record),
    );
    assert_eq!(
        persisted_preimage,
        Some(crate::store::store_impl::StoreKeyValue::value(
            &preimage_record
        )),
        "on-chain payment success must persist a normal preimage record so CCH observes the same success signal as off-chain fulfillment"
    );
}

#[cfg(feature = "watchtower")]
struct MppRemoteRemovedPayerFixture {
    payer: NetworkNode,
    _payee: NetworkNode,
    stuck_channel_actor: ractor::ActorRef<ChannelActorMessage>,
    stuck_channel_id: Hash256,
    payment_hash: Hash256,
    payment_preimage: Hash256,
    completed_attempt_id: Option<u64>,
    stuck_attempt_id: Option<u64>,
    stuck_tlc_id: u64,
    retry_channel_ids: Vec<Hash256>,
}

#[cfg(feature = "watchtower")]
impl MppRemoteRemovedPayerFixture {
    fn attempt_status(&self, attempt_id: Option<u64>) -> Option<AttemptStatus> {
        self.payer
            .get_payment_session(self.payment_hash)
            .and_then(|session| {
                session
                    .attempts()
                    .find(|attempt| Some(attempt.id) == attempt_id)
                    .map(|attempt| attempt.status)
            })
    }

    fn attempt_statuses(&self) -> Vec<(u64, AttemptStatus)> {
        self.payer
            .get_payment_session(self.payment_hash)
            .expect("payer payment session exists")
            .attempts()
            .map(|attempt| (attempt.id, attempt.status))
            .collect()
    }

    fn stuck_tlc(&self) -> TlcInfo {
        self.payer
            .get_tlc(self.stuck_channel_id, TLCId::Offered(self.stuck_tlc_id))
            .expect("remote-removed payer TLC exists")
    }

    fn notify_maintain_channel_tlcs(&self) {
        self.payer
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                    channel_id: self.stuck_channel_id,
                    command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
                }),
            ))
            .expect("network actor alive");
    }

    fn insert_onchain_settlement(&self) {
        insert_onchain_preimage(
            &self.payer.store,
            &self.stuck_channel_id,
            self.payment_hash,
            self.payment_preimage,
        );
    }

    async fn channel_barrier(&self) {
        ractor::call_t!(
            self.stuck_channel_actor.clone(),
            |reply| ChannelActorMessage::Command(ChannelCommand::TestBarrier(reply)),
            5_000
        )
        .expect("stuck channel actor must process the barrier");
    }

    fn assert_channel_running(&self) {
        assert_eq!(
            self.stuck_channel_actor.get_status(),
            ractor::ActorStatus::Running,
            "reconciliation must not crash the stuck channel actor"
        );
    }

    async fn reaches_success(&self) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.payer.get_payment_status(self.payment_hash).await == PaymentStatus::Success
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok()
    }

    fn assert_pre_reconcile_state(&self) {
        assert_eq!(
            self.attempt_status(self.completed_attempt_id),
            Some(AttemptStatus::Success)
        );
        assert_eq!(
            self.attempt_status(self.stuck_attempt_id),
            Some(AttemptStatus::Inflight)
        );
        let tlc = self.stuck_tlc();
        assert_eq!(tlc.outbound_status(), OutboundTlcStatus::RemoteRemoved);
        assert!(matches!(
            tlc.removed_reason,
            Some(RemoveTlcReason::RemoveTlcFulfill(..))
        ));
        assert_eq!(tlc.removed_confirmed_at, None);
    }
}

#[cfg(feature = "watchtower")]
async fn setup_mpp_remote_removed_payer_fixture() -> MppRemoteRemovedPayerFixture {
    setup_mpp_remote_removed_payer_fixture_with_parts(2).await
}

#[cfg(feature = "watchtower")]
async fn setup_mpp_remote_removed_payer_fixture_with_parts(
    payment_parts: usize,
) -> MppRemoteRemovedPayerFixture {
    setup_mpp_remote_removed_payer_fixture_with_retry_channels(payment_parts, 0).await
}

#[cfg(feature = "watchtower")]
async fn setup_mpp_remote_removed_payer_fixture_with_retry_channels(
    payment_parts: usize,
    retry_channel_count: usize,
) -> MppRemoteRemovedPayerFixture {
    assert!(
        payment_parts >= 2,
        "fixture requires at least two MPP parts"
    );
    let part_amount = 10_000_000_000;
    let total_amount = part_amount * payment_parts as u128;
    let channel_specs =
        vec![((0, 1), (MIN_RESERVED_CKB + part_amount, MIN_RESERVED_CKB)); payment_parts];
    let (nodes, channels) = create_n_nodes_network(&channel_specs, 2).await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let payment_preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(total_amount))
        .payment_preimage(payment_preimage)
        .payee_pub_key(node_1.pubkey.into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build MPP hold invoice");
    node_1.insert_invoice(invoice.clone(), None);

    let payment_hash = *invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            max_parts: Some(payment_parts as u64),
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send MPP payment");
    node_0.wait_until_inflight(payment_hash).await;

    wait_until_timeout(30_000, || {
        channels.iter().all(|channel_id| {
            node_0
                .get_channel_actor_state(*channel_id)
                .tlc_state
                .offered_tlcs
                .tlcs
                .iter()
                .any(|tlc| {
                    tlc.payment_hash == payment_hash
                        && tlc.outbound_status() == OutboundTlcStatus::Committed
                })
        })
    })
    .await;

    let offered_tlc = |channel_id| {
        node_0
            .get_channel_actor_state(channel_id)
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .find(|tlc| tlc.payment_hash == payment_hash)
            .cloned()
            .expect("payer offered MPP TLC exists")
    };
    let channel_tlcs = channels
        .iter()
        .map(|channel_id| offered_tlc(*channel_id))
        .collect::<Vec<_>>();
    let stuck_channel_index = channel_tlcs
        .iter()
        .position(|tlc| tlc.attempt_id == Some(1))
        .expect("the first MPP attempt id is allocated from one");
    let completed_channel_index = (0..channel_tlcs.len())
        .find(|index| *index != stuck_channel_index)
        .expect("fixture has another MPP part to complete normally");
    let completed_tlc = channel_tlcs[completed_channel_index].clone();
    let stuck_tlc = channel_tlcs[stuck_channel_index].clone();
    let stuck_channel_id = channels[stuck_channel_index];
    assert_ne!(completed_tlc.attempt_id, stuck_tlc.attempt_id);

    // Model the online split that completed normally. This leaves the aggregate MPP session
    // Inflight because the force-closed split has not delivered its payer completion event.
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::TlcRemoveReceived(
                payment_hash,
                completed_tlc.attempt_id,
                RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
            ),
        ))
        .expect("network actor alive");
    wait_until_timeout(10_000, || {
        node_0
            .get_payment_session(payment_hash)
            .is_some_and(|session| {
                session.status == PaymentStatus::Inflight
                    && session
                        .attempts()
                        .any(|attempt| attempt.status == AttemptStatus::Success)
            })
    })
    .await;
    let TLCId::Offered(stuck_tlc_id) = stuck_tlc.tlc_id else {
        panic!("payer TLC must be offered");
    };
    // Deliver the fulfill through the real peer-message handler. The test-only lookup only
    // exposes the live actor selected by channel id; all state transitions remain production code.
    let stuck_channel_actor = node_0
        .get_channel_actor(stuck_channel_id)
        .await
        .expect("stuck payer channel actor is live");
    stuck_channel_actor
        .send_message(ChannelActorMessage::PeerMessage(
            FiberChannelMessage::RemoveTlc(RemoveTlc {
                channel_id: stuck_channel_id,
                tlc_id: stuck_tlc_id,
                reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
            }),
        ))
        .expect("stuck payer channel actor alive");
    wait_until_timeout(10_000, || {
        node_0
            .get_tlc(stuck_channel_id, TLCId::Offered(stuck_tlc_id))
            .is_some_and(|tlc| {
                tlc.outbound_status() == OutboundTlcStatus::RemoteRemoved
                    && matches!(
                        tlc.removed_reason,
                        Some(RemoveTlcReason::RemoveTlcFulfill(..))
                    )
            })
    })
    .await;
    assert_eq!(
        node_0.store.get_preimage(&payment_hash),
        Some(payment_preimage),
        "the peer-message handler must persist the learned preimage"
    );

    // Model A observing B's already-broadcast force-close before the remove commitment handshake
    // reaches `apply_remove_tlc_operation`. This is a legitimate transition from ChannelReady and
    // leaves the peer-message-produced RemoteRemoved state durable on the closed channel.
    let tx_hash = TransactionBuilder::default().build().hash();
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ClosingTransactionConfirmed(
                node_1.pubkey,
                stuck_channel_id,
                tx_hash,
                true,
                false,
            ),
        ))
        .expect("payer network actor alive");
    wait_until_timeout(10_000, || {
        matches!(
            node_0.get_channel_actor_state(stuck_channel_id).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    // Retry-specific tests need unused first-hop capacity that did not participate in the old
    // payment. Opening these channels only after the old MPP parts are fixed guarantees that a
    // later replacement attempt is backed by a real new TLC on a different channel.
    let mut retry_channel_ids = Vec::with_capacity(retry_channel_count);
    for _ in 0..retry_channel_count {
        let (channel_id, _) = establish_channel_between_nodes(
            &mut node_0,
            &mut node_1,
            ChannelParameters {
                a_max_tlc_value_in_flight: Some(total_amount),
                b_max_tlc_value_in_flight: Some(total_amount),
                ..ChannelParameters::new(MIN_RESERVED_CKB + total_amount, MIN_RESERVED_CKB)
            },
        )
        .await;
        retry_channel_ids.push(channel_id);
    }
    if retry_channel_count > 0 {
        wait_for_network_graph_update(&node_0, payment_parts + retry_channel_count).await;
    }

    // The synthetic payee is normally no longer part of the scenario after delivering the
    // fulfill. Retry tests keep it online only so production route finding and AddTlc can create a
    // real replacement TLC on the freshly opened channel.
    if retry_channel_count == 0 {
        node_1.stop().await;
    }

    let fixture = MppRemoteRemovedPayerFixture {
        payer: node_0,
        _payee: node_1,
        stuck_channel_actor,
        stuck_channel_id,
        payment_hash,
        payment_preimage,
        completed_attempt_id: completed_tlc.attempt_id,
        stuck_attempt_id: stuck_tlc.attempt_id,
        stuck_tlc_id,
        retry_channel_ids,
    };
    fixture.assert_pre_reconcile_state();
    fixture
}

// Reproduces the payer-side state reported by the PR #1512 integration test: one MPP part has
// completed off-chain, while the other part learned the fulfill from its peer but force-closed
// before the removal was commitment-confirmed. The latter TLC is therefore already
// `RemoteRemoved` with a fulfill reason, but its payment attempt is still `Inflight`. Once the
// same preimage is verified on-chain, reconciliation must deliver the missing payer completion
// event without trying to mark the TLC removed a second time.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_mpp_payer_remote_removed_attempt_succeeds_from_onchain_preimage() {
    init_tracing();

    let fixture = setup_mpp_remote_removed_payer_fixture().await;
    fixture.insert_onchain_settlement();
    fixture.notify_maintain_channel_tlcs();
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;

    let reached_success = fixture.reaches_success().await;
    fixture.assert_channel_running();
    assert!(
        reached_success,
        "on-chain confirmation of the RemoteRemoved MPP split must complete the payer payment; status={:?}, attempts={:?}",
        fixture.payer.get_payment_status(fixture.payment_hash).await,
        fixture.attempt_statuses()
    );
}

// The fulfill and attempt state are durable. If the payer restarts before reconciliation, the
// no-live-channel fallback must recover the same missing completion event from persisted state.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_mpp_payer_remote_removed_attempt_succeeds_after_restart() {
    init_tracing();

    let mut fixture = setup_mpp_remote_removed_payer_fixture().await;
    fixture.payer.restart().await;
    fixture.stuck_channel_actor = fixture
        .payer
        .get_channel_actor(fixture.stuck_channel_id)
        .await
        .expect("restart restores the closed watch-chain actor");
    fixture.assert_pre_reconcile_state();

    // Stop the restored watch-chain actor so CheckChannels must use the persisted-state fallback,
    // matching a restart window where the channel actor is unavailable.
    fixture
        .payer
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: fixture.stuck_channel_id,
                command: ChannelCommand::NotifyEvent(ChannelEvent::Stop(StopReason::Closed)),
            }),
        ))
        .expect("network actor alive");
    wait_until_timeout(10_000, || {
        fixture.stuck_channel_actor.get_status() == ractor::ActorStatus::Stopped
    })
    .await;
    // The stopped actor enqueues ChannelActorStopped from post_stop. This NetworkActor RPC is a
    // barrier proving that event was consumed and the channel was removed from the live map.
    fixture.payer.node_info().await;
    assert!(
        fixture
            .payer
            .get_channel_actor(fixture.stuck_channel_id)
            .await
            .is_none(),
        "CheckChannels must exercise the no-live-channel fallback"
    );
    fixture.assert_pre_reconcile_state();

    fixture.insert_onchain_settlement();
    fixture
        .payer
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::CheckChannels,
        ))
        .expect("network actor alive");
    fixture.payer.node_info().await;

    let reached_success = fixture.reaches_success().await;
    assert!(
        reached_success,
        "restart/no-live reconciliation must complete the RemoteRemoved MPP split; status={:?}, attempts={:?}",
        fixture.payer.get_payment_status(fixture.payment_hash).await,
        fixture.attempt_statuses()
    );
}

// Watchtower and startup scans can report the same settlement repeatedly. After the first
// successful reconciliation, subsequent scans must not emit another payer completion event or
// mutate the already-reconciled TLC/payment state.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_mpp_payer_remote_removed_onchain_reconciliation_is_idempotent() {
    init_tracing();

    let mut fixture = setup_mpp_remote_removed_payer_fixture().await;
    fixture
        .payer
        .add_unexpected_events(vec!["panic".to_string(), "panicked".to_string()])
        .await;
    fixture.insert_onchain_settlement();
    fixture.notify_maintain_channel_tlcs();
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;
    assert!(
        fixture.reaches_success().await,
        "first reconciliation must complete the payer before idempotency can be checked; status={:?}, attempts={:?}",
        fixture.payer.get_payment_status(fixture.payment_hash).await,
        fixture.attempt_statuses()
    );
    fixture.assert_channel_running();

    let tlc_after_first_scan = fixture.stuck_tlc();
    let attempts_after_first_scan = fixture.attempt_statuses();
    tokio::time::sleep(Duration::from_millis(200)).await;
    while fixture.payer.event_emitter.try_recv().is_ok() {}

    for _ in 0..3 {
        fixture.notify_maintain_channel_tlcs();
    }
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;
    fixture.assert_channel_running();
    // Any duplicate TlcRemoveReceived emitted by the ChannelActor is queued back to the
    // NetworkActor. Cross that mailbox as well, then give the event-forwarding task a bounded
    // window to publish the corresponding debug notification before asserting its absence.
    fixture.payer.node_info().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut duplicate_completion_events = 0;
    let mut duplicate_payment_actor_starts = 0;
    while let Ok(event) = fixture.payer.event_emitter.try_recv() {
        if let NetworkServiceEvent::DebugEvent(DebugEvent::Common(message)) = event {
            if message.starts_with("after on_remove_tlc_event session_status:") {
                duplicate_completion_events += 1;
            } else if message.starts_with("payment actor start:") {
                duplicate_payment_actor_starts += 1;
            }
        }
    }
    assert_eq!(
        duplicate_completion_events, 0,
        "repeated settlement scans must not emit duplicate TlcRemoveReceived events"
    );
    assert_eq!(
        duplicate_payment_actor_starts, 0,
        "repeated settlement scans must not restart an already-completed payment actor"
    );
    assert_eq!(fixture.stuck_tlc(), tlc_after_first_scan);
    assert_eq!(fixture.attempt_statuses(), attempts_after_first_scan);
    assert_eq!(
        fixture.payer.get_payment_status(fixture.payment_hash).await,
        PaymentStatus::Success
    );
    assert!(
        fixture
            .payer
            .get_triggered_unexpected_events()
            .await
            .is_empty(),
        "repeated reconciliation must not panic"
    );
}

// Attempt and aggregate session records are separate writes. If the node crashes after writing
// the successful attempt but before writing the successful session, loading the session will
// optimistically recompute Success from its attempts even though the durable session record is
// still Inflight. Reconciliation must not mistake that computed value for a completed previous
// run: after restart it must resume the PaymentActor and finish the session write.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_mpp_payer_onchain_reconciliation_repairs_partial_session_commit_after_restart() {
    init_tracing();

    let mut fixture = setup_mpp_remote_removed_payer_fixture().await;
    let attempt_id = fixture
        .stuck_attempt_id
        .expect("remote-removed payer TLC has an attempt id");
    let mut attempt = fixture
        .payer
        .store
        .get_attempt(fixture.payment_hash, attempt_id)
        .expect("remote-removed payer attempt exists");
    attempt.set_success_status();
    attempt.preimage = Some(fixture.payment_preimage);
    fixture.payer.store.insert_attempt(attempt);

    assert_eq!(
        fixture
            .payer
            .store
            .get_persisted_payment_status(fixture.payment_hash),
        Some(PaymentStatus::Inflight),
        "the fixture must model a crash before the aggregate session write"
    );
    assert_eq!(
        fixture
            .payer
            .get_payment_session(fixture.payment_hash)
            .expect("payer payment session exists")
            .status,
        PaymentStatus::Success,
        "normal session loading must expose why the persisted status needs a separate check"
    );

    fixture.payer.restart().await;
    fixture.stuck_channel_actor = fixture
        .payer
        .get_channel_actor(fixture.stuck_channel_id)
        .await
        .expect("restart restores the closed watch-chain actor");
    while fixture.payer.event_emitter.try_recv().is_ok() {}

    fixture.insert_onchain_settlement();
    fixture.notify_maintain_channel_tlcs();
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;
    wait_until_timeout(10_000, || {
        fixture
            .payer
            .store
            .get_persisted_payment_status(fixture.payment_hash)
            == Some(PaymentStatus::Success)
    })
    .await;

    fixture.payer.node_info().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut payment_actor_starts = 0;
    while let Ok(event) = fixture.payer.event_emitter.try_recv() {
        if matches!(
            event,
            NetworkServiceEvent::DebugEvent(DebugEvent::Common(message))
                if message.starts_with("payment actor start:")
        ) {
            payment_actor_starts += 1;
        }
    }
    assert_eq!(
        payment_actor_starts, 1,
        "the incomplete session commit must pass through the PaymentActor exactly once"
    );
}

// A failed payment retry deletes its old attempts and allocates attempt ids from one again. The
// old force-closed channel can still retain an on-chain-confirmed TLC with one of those ids. That
// proof belongs to both the old attempt generation and the old first-hop channel, so it must not
// update a replacement attempt that happens to reuse `(payment_hash, attempt_id)` on another
// channel.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_mpp_payer_old_onchain_tlc_does_not_reconcile_reused_attempt_id() {
    init_tracing();

    let fixture = setup_mpp_remote_removed_payer_fixture_with_retry_channels(3, 1).await;
    let payment_hash = fixture.payment_hash;
    let stale_attempt_id = fixture
        .stuck_attempt_id
        .expect("remote-removed payer TLC has an attempt id");

    // First reconcile the old source TLC normally. This keeps the stale TLC and its attempt fully
    // consistent: both now record Success with the same preimage, while the third MPP shard keeps
    // the aggregate payment Inflight.
    fixture.insert_onchain_settlement();
    fixture.notify_maintain_channel_tlcs();
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;
    fixture.payer.node_info().await;
    let stale_attempt = fixture
        .payer
        .store
        .get_attempt(payment_hash, stale_attempt_id)
        .expect("stale payer attempt exists");
    assert_eq!(stale_attempt.status, AttemptStatus::Success);
    assert_eq!(stale_attempt.preimage, Some(fixture.payment_preimage));

    // Model the independent third shard exhausting its retries after two shards have succeeded.
    // A partial-success MPP can therefore be terminal Failed without contradicting the stale
    // fulfilled TLC that will continue to be scanned on the force-closed channel.
    let completed_attempt_id = fixture
        .completed_attempt_id
        .expect("normally completed payer TLC has an attempt id");
    let mut failed_attempt = fixture
        .payer
        .get_payment_session(payment_hash)
        .expect("old payer payment session exists")
        .attempts()
        .find(|attempt| attempt.id != stale_attempt_id && attempt.id != completed_attempt_id)
        .cloned()
        .expect("the third MPP shard is still in flight");
    assert_eq!(failed_attempt.status, AttemptStatus::Inflight);
    failed_attempt.set_failed_status("third MPP shard exhausted its retries", false);
    fixture.payer.store.insert_attempt(failed_attempt);

    // Model only the durable terminal outcome of the old generation. From this point onward the
    // retry itself is entirely production code: the old actor stops, send_payment deletes the old
    // attempts, allocates ids from one, builds fresh routes, and adds fresh first-hop TLCs.
    let mut failed_session = fixture
        .payer
        .get_payment_session(payment_hash)
        .expect("old payer payment session exists");
    let retry_invoice = failed_session
        .request
        .invoice
        .clone()
        .expect("MPP fixture pays an invoice");
    failed_session.set_failed_status("third MPP shard exhausted its retries");
    fixture.payer.store.insert_payment_session(failed_session);

    let payment_actor_name = format!(
        "Payment-{} Node({:?})",
        payment_hash,
        fixture.payer.network_actor.get_name()
    );
    let old_payment_actor: ractor::ActorRef<PaymentActorMessage> =
        ractor::registry::where_is(payment_actor_name.clone())
            .expect("old payment actor is still running")
            .into();
    old_payment_actor
        .send_message(PaymentActorMessage::CheckPaymentStatus)
        .expect("old payment actor accepts its final status check");
    wait_until_timeout(10_000, || {
        old_payment_actor.get_status() == ractor::ActorStatus::Stopped
    })
    .await;
    wait_until_timeout(10_000, || {
        ractor::registry::where_is(payment_actor_name.clone()).is_none()
    })
    .await;
    fixture.payer.node_info().await;

    crate::invoice::InvoiceStore::update_invoice_status(
        &fixture._payee.store,
        &payment_hash,
        CkbInvoiceStatus::Open,
    )
    .expect("reopen the hold invoice for the production retry");
    fixture
        .payer
        .send_payment(SendPaymentCommand {
            invoice: Some(retry_invoice),
            max_parts: Some(3),
            ..Default::default()
        })
        .await
        .expect("retry the failed MPP payment through production send_payment");
    wait_until_timeout(10_000, || {
        fixture
            .payer
            .get_payment_session(payment_hash)
            .is_some_and(|session| {
                session.status == PaymentStatus::Inflight
                    && session.attempts().count() == 1
                    && session
                        .attempts()
                        .all(|attempt| attempt.status == AttemptStatus::Inflight)
            })
    })
    .await;

    let replacement_attempt = fixture
        .payer
        .store
        .get_attempt(payment_hash, stale_attempt_id)
        .expect("the retry reuses the old attempt id");
    assert_ne!(
        stale_attempt.first_hop_channel_outpoint(),
        replacement_attempt.first_hop_channel_outpoint(),
        "the reused attempt id must now belong to another first-hop channel"
    );
    assert!(
        fixture.retry_channel_ids.iter().any(|channel_id| {
            fixture
                .payer
                .get_channel_actor_state(*channel_id)
                .tlc_state
                .offered_tlcs
                .tlcs
                .iter()
                .any(|tlc| {
                    tlc.payment_hash == payment_hash && tlc.attempt_id == Some(stale_attempt_id)
                })
        }),
        "the replacement attempt must have a real offered TLC on a fresh retry channel"
    );

    fixture.notify_maintain_channel_tlcs();
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;
    fixture.payer.node_info().await;

    let replacement_after_scan = fixture
        .payer
        .store
        .get_attempt(payment_hash, stale_attempt_id)
        .expect("replacement attempt must remain present");
    assert_eq!(
        replacement_after_scan.status,
        AttemptStatus::Inflight,
        "an old channel's on-chain TLC must not fulfill a reused attempt id from another channel"
    );
    assert_eq!(
        replacement_after_scan.preimage, None,
        "the stale on-chain preimage must not be attached to the replacement attempt"
    );
}

// Per-attempt reconciliation must be idempotent independently of the aggregate payment status.
// One MPP shard can be durably fulfilled on-chain while the requested amount is still incomplete.
// After the PaymentActor stops (as it does on its periodic non-final status check or a restart), a
// repeated settlement scan should acknowledge the already-reconciled shard without starting a new
// actor.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_mpp_payer_partial_onchain_reconciliation_does_not_restart_payment_actor() {
    init_tracing();

    // Three real route parts keep the aggregate payment incomplete after the first two parts
    // succeed: one completed normally, one is reconciled below, and one remains in flight.
    let mut fixture = setup_mpp_remote_removed_payer_fixture_with_parts(3).await;
    let attempt_id = fixture
        .stuck_attempt_id
        .expect("remote-removed payer TLC has an attempt id");

    fixture.insert_onchain_settlement();
    fixture.notify_maintain_channel_tlcs();
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;
    let reconciled_attempt = fixture
        .payer
        .store
        .get_attempt(fixture.payment_hash, attempt_id)
        .expect("the reconciled attempt is durable");
    assert_eq!(reconciled_attempt.status, AttemptStatus::Success);
    assert_eq!(
        reconciled_attempt.preimage,
        Some(fixture.payment_preimage),
        "the first scan must durably reconcile the exact on-chain shard"
    );
    assert_eq!(
        fixture
            .payer
            .store
            .get_persisted_payment_status(fixture.payment_hash),
        Some(PaymentStatus::Inflight),
        "the third real MPP shard keeps the aggregate payment incomplete"
    );
    assert_eq!(
        fixture
            .payer
            .get_payment_session(fixture.payment_hash)
            .expect("payer payment session exists")
            .attempts()
            .filter(|attempt| attempt.status == AttemptStatus::Inflight)
            .count(),
        1,
        "exactly one real MPP shard must still be in flight"
    );

    let payment_actor_name = format!(
        "Payment-{} Node({:?})",
        fixture.payment_hash,
        fixture.payer.network_actor.get_name()
    );
    let payment_actor: ractor::ActorRef<PaymentActorMessage> =
        ractor::registry::where_is(payment_actor_name.clone())
            .expect("the partial payment actor is still running after reconciliation")
            .into();
    payment_actor
        .send_message(PaymentActorMessage::CheckPaymentStatus)
        .expect("partial payment actor accepts its periodic status check");
    wait_until_timeout(10_000, || {
        payment_actor.get_status() == ractor::ActorStatus::Stopped
    })
    .await;
    fixture.payer.node_info().await;
    assert!(
        ractor::registry::where_is(payment_actor_name).is_none(),
        "the stopped payment actor must be removed before replaying the scan"
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    while fixture.payer.event_emitter.try_recv().is_ok() {}
    fixture.notify_maintain_channel_tlcs();
    fixture.payer.node_info().await;
    fixture.channel_barrier().await;
    fixture.payer.node_info().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut duplicate_payment_actor_starts = 0;
    while let Ok(event) = fixture.payer.event_emitter.try_recv() {
        if matches!(
            event,
            NetworkServiceEvent::DebugEvent(DebugEvent::Common(message))
                if message.starts_with("payment actor start:")
        ) {
            duplicate_payment_actor_starts += 1;
        }
    }
    assert_eq!(
        duplicate_payment_actor_starts, 0,
        "an already-reconciled partial MPP shard must not restart PaymentActor on every scan"
    );
}

// When the payee's channel is force-closed with a still-pending received TLC, the payee claims it
// on-chain with the invoice preimage. Once the watchtower observes that on-chain settlement, the
// payee must mark the received TLC fulfilled in channel state and, since the invoice is now fully
// paid, move the invoice to `Paid`.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_payee_invoice_paid_from_onchain_preimage() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    // Hold invoice so the payee keeps the received TLC pending until on-chain settlement.
    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_1.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    wait_for_tlc_sync(&node_0, &node_1, channels[0], 1).await;
    let received_tlc_id = node_1
        .get_channel_actor_state(channels[0])
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| tlc.tlc_id)
        .expect("payee received tlc exists");
    assert!(matches!(received_tlc_id, TLCId::Received(_)));
    assert_ne!(
        node_1.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid)
    );

    node_1
        .send_shutdown(channels[0], true)
        .await
        .expect("force shutdown payee channel");
    wait_until_timeout(10_000, || {
        matches!(
            node_1.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    // Simulate the watchtower observing the payee's on-chain claim (preimage revealed on-chain).
    insert_onchain_preimage(&node_1.store, &channels[0], payment_hash, hold_preimage);

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Paid)
    })
    .await;

    // The received TLC is marked fulfilled in the payee's (force-closed) channel state.
    assert!(matches!(
        node_1
            .get_tlc(channels[0], received_tlc_id)
            .and_then(|tlc| tlc.removed_reason),
        Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));
}

// Mirrors the CCH receive-btc force-close timing: the channel settlement completion can be
// observed before the payee's on-chain preimage claim is discovered. The invoice must still
// converge to Paid once the later on-chain preimage settlement is recorded.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_payee_invoice_paid_when_onchain_preimage_arrives_after_settlement_completion() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_1.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    wait_for_tlc_sync(&node_0, &node_1, channels[0], 1).await;

    let received_tlc_id = node_1
        .get_channel_actor_state(channels[0])
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| tlc.tlc_id)
        .expect("payee received tlc exists");

    node_1
        .send_shutdown(channels[0], true)
        .await
        .expect("force shutdown payee channel");
    wait_until_timeout(10_000, || {
        matches!(
            node_1.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channels[0]),
        ))
        .expect("network actor alive");
    wait_until_timeout(10_000, || {
        matches!(
            node_1.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
                    && flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED)
        )
    })
    .await;
    assert_ne!(
        node_1.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid),
        "the invoice cannot be marked paid until an on-chain preimage settlement is recorded"
    );

    insert_onchain_preimage(&node_1.store, &channels[0], payment_hash, hold_preimage);
    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Paid)
    })
    .await;
    assert!(matches!(
        node_1
            .get_tlc(channels[0], received_tlc_id)
            .and_then(|tlc| tlc.removed_reason),
        Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));
}

// Mirrors the receive-btc hold-invoice path: the payee created an invoice with only a payment
// hash, the peer force-closed before the preimage was revealed, and the payee calls settle_invoice
// only after the channel is already waiting for on-chain settlement.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_hold_invoice_paid_when_settled_after_remote_force_close_and_onchain_preimage() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = HashAlgorithm::Sha256.hash(payment_preimage.as_ref()).into();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_hash(payment_hash)
        .hash_algorithm(HashAlgorithm::Sha256)
        .payee_pub_key(node_1.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;
    wait_until_timeout(30_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    wait_for_tlc_sync(&node_0, &node_1, channels[0], 1).await;

    let received_tlc_id = node_1
        .get_channel_actor_state(channels[0])
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| tlc.tlc_id)
        .expect("payee received tlc exists");

    node_0
        .send_shutdown(channels[0], true)
        .await
        .expect("peer force shutdown channel");
    let tx_hash = TransactionBuilder::default().build().hash();
    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ClosingTransactionConfirmed(
                node_0.pubkey,
                channels[0],
                tx_hash,
                true,
                false,
            ),
        ))
        .expect("node_1 network actor alive");
    wait_until_timeout(30_000, || {
        matches!(
            node_1.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    node_1
        .settle_invoice(&payment_hash, payment_preimage)
        .await
        .expect("settle invoice after remote force close");
    assert_ne!(
        node_1.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid),
        "local preimage reveal alone must not mark a force-closed received TLC paid"
    );

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channels[0]),
        ))
        .expect("network actor alive");
    wait_until_timeout(10_000, || {
        matches!(
            node_1.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
                    && flags.contains(CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED)
        )
    })
    .await;

    insert_onchain_preimage(&node_1.store, &channels[0], payment_hash, payment_preimage);
    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Paid)
    })
    .await;
    assert!(matches!(
        node_1
            .get_tlc(channels[0], received_tlc_id)
            .and_then(|tlc| tlc.removed_reason),
        Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));
}

// Mirrors the E2E timing where the payee reveals the preimage locally after the peer has already
// force-closed, but before the payee observes the close on chain. The received TLC is already
// locally fulfilled, so later on-chain reconciliation must still settle the invoice once the
// channel-scoped on-chain preimage proof appears.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_hold_invoice_paid_when_onchain_preimage_confirms_already_removed_received_tlc() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = HashAlgorithm::Sha256.hash(payment_preimage.as_ref()).into();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_hash(payment_hash)
        .hash_algorithm(HashAlgorithm::Sha256)
        .payee_pub_key(node_1.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;
    wait_until_timeout(30_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    wait_for_tlc_sync(&node_0, &node_1, channels[0], 1).await;

    let received_tlc_id = node_1
        .get_channel_actor_state(channels[0])
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| tlc.tlc_id)
        .expect("payee received tlc exists");
    let TLCId::Received(received_tlc_index) = received_tlc_id else {
        panic!("payee tlc must be received");
    };

    let mut actor_state = node_1.get_channel_actor_state(channels[0]);
    actor_state.tlc_state.set_received_tlc_removed(
        received_tlc_index,
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
    );
    actor_state.state = ChannelState::Closed(
        CloseFlags::UNCOOPERATIVE_REMOTE
            | CloseFlags::WAITING_ONCHAIN_SETTLEMENT
            | CloseFlags::ONCHAIN_SETTLEMENT_CONFIRMED,
    );
    node_1
        .update_channel_actor_state(
            actor_state,
            Some(ReloadParams {
                notify_changes: false,
            }),
        )
        .await;

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::SettleOnChainFulfilledInvoice(payment_hash),
        ))
        .expect("network actor alive");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_ne!(
        node_1.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid),
        "a local RemoveTlcFulfill without on-chain settlement evidence must not mark the invoice paid"
    );

    insert_onchain_preimage(&node_1.store, &channels[0], payment_hash, payment_preimage);
    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Paid)
    })
    .await;
    assert!(matches!(
        node_1
            .get_tlc(channels[0], received_tlc_id)
            .and_then(|tlc| tlc.removed_reason),
        Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));
}

// On-chain fulfillment can happen independently on multiple force-closed channels for a single
// MPP invoice. The invoice should be marked paid once all fulfilled parts across channels satisfy
// the total amount; checking only the current channel would leave it stuck at Received/Open.
#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_payee_mpp_invoice_paid_from_onchain_preimages_across_channels() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10_000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10_000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20_000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_1.pubkey.into())
        .allow_mpp(true)
        .payment_secret(payment_secret)
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build MPP hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;
    let mut custom_records = PaymentCustomRecords::default();
    BasicMppPaymentData::new(payment_secret, 20_000).write(&mut custom_records);
    let tlc_expiry =
        now_timestamp_as_millis_u64() + DEFAULT_FINAL_TLC_EXPIRY_DELTA + DEFAULT_TLC_EXPIRY_DELTA;
    let hops_infos = vec![
        PaymentHopData {
            amount: 10_000,
            expiry: tlc_expiry,
            next_hop: Some(node_1.pubkey),
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
        PaymentHopData {
            amount: 10_000,
            expiry: tlc_expiry,
            hash_algorithm,
            custom_records: Some(custom_records),
            ..Default::default()
        },
    ];
    let packet = PeeledPaymentOnionPacket::create(
        node_0.get_private_key().clone(),
        hops_infos,
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    for channel_id in channels.iter().copied() {
        call!(node_0.network_actor, |rpc_reply| {
            NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::AddTlc(
                        AddTlcCommand {
                            amount: 10_000,
                            hash_algorithm,
                            payment_hash,
                            expiry: tlc_expiry,
                            onion_packet: packet.next.clone(),
                            shared_secret: packet.shared_secret,
                            is_trampoline_hop: false,
                            previous_tlc: None,
                            attempt_id: None,
                        },
                        rpc_reply,
                    ),
                },
            ))
        })
        .expect("node alive")
        .expect("add MPP TLC");
    }

    wait_until_timeout(30_000, || {
        node_1.store.get_payment_hold_tlcs(payment_hash).len() == 2
    })
    .await;

    for channel_id in channels.iter().copied() {
        node_1
            .send_shutdown(channel_id, true)
            .await
            .expect("force shutdown payee channel");
        wait_until_timeout(10_000, || {
            matches!(
                node_1.get_channel_actor_state(channel_id).state,
                ChannelState::Closed(flags)
                    if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
            )
        })
        .await;
    }

    for channel_id in channels.iter() {
        insert_onchain_preimage(&node_1.store, channel_id, payment_hash, hold_preimage);
    }

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                channel_id: channels[1],
                command: ChannelCommand::NotifyEvent(ChannelEvent::MaintainChannelTlcs),
            }),
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Paid)
    })
    .await;
}

#[tokio::test]
async fn test_forwarded_payment_relays_remove_to_upstream() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, node_2] = nodes.try_into().expect("3 nodes");

    let payment_preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(payment_preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build invoice");
    let payment_hash = *invoice.payment_hash();
    node_2.insert_invoice(invoice.clone(), Some(payment_preimage));

    let payment = node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send forwarded payment");
    assert_eq!(payment.payment_hash, payment_hash);

    wait_until_timeout(30_000, || {
        node_0
            .get_payment_session(payment_hash)
            .is_some_and(|session| session.status == PaymentStatus::Success)
    })
    .await;
    assert_eq!(
        node_0.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_onchain_settlement_restart_restores_upstream_waiting_commitment_actor() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, mut node_1, node_2] = nodes.try_into().expect("3 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build hold invoice");
    node_2.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    let payment = node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    assert_eq!(payment.payment_hash, payment_hash);
    node_0.wait_until_inflight(payment_hash).await;

    wait_until(|| {
        node_1
            .get_channel_actor_state(channels[1])
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .any(|tlc| tlc.payment_hash == payment_hash)
    })
    .await;

    let closed_downstream_state = node_1.get_channel_actor_state(channels[1]);
    let downstream_tlc = closed_downstream_state
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .cloned()
        .expect("downstream tlc exists");
    let (previous_channel_id, previous_tlc_id) = downstream_tlc
        .forwarding_tlc
        .expect("downstream tlc should track the upstream forwarding tlc");
    assert_eq!(previous_channel_id, channels[0]);

    node_1
        .send_shutdown(channels[1], true)
        .await
        .expect("force shutdown downstream channel");

    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    let mut upstream_state = node_1.get_channel_actor_state(channels[0]);
    upstream_state.update_state(ChannelState::ShuttingDown(
        ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
    ));
    node_1
        .update_channel_actor_state(
            upstream_state,
            Some(ReloadParams {
                notify_changes: false,
            }),
        )
        .await;

    node_1.restart().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let restored_upstream_state = node_1.get_channel_actor_state(channels[0]);
    assert!(
        !restored_upstream_state.reestablishing,
        "upstream waiting-for-confirmation channel should restore as chain-watch only"
    );
    assert_eq!(
        restored_upstream_state.connectivity_state,
        ChannelConnectivityState::Offline,
        "upstream waiting-for-confirmation channel should stay offline after restart"
    );

    let upstream_control_before_close_confirmation = call!(node_1.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::Update(
                    UpdateCommand {
                        enabled: None,
                        tlc_expiry_delta: None,
                        tlc_minimum_value: None,
                        tlc_fee_proportional_millionths: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_1 alive");
    assert!(
        upstream_control_before_close_confirmation.is_ok(),
        "upstream waiting-for-confirmation channel actor should be restored after restart"
    );

    let mut restarted_downstream_state = node_1.get_channel_actor_state(channels[1]);
    restarted_downstream_state
        .tlc_state
        .get_mut(&TLCId::Offered(downstream_tlc.id()))
        .expect("closed downstream tlc exists")
        .expiry = now_timestamp_as_millis_u64().saturating_sub(1);
    node_1
        .update_channel_actor_state(
            restarted_downstream_state,
            Some(ReloadParams {
                notify_changes: false,
            }),
        )
        .await;

    node_1.store.insert_onchain_tlc_settlement(
        &fiber_types::NodeId::local(),
        &channels[1],
        TLCId::Offered(downstream_tlc.id()),
        OnChainTlcSettlement {
            payment_hash,
            hash_algorithm: HashAlgorithm::CkbHash,
            preimage: None,
            tx_hash: gen_rand_sha256_hash(),
            tlc_index: 0,
        },
    );

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channels[1]),
        ))
        .expect("network actor alive");

    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            node_0.wait_until_failed(payment_hash)
        )
        .await
        .is_err(),
        "payment should still stay pending until waiting-commitment-confirmation gains full restart recovery"
    );

    assert!(node_1
        .get_tlc(channels[0], TLCId::Received(previous_tlc_id))
        .and_then(|tlc| tlc.removed_reason)
        .is_some());
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_check_channels_onchain_fulfillment_fallback_marks_downstream_tlc() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build hold invoice");
    node_2.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;

    wait_until(|| {
        node_1
            .get_channel_actor_state(channels[1])
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .any(|tlc| tlc.payment_hash == payment_hash)
    })
    .await;

    let downstream_tlc_id = node_1
        .get_channel_actor_state(channels[1])
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| tlc.id())
        .expect("downstream tlc exists");

    node_1
        .send_shutdown(channels[1], true)
        .await
        .expect("force shutdown downstream channel");
    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    insert_onchain_preimage(&node_1.store, &channels[1], payment_hash, hold_preimage);
    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channels[1]),
        ))
        .expect("network actor alive");
    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::CheckChannels,
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_0
            .get_payment_session(payment_hash)
            .is_some_and(|session| session.status == PaymentStatus::Success)
    })
    .await;

    assert!(matches!(
        node_1
            .get_tlc(channels[1], TLCId::Offered(downstream_tlc_id))
            .and_then(|tlc| tlc.removed_reason),
        Some(RemoveTlcReason::RemoveTlcFulfill(..))
    ));
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_check_channels_fallback_does_not_mark_downstream_when_upstream_rejects_remove() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build hold invoice");
    node_2.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;

    wait_until(|| {
        node_1
            .get_channel_actor_state(channels[1])
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .any(|tlc| tlc.payment_hash == payment_hash)
    })
    .await;

    let downstream_tlc = node_1
        .get_channel_actor_state(channels[1])
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .cloned()
        .expect("downstream tlc exists");
    let (upstream_channel_id, upstream_tlc_id) = downstream_tlc
        .forwarding_tlc
        .expect("downstream tlc should track upstream tlc");
    assert_eq!(upstream_channel_id, channels[0]);

    node_1
        .send_shutdown(channels[1], true)
        .await
        .expect("force shutdown downstream channel");
    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channels[1]),
        ))
        .expect("network actor alive");
    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    let mut upstream_state = node_1.get_channel_actor_state(upstream_channel_id);
    upstream_state.reestablishing = true;
    node_1
        .update_channel_actor_state(
            upstream_state,
            Some(ReloadParams {
                notify_changes: false,
            }),
        )
        .await;

    insert_onchain_preimage(&node_1.store, &channels[1], payment_hash, hold_preimage);
    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::CheckChannels,
        ))
        .expect("network actor alive");
    node_1.node_info().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        node_1
            .get_tlc(channels[1], TLCId::Offered(downstream_tlc.id()))
            .and_then(|tlc| tlc.removed_reason)
            .is_none(),
        "downstream TLC must not be marked fulfilled until upstream RemoveTlc is actually accepted"
    );
    assert!(
        node_1
            .get_tlc(upstream_channel_id, TLCId::Received(upstream_tlc_id))
            .and_then(|tlc| tlc.removed_reason)
            .is_none(),
        "upstream actor is reestablishing and should reject the RemoveTlcCommand"
    );
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_check_channels_fallback_does_not_mutate_live_downstream_actor_state() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_2.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build hold invoice");
    node_2.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;

    wait_until(|| {
        node_1
            .get_channel_actor_state(channels[1])
            .tlc_state
            .offered_tlcs
            .tlcs
            .iter()
            .any(|tlc| tlc.payment_hash == payment_hash)
    })
    .await;

    let downstream_tlc_id = node_1
        .get_channel_actor_state(channels[1])
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| tlc.id())
        .expect("downstream tlc exists");

    node_1
        .send_shutdown(channels[1], true)
        .await
        .expect("force shutdown downstream channel");
    wait_until(|| {
        matches!(
            node_1.get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::UNCOOPERATIVE_LOCAL)
                    && flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    insert_onchain_preimage(&node_1.store, &channels[1], payment_hash, hold_preimage);
    node_1
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::CheckChannels,
        ))
        .expect("network actor alive");
    node_1.node_info().await;

    assert!(
        node_1
            .get_tlc(channels[1], TLCId::Offered(downstream_tlc_id))
            .and_then(|tlc| tlc.removed_reason)
            .is_none(),
        "CheckChannels fallback must not mutate a closed channel while its actor is still live"
    );
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_settlement_completed_reconciles_payer_onchain_preimage_before_actor_stops() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_1.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;
    wait_for_tlc_sync(&node_0, &node_1, channels[0], 1).await;

    node_0
        .send_shutdown(channels[0], true)
        .await
        .expect("force shutdown payer channel");
    wait_until(|| {
        matches!(
            node_0.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    node_0.node_info().await;
    insert_onchain_preimage(&node_0.store, &channels[0], payment_hash, hold_preimage);
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channels[0]),
        ))
        .expect("network actor alive");

    wait_until_timeout(10_000, || {
        matches!(
            node_0.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if !flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    assert_eq!(
        node_0.get_payment_status(payment_hash).await,
        PaymentStatus::Success,
        "settlement completion must reconcile the observed on-chain preimage before stopping the channel actor"
    );
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn test_payment_succeeds_when_onchain_preimage_arrives_before_settlement_completion() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let hold_preimage = gen_rand_sha256_hash();
    let hold_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(hold_preimage)
        .payee_pub_key(node_1.pubkey.into())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build hold invoice");
    node_1.insert_invoice(hold_invoice.clone(), None);

    let payment_hash = *hold_invoice.payment_hash();
    node_0
        .send_payment(SendPaymentCommand {
            amount: Some(1000),
            max_fee_rate: Some(1000),
            invoice: Some(hold_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment to hold invoice");
    node_0.wait_until_inflight(payment_hash).await;
    wait_for_tlc_sync(&node_0, &node_1, channels[0], 1).await;

    node_0
        .send_shutdown(channels[0], true)
        .await
        .expect("force shutdown payer channel");
    wait_until(|| {
        matches!(
            node_0.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags)
                if flags.contains(CloseFlags::WAITING_ONCHAIN_SETTLEMENT)
        )
    })
    .await;

    insert_onchain_preimage(&node_0.store, &channels[0], payment_hash, hold_preimage);
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            NetworkActorEvent::ChannelSettlementCompleted(channels[0]),
        ))
        .expect("network actor alive");

    wait_until_timeout(30_000, || {
        node_0
            .get_payment_session(payment_hash)
            .is_some_and(|session| session.status == PaymentStatus::Success)
    })
    .await;
    assert_eq!(
        node_0.get_payment_status(payment_hash).await,
        PaymentStatus::Success
    );
}

#[tokio::test]
async fn test_send_payment_shutdown_cooperative() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();
    for i in 0..10 {
        let res = nodes[0].send_payment_keysend(&nodes[3], 1000, false).await;
        if let Ok(send_payment_res) = res {
            if i > 5 {
                all_sent.insert(send_payment_res.payment_hash);
            }
        }

        if i == 5 {
            let _ = nodes[3].send_shutdown(channels[2], false).await;
        }
    }

    let mut failed_count = 0;
    let all_tx_count = all_sent.len();
    while !all_sent.is_empty() {
        for payment_hash in all_sent.clone().iter() {
            nodes[0].wait_until_final_status(*payment_hash).await;
            let res = nodes[0].get_payment_result(*payment_hash).await;
            eprintln!(
                "payment_hash: {:?} status: {:?} failed_count: {:?}",
                payment_hash, res.status, failed_count
            );
            if res.status == PaymentStatus::Failed || res.status == PaymentStatus::Success {
                failed_count += 1;
                all_sent.remove(payment_hash);
            }
        }
    }
    assert_eq!(failed_count, all_tx_count);

    loop {
        let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
        eprintln!(
            "node_3_channel_actor_state: {:?}",
            node_3_channel_actor_state.state
        );
        let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
        eprintln!(
            "node_2_channel_actor_state: {:?}",
            node_2_channel_actor_state.state
        );
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_3_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
    let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

#[tokio::test]
async fn test_send_payment_shutdown_cooperative_sender_sent() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let old_node0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let old_node3_balance = node_3.get_local_balance_from_channel(channels[2]);

    let mut all_sent = HashSet::new();
    let tlc_amount = 1000;
    for _i in 0..4 {
        let res = node_0
            .send_payment_keysend(&node_3, tlc_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            all_sent.insert(send_payment_res.payment_hash);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;
    for _i in 0..100 {
        let node_3_channel_actor_state = node_3.get_channel_actor_state(channels[2]);
        if node_3_channel_actor_state
            .tlc_state
            .all_tlcs()
            .next()
            .is_none()
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }

    loop {
        let res = node_2.send_shutdown(channels[2], false).await;
        if res.is_ok() {
            debug!("send shutdown successfully");
            break;
        }
    }

    let mut succ_count = 0;
    let all_tx_count = all_sent.len();
    let mut count = 0;
    while !all_sent.is_empty() && count < 100 {
        for payment_hash in all_sent.clone().iter() {
            node_0.wait_until_final_status(*payment_hash).await;
            let res = node_0.get_payment_result(*payment_hash).await;
            eprintln!("payment_hash: {:?} status: {:?}", payment_hash, res.status);

            if res.status == PaymentStatus::Success {
                succ_count += 1;
                all_sent.remove(payment_hash);
            }
        }
        count += 1;
    }
    debug!("all_count: {:?} succ_count: {:?}", all_tx_count, succ_count);

    for _i in 0..100 {
        let node_3_channel_actor_state = node_3.get_channel_actor_state(channels[2]);
        eprintln!(
            "node_3_channel_actor_state: {:?}",
            node_3_channel_actor_state.state
        );
        let node_2_channel_actor_state = node_2.get_channel_actor_state(channels[2]);
        eprintln!(
            "node_2_channel_actor_state: {:?}",
            node_2_channel_actor_state.state
        );
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    let node_3_channel_actor_state = node_3.get_channel_actor_state(channels[2]);
    assert_eq!(
        node_3_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
    let node_2_channel_actor_state = node_2.get_channel_actor_state(channels[2]);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    let new_node0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let new_node3_balance = node_3.get_local_balance_from_channel(channels[2]);
    debug!(
        "node0 send: {} - {} = {}",
        old_node0_balance,
        new_node0_balance,
        old_node0_balance - new_node0_balance
    );
    debug!(
        "node3 recv: {} + {} = {}",
        old_node3_balance,
        new_node3_balance - old_node3_balance,
        new_node3_balance
    );

    assert_eq!(
        old_node0_balance - new_node0_balance,
        (tlc_amount + 3) * succ_count
    );
    assert_eq!(
        new_node3_balance - old_node3_balance,
        tlc_amount * succ_count
    );
}

#[tokio::test]
async fn test_send_payment_shutdown_under_send_each_other() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let mut all_sent = HashSet::new();
    for _i in 0..5 {
        let rand_amount = 1 + (rand::random::<u64>() % 1000) as u128;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], rand_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            all_sent.insert(send_payment_res.payment_hash);
        }
        let rand_amount = 1 + (rand::random::<u64>() % 1000) as u128;
        let res = nodes[3]
            .send_payment_keysend(&nodes[0], rand_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            all_sent.insert(send_payment_res.payment_hash);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    for _i in 0..20 {
        let res = nodes[3].send_shutdown(channels[2], false).await;
        if res.is_ok() {
            debug!("send shutdown successfully");
            break;
        }
        debug!("shutdown res: {:?}", res);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    for i in 0..30 {
        assert!(nodes[2].get_triggered_unexpected_events().await.is_empty());
        assert!(nodes[3].get_triggered_unexpected_events().await.is_empty());

        let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
        eprintln!(
            "checking {}: node_2_channel_actor_state: {:?} tlc_pending:\n",
            i, node_2_channel_actor_state.state,
        );
        node_2_channel_actor_state.tlc_state.debug();

        let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
        eprintln!(
            "checking { }: node_3_channel_actor_state: {:?} tlc_pending:\n",
            i, node_3_channel_actor_state.state,
        );
        node_3_channel_actor_state.tlc_state.debug();
        if !node_2_channel_actor_state.any_tlc_pending()
            && !node_3_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    wait_until(|| {
        matches!(
            nodes[3].get_channel_actor_state(channels[2]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_3_channel_actor_state = nodes[3].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_3_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    wait_until(|| {
        matches!(
            nodes[2].get_channel_actor_state(channels[2]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[2]);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

async fn run_shutdown_with_payment_send(sender: usize, receiver: usize) {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;

    let mut node0_sent_payments = HashSet::new();
    for _i in 0..5 {
        let rand_amount = 1 + (rand::random::<u64>() % 1000) as u128;
        let res = nodes[sender]
            .send_payment_keysend(&nodes[receiver], rand_amount, false)
            .await;
        if let Ok(send_payment_res) = res {
            node0_sent_payments.insert(send_payment_res.payment_hash);
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    let _ = nodes[2].send_shutdown(channels[1], false).await;

    // there will be no pending tlcs
    for i in 0..10 {
        assert!(nodes[1].get_triggered_unexpected_events().await.is_empty());
        assert!(nodes[2].get_triggered_unexpected_events().await.is_empty());

        let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[1]);
        eprintln!(
            "checking {}: node_1_channel_actor_state: {:?} tlc_pending:\n",
            i, node_1_channel_actor_state.state,
        );
        node_1_channel_actor_state.tlc_state.debug();

        let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[1]);
        eprintln!(
            "checking { }: node_2_channel_actor_state: {:?} tlc_pending:\n",
            i, node_2_channel_actor_state.state,
        );
        node_2_channel_actor_state.tlc_state.debug();
        if !node_1_channel_actor_state.any_tlc_pending()
            && !node_2_channel_actor_state.any_tlc_pending()
        {
            break;
        }
    }

    wait_until(|| {
        matches!(
            nodes[1].get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[1]);
    error!("node_1 state: {:?}", node_1_channel_actor_state.state);
    assert_eq!(
        node_1_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    wait_until(|| {
        matches!(
            nodes[2].get_channel_actor_state(channels[1]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_2_channel_actor_state = nodes[2].get_channel_actor_state(channels[1]);
    error!("node_2 state: {:?}", node_2_channel_actor_state.state);
    assert_eq!(
        node_2_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

#[tokio::test]
async fn test_send_payment_shutdown_under_single_direction_send() {
    run_shutdown_with_payment_send(1, 2).await;
}

#[tokio::test]
async fn test_shutdown_with_pending_tlc() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;

    // create a new payment hash
    let preimage: [u8; 32] = gen_rand_sha256_hash().as_ref().try_into().unwrap();

    let hash_algorithm = HashAlgorithm::Sha256;
    let payment_hash: Hash256 = hash_algorithm.hash(preimage).into();
    let expiry = now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA;
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_hash(payment_hash)
        .hash_algorithm(hash_algorithm)
        .payee_pub_key(nodes[1].pubkey.into())
        .final_expiry_delta(0)
        .build()
        .expect("build pending invoice");
    nodes[1].insert_invoice(invoice, None);
    let hops_infos = vec![
        PaymentHopData {
            amount: 1000,
            expiry,
            next_hop: Some(nodes[1].pubkey),
            hash_algorithm,
            ..Default::default()
        },
        PaymentHopData {
            amount: 1000,
            expiry,
            hash_algorithm,
            ..Default::default()
        },
    ];
    let packet = PeeledPaymentOnionPacket::create(
        nodes[0].get_private_key().clone(),
        hops_infos,
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create pending onion packet");
    let add_tlc_result = call!(nodes[0].network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 1000,
                        hash_algorithm,
                        payment_hash,
                        expiry,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        is_trampoline_hop: false,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive");
    assert!(add_tlc_result.is_ok());
    let res = nodes[0].send_shutdown(channels[0], false).await;
    assert!(res.is_err());

    let res = nodes[1].send_shutdown(channels[0], false).await;
    assert!(res.is_ok());

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let node_0_channel_actor_state = nodes[0].get_channel_actor_state(channels[0]);
    assert!(node_0_channel_actor_state.any_tlc_pending());

    assert!(matches!(
        node_0_channel_actor_state.state,
        ChannelState::ShuttingDown(ShuttingDownFlags::AWAITING_PENDING_TLCS)
    ));
    let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[0]);
    assert!(node_1_channel_actor_state.any_tlc_pending());
    assert!(matches!(
        node_1_channel_actor_state.state,
        ChannelState::ShuttingDown(ShuttingDownFlags::AWAITING_PENDING_TLCS)
    ));

    let remove_tlc_result = call!(nodes[1].network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::RemoveTlc(
                    RemoveTlcCommand {
                        id: add_tlc_result.unwrap().tlc_id,
                        reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
                            payment_preimage: preimage.into(),
                        }),
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node_b alive");
    assert!(remove_tlc_result.is_ok());

    wait_until(|| {
        matches!(
            nodes[0].get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(..)
        )
    })
    .await;
    let node_0_channel_actor_state = nodes[0].get_channel_actor_state(channels[0]);
    assert_eq!(
        node_0_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );

    wait_until(|| {
        matches!(
            nodes[1].get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(..)
        )
    })
    .await;

    let node_1_channel_actor_state = nodes[1].get_channel_actor_state(channels[0]);
    assert_eq!(
        node_1_channel_actor_state.state,
        ChannelState::Closed(CloseFlags::COOPERATIVE)
    );
}

#[tokio::test]
async fn test_payment_onion_invoice_udt_type_script_mismatch_fails() {
    init_tracing();

    use ckb_types::prelude::*;

    let channel_udt_script = Script::new_builder().args([0u8; 53].pack()).build();
    let invoice_udt_script = Script::new_builder().args([1u8; 53].pack()).build();

    let (nodes, channels) = create_n_nodes_network_with_params(
        &[(
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                funding_udt_type_script: Some(channel_udt_script.clone()),
                ..Default::default()
            },
        )],
        2,
        None,
    )
    .await;

    let [mut node_a, node_b] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_a;
    let target_pubkey = node_b.pubkey;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .hash_algorithm(HashAlgorithm::CkbHash)
        .udt_type_script(invoice_udt_script)
        .payee_pub_key(target_pubkey.into())
        .final_expiry_delta(0)
        .build()
        .expect("build invoice");
    node_b.insert_invoice(invoice.clone(), Some(preimage));

    let payment_hash = *invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    // Build an onion packet for the receiver (last hop) carrying the payment hash.
    let hops_infos = vec![
        PaymentHopData {
            amount,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
    ];
    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos,
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result = call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                        is_trampoline_hop: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    // Wait until the sender observes the failure.
    let offered_id = TLCId::Offered(add_tlc_result.tlc_id);
    wait_until(|| {
        source_node
            .get_tlc(channels[0], offered_id)
            .is_some_and(|tlc| tlc.removed_reason.is_some())
    })
    .await;

    let tlc = source_node
        .get_tlc(channels[0], offered_id)
        .expect("offered tlc exists");
    let RemoveTlcReason::RemoveTlcFail(packet) = tlc.removed_reason.expect("tlc should be removed")
    else {
        panic!("expected RemoveTlcFail due to UDT mismatch");
    };

    // Decode the error using the session key used to build the onion.
    let session_key = source_node.get_private_key().0.secret_bytes();
    let err = packet
        .decode(&session_key, vec![target_pubkey])
        .expect("decode error packet");
    assert_eq!(
        err.error.error_code,
        TlcErrorCode::IncorrectOrUnknownPaymentDetails
    );
}

#[tokio::test]
async fn test_payment_onion_invoice_hash_algorithm_mismatch_fails() {
    init_tracing();

    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;

    let [mut node_a, node_b] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_a;
    let target_pubkey = node_b.pubkey;

    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice_hash_algorithm = HashAlgorithm::CkbHash;
    let tlc_hash_algorithm = HashAlgorithm::Sha256;
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_hash(invoice_hash_algorithm.hash(preimage).into())
        .hash_algorithm(invoice_hash_algorithm)
        .payee_pub_key(target_pubkey.into())
        .build()
        .expect("build hold invoice");
    node_b.insert_invoice(invoice.clone(), None);

    let payment_hash = *invoice.payment_hash();
    let hops_infos = vec![
        PaymentHopData {
            amount,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            funding_tx_hash: Hash256::default(),
            hash_algorithm: tlc_hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: tlc_hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
    ];
    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos,
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result = call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount,
                        hash_algorithm: tlc_hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                        is_trampoline_hop: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    let offered_id = TLCId::Offered(add_tlc_result.tlc_id);
    wait_until(|| {
        source_node
            .get_tlc(channels[0], offered_id)
            .is_some_and(|tlc| tlc.removed_reason.is_some())
    })
    .await;

    let tlc = source_node
        .get_tlc(channels[0], offered_id)
        .expect("offered tlc exists");
    let RemoveTlcReason::RemoveTlcFail(packet) = tlc.removed_reason.expect("tlc should be removed")
    else {
        panic!("expected RemoveTlcFail due to hash algorithm mismatch");
    };

    let session_key = source_node.get_private_key().0.secret_bytes();
    let err = packet
        .decode(&session_key, vec![target_pubkey])
        .expect("decode error packet");
    assert_eq!(
        err.error.error_code,
        TlcErrorCode::IncorrectOrUnknownPaymentDetails
    );
    assert_eq!(
        node_b.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Open)
    );
}

#[tokio::test]
async fn test_forward_payment_rejects_mismatched_hash_algorithm_between_wire_and_onion() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [mut node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");
    let source_node = &mut node_0;
    let first_channel_funding_tx: Hash256 =
        source_node.get_channel_funding_tx(&channels[0]).unwrap();
    let second_channel_funding_tx: Hash256 = node_1.get_channel_funding_tx(&channels[1]).unwrap();

    let forward_amount: u128 = 1000;
    let source_amount: u128 = 1001;
    let onion_hash_algorithm = HashAlgorithm::CkbHash;
    let wire_hash_algorithm = HashAlgorithm::Sha256;
    let session_key = source_node.get_private_key().0.secret_bytes();
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(forward_amount))
        .payment_preimage(preimage)
        .hash_algorithm(onion_hash_algorithm)
        .payee_pub_key(node_2.pubkey.into())
        .build()
        .expect("build invoice");
    node_2.insert_invoice(invoice.clone(), Some(preimage));
    let payment_hash = *invoice.payment_hash();

    let old_node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let old_node_1_left_balance = node_1.get_local_balance_from_channel(channels[0]);
    let old_node_1_right_balance = node_1.get_local_balance_from_channel(channels[1]);
    let old_node_2_balance = node_2.get_local_balance_from_channel(channels[1]);

    let hops_infos = vec![
        PaymentHopData {
            amount: source_amount,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA * 3,
            next_hop: Some(node_1.pubkey),
            funding_tx_hash: first_channel_funding_tx,
            hash_algorithm: onion_hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: forward_amount,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA * 2,
            next_hop: Some(node_2.pubkey),
            funding_tx_hash: second_channel_funding_tx,
            hash_algorithm: onion_hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
        PaymentHopData {
            amount: forward_amount,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: None,
            funding_tx_hash: Hash256::default(),
            hash_algorithm: onion_hash_algorithm,
            payment_preimage: None,
            custom_records: None,
        },
    ];
    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos,
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result = call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: source_amount,
                        hash_algorithm: wire_hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA * 3,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        previous_tlc: None,
                        attempt_id: None,
                        is_trampoline_hop: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    let offered_id = TLCId::Offered(add_tlc_result.tlc_id);
    wait_until(|| {
        source_node
            .get_tlc(channels[0], offered_id)
            .is_some_and(|tlc| tlc.removed_reason.is_some())
            || node_2.get_local_balance_from_channel(channels[1]) != old_node_2_balance
    })
    .await;

    let tlc = source_node
        .get_tlc(channels[0], offered_id)
        .expect("offered tlc exists");
    let RemoveTlcReason::RemoveTlcFail(packet) = tlc.removed_reason.expect("tlc should be removed")
    else {
        panic!("expected RemoveTlcFail due to mismatched hash algorithm");
    };

    let err = packet
        .decode(&session_key, vec![node_1.pubkey])
        .expect("decode error packet");
    assert_eq!(
        err.error.error_code,
        TlcErrorCode::IncorrectOrUnknownPaymentDetails
    );

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_left_balance = node_1.get_local_balance_from_channel(channels[0]);
    let node_1_right_balance = node_1.get_local_balance_from_channel(channels[1]);
    let node_2_balance = node_2.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_0_balance, old_node_0_balance);
    assert_eq!(node_1_left_balance, old_node_1_left_balance);
    assert_eq!(node_1_right_balance, old_node_1_right_balance);
    assert_eq!(node_2_balance, old_node_2_balance);
}

#[tokio::test]
async fn test_send_payment_middle_hop_restart_will_be_ok() {
    async fn inner_run_restart_test(restart_node_index: usize) {
        init_tracing();

        let funding_amount = MIN_RESERVED_CKB + 1000 * 100_000_000;
        let (mut nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (funding_amount, funding_amount)),
                ((1, 2), (funding_amount, funding_amount)),
                ((2, 3), (funding_amount, funding_amount)),
            ],
            4,
        )
        .await;

        let payment_amount = 10 * 100_000_000;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();

        let payment_hash = res.payment_hash;

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Success);

        nodes[restart_node_index].restart().await;

        // wait for the node to be ready after reestablish channel
        tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();
        let payment_hash = res.payment_hash;
        eprintln!("res: {:?}", payment_hash);

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Success);
    }
    for restart_index in 1..=3 {
        let _ = inner_run_restart_test(restart_index).await;
    }
}

#[tokio::test]
async fn test_send_payment_middle_hop_stop_send_payment_then_start() {
    async fn inner_run_restart_test(restart_node_index: usize) {
        init_tracing();

        let funding_amount = MIN_RESERVED_CKB + 1000 * 100_000_000;
        let (mut nodes, _channels) = create_n_nodes_network(
            &[
                ((0, 1), (funding_amount, funding_amount)),
                ((1, 2), (funding_amount, funding_amount)),
                ((2, 3), (funding_amount, funding_amount)),
            ],
            4,
        )
        .await;

        let payment_amount = 10 * 100_000_000;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();

        let payment_hash = res.payment_hash;

        nodes[0].wait_until_success(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Success);

        nodes[restart_node_index].stop().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();
        let payment_hash = res.payment_hash;
        eprintln!("res: {:?}", payment_hash);

        nodes[0].wait_until_failed(payment_hash).await;
        let status = nodes[0].get_payment_status(payment_hash).await;
        assert_eq!(status, PaymentStatus::Failed);

        tokio::time::sleep(tokio::time::Duration::from_millis(4 * 1000)).await;

        // now we start nodes[2], expect the payment will success
        nodes[restart_node_index].start().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(5000)).await;

        // after node reconnect, there will be new channel_update, and payment history will
        // process it to clear the old fail records, with time passed, we can send payment with larger amount
        let mut count = 0;
        loop {
            let res = nodes[0]
                .send_payment_keysend(&nodes[3], payment_amount, true)
                .await;

            if res.is_ok() {
                break;
            } else {
                count += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                eprintln!("retry to wait amount increasing: {:?}", count);
            }
        }
    }

    let _ = inner_run_restart_test(2).await;
    let _ = inner_run_restart_test(3).await;
}

#[tokio::test]
async fn test_send_payment_sync_up_new_channel_is_added() {
    init_tracing();

    // create a network with 4 nodes, but only connect with 2 channels
    // node0 -> node1 -> node2  node3
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;
    let [mut node_0, mut node_1, mut node_2, mut node_3] = nodes.try_into().expect("4 nodes");

    let payment_amount = 10 * 100_000_000;
    let res = node_0
        .send_payment_keysend(&node_3, payment_amount, true)
        .await;

    assert!(res
        .unwrap_err()
        .to_string()
        .contains("Failed to build route"));

    // now add channel for node_2 and node_3
    let (channel_id, funding_tx_hash) = {
        establish_channel_between_nodes(
            &mut node_2,
            &mut node_3,
            ChannelParameters::new(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT),
        )
        .await
    };
    let funding_tx = node_2
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");

    // all the other nodes submit_tx
    for node in [&mut node_0, &mut node_1].into_iter() {
        let res = node.submit_tx(funding_tx.clone()).await;
        assert!(matches!(res, TxStatus::Committed(..)));
        node.add_channel_tx(channel_id, funding_tx_hash);
        wait_for_network_graph_update(node, 3).await;
    }

    let res = node_0
        .send_payment_keysend(&node_3, payment_amount, false)
        .await;

    let payment_hash = res.unwrap().payment_hash;
    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
// This test is not stable and may fail randomly, so we ignore it for now.
// The root cause is `assert!(node_0.get_triggered_unexpected_events().await.is_empty())` may fail
#[ignore]
async fn test_send_payment_remove_tlc_with_preimage_will_retry() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [mut node_0, mut node_1, node_2] = nodes.try_into().expect("3 nodes");

    let mut payments = HashSet::new();

    for i in 0..10 {
        if i % 2 == 0 {
            let amount = rand::random::<u128>() % 1000 + 1;
            let res = node_0
                .send_payment_keysend(&node_2, amount, false)
                .await
                .unwrap();
            payments.insert(res.payment_hash);
            node_0.wait_until_inflight(res.payment_hash).await;
        } else {
            let amount = rand::random::<u128>() % 1000 + 1;
            let res = node_2
                .send_payment_keysend(&node_0, amount, false)
                .await
                .unwrap();
            node_2.wait_until_created(res.payment_hash).await;
        }
    }

    let node1_pubkey = node_1.pubkey;
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(
                node1_pubkey,
                PeerDisconnectReason::Requested,
                None,
            ),
        ))
        .expect("node_a alive");

    node_1
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(pubkey, _) => {
                assert_eq!(pubkey, &node_0.pubkey);
                true
            }
            _ => false,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // reconnect node_0 and node_1
    node_0.connect_to_nonblocking(&node_1).await;

    // The live channel actor keeps retryable RemoveTlc work and resumes it after reestablishment,
    // so all the payments should succeed after reconnect.
    let started = SystemTime::now();

    loop {
        for payment_hash in payments.clone().iter() {
            assert!(node_0.get_triggered_unexpected_events().await.is_empty());
            assert!(node_1.get_triggered_unexpected_events().await.is_empty());
            assert!(node_2.get_triggered_unexpected_events().await.is_empty());

            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentStatus::Success {
                payments.remove(payment_hash);
            }
        }
        if payments.is_empty() {
            break;
        }
        let elapsed = SystemTime::now()
            .duration_since(started)
            .expect("time passed")
            .as_secs();
        if elapsed > 50 {
            let node0_state = node_0.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node_0_state:", node_0.pubkey);
            node0_state.tlc_state.debug();

            let node1_state = node_1.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node1_left_actor_state:", node_1.pubkey);
            node1_state.tlc_state.debug();

            let node1_right_state = node_1.get_channel_actor_state(channels[1]);
            eprintln!("peer {:?} node1_right_actor_state:", node_1.pubkey);
            node1_right_state.tlc_state.debug();

            let node2_state = node_2.get_channel_actor_state(channels[1]);
            eprintln!("peer {:?} node_2_state:", node_2.pubkey);
            node2_state.tlc_state.debug();

            panic!("timeout");
        }
    }
}

#[tokio::test]
#[ignore]
// FIXME: there is a bug in reestablishing channel.
async fn test_send_payment_send_each_other_reestablishing() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [mut node_0, mut node_1] = nodes.try_into().expect("2 nodes");

    let mut payments = HashSet::new();

    for i in 0..20 {
        if i % 2 == 0 {
            let amount = rand::random::<u128>() % 1000 + 1;
            let res = node_0
                .send_payment_keysend(&node_1, amount, false)
                .await
                .unwrap();
            payments.insert(res.payment_hash);
        } else {
            let amount = rand::random::<u128>() % 1000 + 1;
            let _res = node_1
                .send_payment_keysend(&node_0, amount, false)
                .await
                .unwrap();
        }
    }

    let node1_pubkey = node_1.pubkey;
    node_0
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(
                node1_pubkey,
                PeerDisconnectReason::Requested,
                None,
            ),
        ))
        .expect("node_a alive");

    node_1
        .expect_event(|event| match event {
            NetworkServiceEvent::PeerDisConnected(pubkey, _) => {
                assert_eq!(pubkey, &node_0.pubkey);
                true
            }
            _ => false,
        })
        .await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // reconnect node_0 and node_1
    node_0.connect_to_nonblocking(&node_1).await;

    // The live channel actor keeps retryable RemoveTlc work and resumes it after reestablishment,
    // so all the payments should succeed after reconnect.
    let started = SystemTime::now();

    loop {
        for payment_hash in payments.clone().iter() {
            assert!(node_0.get_triggered_unexpected_events().await.is_empty());
            assert!(node_1.get_triggered_unexpected_events().await.is_empty());

            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentStatus::Success || status == PaymentStatus::Failed {
                payments.remove(payment_hash);
            }
        }
        if payments.is_empty() {
            break;
        }
        let elapsed = SystemTime::now()
            .duration_since(started)
            .expect("time passed")
            .as_secs();
        if elapsed > 50 {
            let node0_state = node_0.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node_0_state:", node_0.pubkey);
            node0_state.tlc_state.debug();

            let node1_state = node_1.get_channel_actor_state(channels[0]);
            eprintln!("peer {:?} node1_left_actor_state:", node_1.pubkey);
            node1_state.tlc_state.debug();

            let node1_right_state = node_1.get_channel_actor_state(channels[1]);
            eprintln!("peer {:?} node1_right_actor_state:", node_1.pubkey);
            node1_right_state.tlc_state.debug();

            panic!("timeout");
        }
    }
}

#[tokio::test]
async fn test_send_payment_invoice_cancel_multiple_ops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 0), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, _node_1, _node_2] = nodes.try_into().expect("4 nodes");

    let mut payments = HashSet::new();
    let mut invoices: Vec<CkbInvoice> = vec![];

    let target_pubkey = node_0.pubkey;
    let count = 10;
    for _i in 0..count {
        let preimage = gen_rand_sha256_hash();
        let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(100))
            .payment_preimage(preimage)
            .payee_pub_key(target_pubkey.into())
            .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_0.private_key.0))
            .expect("build invoice success");

        node_0.insert_invoice(ckb_invoice.clone(), Some(preimage));
        invoices.push(ckb_invoice);
    }

    for i in 0..count {
        let invoice = &invoices[i];

        node_0.cancel_invoice(invoice.payment_hash());
        let res = node_0
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                amount: invoice.amount,
                max_fee_rate: Some(1000),
                allow_self_payment: true,
                ..Default::default()
            })
            .await
            .unwrap();
        payments.insert(res.payment_hash);
        node_0.wait_until_created(res.payment_hash).await;
    }

    loop {
        for payment_hash in payments.clone().iter() {
            node_0.wait_until_final_status(*payment_hash).await;
            let status = node_0.get_payment_status(*payment_hash).await;
            eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
            if status == PaymentStatus::Failed {
                payments.remove(payment_hash);
            }
            assert_ne!(status, PaymentStatus::Success);
        }
        if payments.is_empty() {
            break;
        }
    }
}

#[tokio::test]
async fn test_send_payment_no_preimage_invoice_will_make_payment_failed() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [node_0, node_1] = nodes.try_into().expect("4 nodes");

    let mut payments = HashSet::new();
    let mut invoices: Vec<CkbInvoice> = vec![];

    let count = 2;
    let target_pubkey = node_1.pubkey;
    // Note: the preimages are not stored in db
    for _i in 0..count {
        let preimage = gen_rand_sha256_hash();
        let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(100))
            .payment_preimage(preimage)
            .payee_pub_key(target_pubkey.into())
            .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
            .expect("build invoice success");

        invoices.push(ckb_invoice);
    }

    for i in 0..count {
        let invoice = &invoices[i];

        let res = node_0
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.to_string()),
                amount: invoice.amount,
                allow_self_payment: true,
                ..Default::default()
            })
            .await
            .unwrap();
        payments.insert(res.payment_hash);
        node_0.wait_until_created(res.payment_hash).await;
    }

    for payment_hash in payments.iter() {
        node_0.wait_until_failed(*payment_hash).await;
    }
}

#[tokio::test]
async fn test_send_payment_with_mixed_channel_hops() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node0, mut node1, mut node2] = nodes.try_into().expect("3 nodes");

    // create a private UDT channel with node_1 and node_2
    let (_new_channel_id, funding_tx) = establish_channel_between_nodes(
        &mut node1,
        &mut node2,
        ChannelParameters {
            public: false,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            funding_udt_type_script: Some(Script::default()), // UDT type
            ..Default::default()
        },
    )
    .await;
    let private_udt_channel = OutPoint::new(funding_tx.into(), 0);

    // get a router from node0 -> node2
    let router = node0
        .build_router(BuildRouterCommand {
            amount: Some(1000),
            hops_info: vec![
                HopRequire {
                    pubkey: node1.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node2.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    let channel0_outpoint = node0.get_channel_outpoint(&channels[0]).unwrap();
    let channel1_outpoint = node1.get_channel_outpoint(&channels[1]).unwrap();
    assert_eq!(router.router_hops[0].channel_outpoint, channel0_outpoint);
    assert_eq!(router.router_hops[1].channel_outpoint, channel1_outpoint);
    let mut copied_router = router.clone();

    // normal payment will succeed
    let res = node0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    eprintln!("res: {:?}", res);
    node0.wait_until_success(res.unwrap().payment_hash).await;

    // now we manually replace the second channel with the UDT channel
    // the payment will failed with proper error code
    copied_router.router_hops[1].channel_outpoint = private_udt_channel;
    let res = node0
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: copied_router.router_hops,
            keysend: Some(true),
            ..Default::default()
        })
        .await;
    eprintln!("res: {:?}", res);

    let err = res.expect_err("mixed CKB/UDT explicit route should be rejected before sending TLC");
    assert!(
        err.contains("Failed to build route, PathFind error: no path found"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_ckb_with_udt_mixed_routes_fail() {
    init_tracing();

    use ckb_types::prelude::*;

    let udt1_script = Script::new_builder().args([1u8; 53].pack()).build();
    let udt2_script = Script::new_builder().args([2u8; 53].pack()).build();

    // A --(CKB)--> B --(CKB)--> C --(CKB)--> A
    // A --(UDT1)--> B --(UDT1)--> C --(UDT1)--> A
    // A --(UDT2)--> B --(UDT2)--> C --(UDT2)--> A
    let channels_params = vec![
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        (
            (1, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        (
            (2, 0),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                ..Default::default()
            },
        ),
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                funding_udt_type_script: Some(udt1_script.clone()),
                ..Default::default()
            },
        ),
        (
            (1, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                funding_udt_type_script: Some(udt1_script.clone()),
                ..Default::default()
            },
        ),
        (
            (2, 0),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                funding_udt_type_script: Some(udt1_script.clone()),
                ..Default::default()
            },
        ),
        (
            (0, 1),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                funding_udt_type_script: Some(udt2_script.clone()),
                ..Default::default()
            },
        ),
        (
            (1, 2),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                funding_udt_type_script: Some(udt2_script.clone()),
                ..Default::default()
            },
        ),
        (
            (2, 0),
            ChannelParameters {
                public: true,
                node_a_funding_amount: HUGE_CKB_AMOUNT,
                node_b_funding_amount: HUGE_CKB_AMOUNT,
                funding_udt_type_script: Some(udt2_script.clone()),
                ..Default::default()
            },
        ),
    ];

    let (nodes, channels) = create_n_nodes_network_with_params(&channels_params, 3, None).await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    let small_ckb_router = node_a
        .build_router(BuildRouterCommand {
            amount: Some(1),
            hops_info: vec![
                HopRequire {
                    pubkey: node_b.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_c.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_a.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();
    let res = node_a
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: small_ckb_router.router_hops.clone(),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    let error = res.unwrap_err();
    assert!(error.contains("max_fee_amount is too low for selected route"));

    let amount: u128 = 1000;

    let ckb_router = node_a
        .build_router(BuildRouterCommand {
            amount: Some(amount),
            hops_info: vec![
                HopRequire {
                    pubkey: node_b.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_c.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_a.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    let udt1_router = node_a
        .build_router(BuildRouterCommand {
            amount: Some(amount),
            hops_info: vec![
                HopRequire {
                    pubkey: node_b.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_c.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_a.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: Some(udt1_script.clone()),
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    let udt2_router = node_a
        .build_router(BuildRouterCommand {
            amount: Some(amount),
            hops_info: vec![
                HopRequire {
                    pubkey: node_b.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_c.pubkey,
                    channel_outpoint: None,
                },
                HopRequire {
                    pubkey: node_a.pubkey,
                    channel_outpoint: None,
                },
            ],
            udt_type_script: Some(udt2_script.clone()),
            final_tlc_expiry_delta: None,
        })
        .await
        .unwrap();

    let udt2_channels = vec![channels[6], channels[7], channels[8]];
    let before_udt2_balances = capture_balances(&[&node_a, &node_b, &node_c], &udt2_channels);

    for _ in 0..3 {
        let res = node_a
            .send_payment_with_router(SendPaymentWithRouterCommand {
                router: ckb_router.router_hops.clone(),
                keysend: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        node_a.wait_until_success(res.payment_hash).await;

        let res = node_a
            .send_payment_with_router(SendPaymentWithRouterCommand {
                router: udt1_router.router_hops.clone(),
                keysend: Some(true),
                udt_type_script: Some(udt1_script.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        node_a.wait_until_success(res.payment_hash).await;
    }

    let after_udt2_balances = capture_balances(&[&node_a, &node_b, &node_c], &udt2_channels);
    assert_eq!(
        before_udt2_balances, after_udt2_balances,
        "UDT2 balances should remain unchanged"
    );

    let mixed_router = vec![
        ckb_router.router_hops[0].clone(),
        udt1_router.router_hops[1].clone(),
        udt2_router.router_hops[2].clone(),
    ];
    let err = node_a
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: mixed_router,
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .expect_err("mixed CKB/UDT explicit route should be rejected before sending TLC");
    assert!(
        err.contains("Failed to build route, PathFind error: no path found"),
        "unexpected error: {err}"
    );

    let mixed_router = vec![
        udt1_router.router_hops[0].clone(),
        udt1_router.router_hops[1].clone(),
        udt2_router.router_hops[2].clone(),
    ];
    let err = node_a
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: mixed_router,
            keysend: Some(true),
            udt_type_script: Some(udt1_script.clone()),
            ..Default::default()
        })
        .await
        .expect_err("mixed UDT explicit route should be rejected before sending TLC");
    assert!(
        err.contains("Failed to build route, PathFind error: no path found"),
        "unexpected error: {err}"
    );

    let mixed_router = vec![
        ckb_router.router_hops[0].clone(),
        udt1_router.router_hops[1].clone(),
        ckb_router.router_hops[2].clone(),
    ];
    let err = node_a
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: mixed_router,
            keysend: Some(true),
            ..Default::default()
        })
        .await
        .expect_err("mixed CKB/UDT explicit route should be rejected before sending TLC");
    assert!(
        err.contains("Failed to build route, PathFind error: no path found"),
        "unexpected error: {err}"
    );

    let mixed_router = vec![
        udt1_router.router_hops[0].clone(),
        udt2_router.router_hops[1].clone(),
        udt1_router.router_hops[2].clone(),
    ];
    let err = node_a
        .send_payment_with_router(SendPaymentWithRouterCommand {
            router: mixed_router,
            keysend: Some(true),
            udt_type_script: Some(udt1_script.clone()),
            ..Default::default()
        })
        .await
        .expect_err("mixed UDT explicit route should be rejected before sending TLC");
    assert!(
        err.contains("Failed to build route, PathFind error: no path found"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_send_payment_with_first_channel_retry_will_be_ok() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, 1), (HUGE_CKB_AMOUNT + 500, HUGE_CKB_AMOUNT)),
            ((0, 1), (HUGE_CKB_AMOUNT + 1000, HUGE_CKB_AMOUNT)), // multiple node0 -> node1 channels
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node0, _node1, node2] = nodes.try_into().expect("3 nodes");

    // disable channels[2], which will be the first time choice of send_payment
    node0.disable_channel_stealthy(channels[2]).await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let payment = node0
        .send_payment_keysend(&node2, 1000, false)
        .await
        .unwrap();
    node0
        .expect_router_used_channel(&payment, channels[2])
        .await;
    eprintln!("payment: {:?}", payment);
    node0.wait_until_success(payment.payment_hash).await;
    let payment_session = node0.get_payment_session(payment.payment_hash).unwrap();
    for i in 0..=2 {
        let channel_outpoint = node0.get_channel_outpoint(&channels[i]);
        eprintln!("i channel_outpoint: {:?}", channel_outpoint);
    }
    eprintln!("payment_session router: {:?}", payment_session);

    // node0 will succeeded with another channel
    node0
        .expect_payment_used_channel(payment.payment_hash, channels[1])
        .await;
    assert_eq!(payment_session.retry_times(), 2);
}

#[tokio::test]
#[ignore]
async fn test_send_payment_with_reconnect_two_times() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) =
        create_n_nodes_network(&[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))], 2).await;
    let [mut node0, mut node1] = nodes.try_into().expect("2 nodes");

    for _i in 0..2 {
        let mut payments = HashSet::new();
        for _j in 0..5 {
            let res = node0
                .send_payment_keysend(&node1, 1000, false)
                .await
                .unwrap();
            let payment_hash = res.payment_hash;
            payments.insert(payment_hash);
        }

        // disconnect peer
        let node1_pubkey = node1.pubkey;
        node0
            .network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::DisconnectPeer(
                    node1_pubkey,
                    PeerDisconnectReason::Requested,
                    None,
                ),
            ))
            .expect("node_a alive");

        node1
            .expect_event(|event| match event {
                NetworkServiceEvent::PeerDisConnected(pubkey, _) => {
                    assert_eq!(pubkey, &node0.pubkey);
                    true
                }
                _ => false,
            })
            .await;

        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

        // reconnect peer
        node0.connect_to_nonblocking(&node1).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        // wait for the payment to be retried
        for _i in 0..20 {
            assert!(node0.get_triggered_unexpected_events().await.is_empty());
            assert!(node1.get_triggered_unexpected_events().await.is_empty());
            for payment_hash in payments.clone().iter() {
                node0.wait_until_final_status(*payment_hash).await;
                let status = node0.get_payment_status(*payment_hash).await;
                eprintln!("payment_hash: {:?} got status : {:?}", payment_hash, status);
                if status == PaymentStatus::Success || status == PaymentStatus::Failed {
                    payments.remove(payment_hash);
                } else if status == PaymentStatus::Created {
                    // wait for the payment to be retried
                    let payment_session = node0.get_payment_session(*payment_hash).unwrap();
                    eprintln!(
                        "payment_session attempts: {:?}",
                        payment_session.attempts_count()
                    );
                }
            }
            if payments.is_empty() {
                break;
            }
        }
        if !payments.is_empty() {
            panic!("some payments are not finished: {:?}", payments);
        }
    }
}

#[tokio::test]
async fn test_send_payment_pending_count_on_find_path() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let funding_amount = HUGE_CKB_AMOUNT;
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (funding_amount, funding_amount)),
            // we build multiple channels between node_1 and node_2
            ((1, 2), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            ((1, 2), (funding_amount, funding_amount)),
            // node_2 -> node_3
            ((2, 3), (funding_amount, funding_amount)),
        ],
        4,
    )
    .await;

    let mut payments = HashSet::new();
    let mut channel_stats_map = HashMap::new();
    for i in 0..20 {
        let payment_amount = 10;
        let res = nodes[0]
            .send_payment_keysend(&nodes[3], payment_amount, false)
            .await
            .unwrap();

        let payment_hash = res.payment_hash;
        let second_hop_channel = res.routers[0].nodes[1].channel_outpoint.clone();
        channel_stats_map
            .entry(second_hop_channel)
            .and_modify(|e| *e += 1)
            .or_insert(1);

        eprintln!("i: {:?} payment_hash: {:?}", i, payment_hash);
        payments.insert(payment_hash);
    }

    // assert that the path finding tried multiple middle channels
    let mut used_channel_count = 0;
    for channel in &channels[1..channels.len() - 1] {
        let funding_tx = nodes[0].get_channel_funding_tx(channel).unwrap();
        let channel_outpoint = OutPoint::new(funding_tx.into(), 0);

        let tried_count = channel_stats_map.get(&channel_outpoint).unwrap_or(&0);
        debug!(
            "check channel_outpoint: {:?}, count: {:?}",
            channel_outpoint, tried_count
        );
        if *tried_count > 0 {
            used_channel_count += 1;
        }
    }
    assert!(used_channel_count >= 3);
}

#[tokio::test]
async fn test_send_payment_check_router_always_the_right_one() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        6,
    )
    .await;

    let channel1_funding_tx = nodes[0].get_channel_funding_tx(&channels[0]).unwrap();
    let channel1_outpoint = OutPoint::new(channel1_funding_tx.into(), 0);
    let channel2_funding_tx = nodes[1].get_channel_funding_tx(&channels[1]).unwrap();
    let channel2_outpoint = OutPoint::new(channel2_funding_tx.into(), 0);

    let check_router = |router: &SessionRoute| {
        assert_eq!(router.nodes[0].channel_outpoint, channel1_outpoint);
        assert_eq!(router.nodes[1].channel_outpoint, channel2_outpoint);
    };

    for _i in 0..5 {
        let res = nodes[0]
            .send_payment_keysend(&nodes[2], 100, false)
            .await
            .unwrap();
        check_router(&res.routers[0]);
    }

    let res = nodes[0]
        .send_payment_keysend(&nodes[2], 100, false)
        .await
        .unwrap();
    check_router(&res.routers[0]);
}

#[tokio::test]
async fn test_send_payment_with_reverse_channel_of_capaicity_not_enough() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (160000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (170000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            // path finding algorighm will choose this channel firstly,
            // since it has more capacity than the above two channels,
            // but there capacity from 1->2 is not enough for the payment
            // so the first payment will retry two times,
            // and the following payments will only retry once
            ((2, 1), (180000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;

    let node0_actor_state = nodes[0].get_channel_actor_state(channels[0]);
    eprintln!(
        "node_0: {:?} {:?}",
        node0_actor_state.to_local_amount, node0_actor_state.to_remote_amount
    );

    let node1_actor_state = nodes[1].get_channel_actor_state(channels[0]);
    eprintln!(
        "node_1: {:?} {:?}",
        node1_actor_state.to_local_amount, node1_actor_state.to_remote_amount
    );

    let mut payments = HashSet::new();
    let mut statistic = HashMap::new();

    let count = 5;
    for _i in 0..count {
        let payment = nodes[0]
            .send_payment(SendPaymentCommand {
                target_pubkey: Some(nodes[2].pubkey),
                amount: Some(30000),
                keysend: Some(true),
                ..Default::default()
            })
            .await;
        let payment_hash = payment.unwrap().payment_hash;
        nodes[0].wait_until_success(payment_hash).await;
        let session = nodes[0].get_payment_session(payment_hash).unwrap();
        let retry_times = session.retry_times();
        debug!(
            "payment_hash: {:?} retry_times: {:?}",
            payment_hash, retry_times
        );
        statistic
            .entry(retry_times)
            .and_modify(|e| *e += 1)
            .or_insert(1);
        payments.insert(payment_hash);
    }

    // assert only one payment session will try 2 times
    assert_eq!(statistic[&2], 1);
    assert_eq!(statistic[&1], count - 1);
}

#[tokio::test]
#[ignore]
/// this test now can only run with cargo nextest,
/// since it invoiving a global TOKIO_TASK_TRACKER_WITH_CANCELLATION
async fn test_network_cancel_error_handling() {
    use ractor::registry;
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (13900000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (14000000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((2, 1), (14100000000 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;

    let all_actors = registry::registered();
    error!("all actors: {:?}", all_actors.len());

    for i in 0..6 {
        let channel_prefix = format!("Channel-{}", i);
        assert!(
            all_actors
                .iter()
                .any(|actor| { actor.starts_with(&channel_prefix) }),
            "Channel actor should be registered with prefix {}",
            channel_prefix
        );
    }

    for i in 0..3 {
        let network_name = format!("network actor at {}", nodes[i].base_dir.to_str());
        assert!(
            registry::where_is(network_name).is_some(),
            "Network actor should be registered"
        );
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    cancel_tasks_and_wait_for_completion().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    for i in 0..3 {
        let network_name = format!("network actor at {}", nodes[i].base_dir.to_str());
        assert!(
            registry::where_is(network_name).is_none(),
            "Network actor should be removed"
        );
    }
    assert!(registry::registered().is_empty());
}

#[tokio::test]
async fn test_send_payment_will_use_sent_amount_for_better_path_finding() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (105 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (105 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((1, 2), (105 + MIN_RESERVED_CKB, MIN_RESERVED_CKB)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        4,
    )
    .await;

    let [node0, _node1, _node2, node3] = nodes.try_into().expect("4 nodes");

    let payment0 = node0
        .send_payment_keysend(&node3, 100, false)
        .await
        .unwrap()
        .payment_hash;
    let payment0_retry_times = node0.get_payment_session(payment0).unwrap().retry_times();
    node0.wait_until_success(payment0).await;
    assert_eq!(payment0_retry_times, 1);

    let payment1 = node0
        .send_payment_keysend(&node3, 100, false)
        .await
        .unwrap()
        .payment_hash;

    node0.wait_until_success(payment1).await;
    let payment1_retry_times = node0.get_payment_session(payment1).unwrap().retry_times();

    // sent_amount only track the amount inflight.
    // so here we will retry the payment once
    assert_eq!(payment1_retry_times, 2);

    let payment2 = node0
        .send_payment_keysend(&node3, 100, false)
        .await
        .unwrap()
        .payment_hash;

    node0.wait_until_success(payment2).await;
    let payment2_retry_times = node0.get_payment_session(payment2).unwrap().retry_times();
    // sent_amount only track the amount inflight.
    assert_eq!(payment2_retry_times, 2);
}

#[tokio::test]
async fn test_send_payment_dry_run_will_not_create_payment_session() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 400000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 100000, MIN_RESERVED_CKB)),
        ],
        4,
    )
    .await;
    let [node_0, _node_1, node_2, node_3] = nodes.try_into().expect("4 nodes");

    let payment_hash = gen_rand_sha256_hash();
    let res = node_0
        .send_payment(SendPaymentCommand {
            payment_hash: Some(payment_hash),
            amount: Some(1000),
            dry_run: true,
            target_pubkey: node_3.pubkey.into(),
            ..Default::default()
        })
        .await;
    eprintln!("res: {:?}", res);
    let payment = node_0.get_payment_session(payment_hash);
    assert!(payment.is_none(), "Payment session should not be created");

    let payment_hash = gen_rand_sha256_hash();
    let res = node_0
        .send_payment(SendPaymentCommand {
            payment_hash: Some(payment_hash),
            amount: Some(1000),
            dry_run: true,
            target_pubkey: node_2.pubkey.into(),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok(), "Send payment query failed: {:?}", res);
    let payment = node_0.get_payment_session(payment_hash);
    assert!(payment.is_none(), "Payment session should not be created");
}

#[tokio::test]
async fn test_payment_with_payment_data_record() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let mut custom_records = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(payment_secret, 10000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64()
                + DEFAULT_FINAL_TLC_EXPIRY_DELTA
                + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64()
                + DEFAULT_FINAL_TLC_EXPIRY_DELTA
                + DEFAULT_TLC_EXPIRY_DELTA,
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
    ];

    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64()
                            + DEFAULT_FINAL_TLC_EXPIRY_DELTA
                            + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        is_trampoline_hop: false,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // wait tlc 1 is removed
    wait_until_timeout(30_000, || {
        source_node
            .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
            .is_none()
    })
    .await;

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test]
async fn test_payment_with_insufficient_total_amount() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let mut custom_records = PaymentCustomRecords::default();
    // set total amount to 20000000000, but pay only 10000000000
    let record = BasicMppPaymentData::new(payment_secret, 20000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
    ];

    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        is_trampoline_hop: false,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    // timeout hold tlc after 5 seconds
    let channel_id = channels[0];
    let tlc_id = add_tlc_result_1.tlc_id;
    node_1
        .network_actor
        .send_after(Duration::from_secs(5), move || {
            NetworkActorMessage::new_command(NetworkActorCommand::TimeoutHoldTlc(
                payment_hash,
                channel_id,
                tlc_id,
            ))
        });

    // because tlc is not fulfilled, it should be removed after 5 seconds instead of settling
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason
        .is_none()
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // tlc should be removed after 5 seconds
    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    // balance should not change
    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_delayed_final_hold_invoice_cancel_failure_is_decodable_by_payer() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    let target_pubkey = node_1.pubkey;

    let payment_hash = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
        .payment_hash(payment_hash)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(false)
        .payment_secret(payment_secret)
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), None);

    node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(ckb_invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send hold invoice payment");

    wait_until_timeout(10_000, || {
        node_1.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;

    let payment_session = node_0
        .get_payment_session(payment_hash)
        .expect("payment session exists");
    let attempt = payment_session
        .attempts()
        .next()
        .expect("payment has one attempt")
        .clone();
    let offered_tlc_id = node_0
        .get_channel_actor_state(channels[0])
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == payment_hash)
        .map(|tlc| TLCId::Offered(tlc.id()))
        .expect("offered hold tlc exists");

    node_1.cancel_invoice(&payment_hash);

    wait_until_timeout(10_000, || {
        node_0
            .get_tlc(channels[0], offered_tlc_id)
            .is_some_and(|tlc| tlc.removed_reason.is_some())
    })
    .await;

    let tlc = node_0
        .get_tlc(channels[0], offered_tlc_id)
        .expect("offered tlc exists");
    let Some(RemoveTlcReason::RemoveTlcFail(packet)) = tlc.removed_reason else {
        panic!("expected delayed RemoveTlcFail");
    };

    let decoded = packet
        .decode(&attempt.session_key, attempt.hops_public_keys())
        .expect("payer should decode delayed final-hop failure");
    assert!(matches!(
        decoded.error.error_code,
        TlcErrorCode::InvoiceCancelled | TlcErrorCode::HoldTlcTimeout
    ));
    assert_eq!(decoded.hop_index, 0);
}

#[tokio::test]
async fn test_payment_with_wrong_payment_secret() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let wrong_payment_secret = gen_rand_sha256_hash();
    let mut custom_records = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(wrong_payment_secret, 10000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
        PaymentHopData {
            amount: 10000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
    ];

    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 10000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        is_trampoline_hop: false,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // wait tlc 1 is removed
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some_and(|t| t.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_payment_with_insufficient_amount_with_payment_data() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let mut custom_records = PaymentCustomRecords::default();
    let record = BasicMppPaymentData::new(payment_secret, 9000000000);
    record.write(&mut custom_records);
    let hops_infos = vec![
        PaymentHopData {
            amount: 9000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
        PaymentHopData {
            amount: 9000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            hash_algorithm,
            custom_records: Some(custom_records.clone()),
            ..Default::default()
        },
    ];

    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 9000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        is_trampoline_hop: false,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // wait tlc 1 is removed
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some_and(|t| t.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_payment_with_insufficient_amount_without_payment_data() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [mut node_0, node_1] = nodes.try_into().expect("2 nodes");
    let source_node = &mut node_0;
    let target_pubkey = node_1.pubkey;

    let preimage = gen_rand_sha256_hash();
    let payment_secret = gen_rand_sha256_hash();
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(10000000000))
        .payment_preimage(preimage)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(false)
        .payment_secret(payment_secret)
        .build()
        .expect("build invoice success");

    node_1.insert_invoice(ckb_invoice.clone(), Some(preimage));

    let payment_hash = *ckb_invoice.payment_hash();
    let hash_algorithm = HashAlgorithm::CkbHash;

    let hops_infos = vec![
        PaymentHopData {
            amount: 9000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            next_hop: Some(target_pubkey),
            hash_algorithm,
            ..Default::default()
        },
        PaymentHopData {
            amount: 9000000000,
            expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
            hash_algorithm,
            ..Default::default()
        },
    ];

    let packet = PeeledPaymentOnionPacket::create(
        source_node.get_private_key().clone(),
        hops_infos.clone(),
        Some(payment_hash.as_ref().to_vec()),
        SECP256K1,
    )
    .expect("create peeled packet");

    let add_tlc_result_1 = ractor::call!(source_node.network_actor, |rpc_reply| {
        NetworkActorMessage::new_command(NetworkActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id: channels[0],
                command: ChannelCommand::AddTlc(
                    AddTlcCommand {
                        amount: 9000000000,
                        hash_algorithm,
                        payment_hash,
                        expiry: now_timestamp_as_millis_u64() + DEFAULT_TLC_EXPIRY_DELTA,
                        onion_packet: packet.next.clone(),
                        shared_secret: packet.shared_secret,
                        is_trampoline_hop: false,
                        previous_tlc: None,
                        attempt_id: None,
                    },
                    rpc_reply,
                ),
            },
        ))
    })
    .expect("node alive")
    .expect("tlc");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // wait tlc 1 is removed
    while source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .is_some_and(|t| t.removed_reason.is_none())
    {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let tlc_result = source_node
        .get_tlc(channels[0], TLCId::Offered(add_tlc_result_1.tlc_id))
        .unwrap()
        .removed_reason;
    assert!(matches!(
        tlc_result,
        Some(RemoveTlcReason::RemoveTlcFail(..))
    ));

    let node_0_balance = source_node.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_0_balance, 10000000000);
    assert_eq!(node_1_balance, 0);
}

#[tokio::test]
async fn test_send_two_node_send_each_other_multiple_time() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 20000000000, MIN_RESERVED_CKB))],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");
    for _i in 0..3 {
        let res = node_0
            .send_payment_keysend(&node_1, 20000000000, false)
            .await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_0.wait_until_success(payment_hash).await;

        let payment_session = node_0.get_payment_session(payment_hash).unwrap();
        dbg!(&payment_session.status, &payment_session.attempts_count());

        tokio::time::sleep(Duration::from_secs(1)).await;

        let res = node_1
            .send_payment_keysend(&node_0, 20000000000, false)
            .await;

        eprintln!("res: {:?}", res);
        assert!(res.is_ok());
        let payment_hash = res.unwrap().payment_hash;
        eprintln!("begin to wait for payment: {} success ...", payment_hash);
        node_1.wait_until_success(payment_hash).await;

        let payment_session = node_1.get_payment_session(payment_hash).unwrap();
        dbg!(&payment_session.status, &payment_session.attempts_count());
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let res = node_0
        .send_payment_keysend(&node_1, 20000000000, false)
        .await;

    eprintln!("res: {:?}", res);
    assert!(res.is_ok());
    let payment_hash = res.unwrap().payment_hash;
    eprintln!("begin to wait for payment: {} success ...", payment_hash);
    node_0.wait_until_success(payment_hash).await;

    let payment_session = node_0.get_payment_session(payment_hash).unwrap();
    dbg!(&payment_session.status, &payment_session.attempts_count());

    let node_0_balance = node_0.get_local_balance_from_channel(channels[0]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[0]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 20000000000);
}

#[tokio::test]
async fn test_network_with_hops_max_number_limit() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((3, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((5, 6), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((6, 7), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((7, 8), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((8, 9), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((9, 10), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((10, 11), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((11, 12), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((12, 13), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((13, 14), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((14, 15), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        16,
    )
    .await;

    let thirteen_hop_base_limit = DEFAULT_TLC_EXPIRY_DELTA * 12 + DEFAULT_FINAL_TLC_EXPIRY_DELTA;
    let thirteen_hop_limit = thirteen_hop_base_limit + DEFAULT_TLC_EXPIRY_DELTA;

    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[14].pubkey), // can not make a payment with 14 hops
            amount: Some(1000),
            keysend: Some(true),
            max_fee_rate: Some(1000),
            tlc_expiry_limit: Some(thirteen_hop_limit),
            ..Default::default()
        })
        .await;

    assert!(payment.is_err());

    eprintln!("now test begin to send payment ...");
    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[13].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            max_fee_rate: Some(1000),
            tlc_expiry_limit: Some(thirteen_hop_limit),
            ..Default::default()
        })
        .await
        .expect("send payment success");
    eprintln!("payment: {:?}", payment);
    nodes[0].wait_until_success(payment.payment_hash).await;

    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[13].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            max_fee_rate: Some(1000),
            tlc_expiry_limit: Some(15 * 24 * 60 * 60 * 1000), // 15 days
            ..Default::default()
        })
        .await;

    assert!(
        payment.is_err(),
        "we can not set a max tlc expiry limit larger than 14 days"
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_network_with_relay_remove_will_be_ok() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (mut nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((2, 3), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((3, 4), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((4, 5), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        6,
    )
    .await;

    eprintln!("now test begin to send payment ...");
    let payment = nodes[0]
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(nodes[5].pubkey),
            amount: Some(1000),
            keysend: Some(true),
            max_fee_rate: Some(1000),
            tlc_expiry_limit: Some(DEFAULT_TLC_EXPIRY_DELTA * 10 + DEFAULT_FINAL_TLC_EXPIRY_DELTA),
            ..Default::default()
        })
        .await
        .expect("send payment success");
    eprintln!("payment: {:?}", payment);

    loop {
        let channel_actor_state = nodes[1].get_channel_actor_state(channels[1]);
        if !channel_actor_state.tlc_state.offered_tlcs.tlcs.is_empty() {
            nodes[0].stop().await;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_micros(500)).await;
    }

    loop {
        let channel_actor_state = nodes[1].get_channel_actor_state(channels[0]);
        if !channel_actor_state.retryable_tlc_operations.is_empty() {
            eprintln!(
                "channel_actor_state: {:?}",
                channel_actor_state.retryable_tlc_operations
            );
            break;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            eprintln!("channel actor state: {:?}", channel_actor_state.state);
        }
    }

    nodes[0].start().await;
    nodes[0].wait_until_success(payment.payment_hash).await;
}

#[tokio::test]
async fn test_send_payment_with_invalid_amount() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();
    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 0), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let payment = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_1.pubkey),
            amount: Some(0),
            keysend: Some(true),
            allow_self_payment: true,
            dry_run: true,
            ..Default::default()
        })
        .await;

    debug!("payment: {:?}", payment);
    assert!(payment.is_err());
    let error = payment.unwrap_err();
    assert!(error.contains("amount must be greater than 0"));

    let router = node_0
        .build_router(BuildRouterCommand {
            amount: Some(0),
            hops_info: vec![HopRequire {
                pubkey: node_1.pubkey,
                channel_outpoint: None,
            }],
            udt_type_script: None,
            final_tlc_expiry_delta: None,
        })
        .await;

    eprintln!("result: {:?}", router);
    let error = router.unwrap_err();
    assert!(error.contains("amount must be greater than 0"));

    let payment = node_0.send_mpp_payment(&node_2, 0, Some(2)).await;

    debug!("payment: {:?}", payment);

    let error = payment.unwrap_err();
    assert!(error.contains("amount must be greater than 0"));
}

#[tokio::test]
async fn test_send_payment_direct_channel_error_from_node_stop() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((1, 2), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, mut node_1, node_2] = nodes.try_into().expect("3 nodes");

    node_1.stop().await;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    let payment = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(100),
            keysend: Some(true),
            ..Default::default()
        })
        .await;

    assert!(payment.unwrap_err().contains("Insufficient balance"));
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_payment_with_same_invoice() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 3),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
            (
                (1, 3),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
            (
                (2, 3),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
            (
                (3, 4),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
        ],
        5,
        Some(gen_rpc_config()),
    )
    .await;

    let invoice = nodes[4]
        .gen_invoice(NewInvoiceParams {
            amount: 100000,
            ..Default::default()
        })
        .await;

    let mut all_sents: HashMap<usize, Hash256> = Default::default();

    for i in 0..=2 {
        let res = nodes[i]
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.invoice_address.clone()),
                ..Default::default()
            })
            .await;
        let payment_hash = res.as_ref().expect("send payment ok").payment_hash;
        all_sents.insert(i, payment_hash);
    }

    let mut succeeded_count = 0;
    for (node, payment_hash) in all_sents {
        nodes[node].wait_until_final_status(payment_hash).await;
        let status = nodes[node]
            .get_payment_session(payment_hash)
            .expect("payment session exist")
            .status;
        if status == PaymentStatus::Success {
            succeeded_count += 1;
        }
    }
    assert_eq!(succeeded_count, 1);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_payment_two_with_same_invoice() {
    init_tracing();

    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
            (
                (1, 2),
                ChannelParameters {
                    public: true,
                    node_a_funding_amount: HUGE_CKB_AMOUNT,
                    node_b_funding_amount: HUGE_CKB_AMOUNT,
                    ..Default::default()
                },
            ),
        ],
        3,
        Some(gen_rpc_config()),
    )
    .await;

    let invoice = nodes[2]
        .gen_invoice(NewInvoiceParams {
            amount: 100000,
            ..Default::default()
        })
        .await;

    let mut all_sents: HashMap<usize, Hash256> = Default::default();

    for i in 0..=1 {
        let res = nodes[i]
            .send_payment(SendPaymentCommand {
                invoice: Some(invoice.invoice_address.clone()),
                ..Default::default()
            })
            .await;
        let payment_hash = res.as_ref().expect("send payment ok").payment_hash;
        all_sents.insert(i, payment_hash);
    }

    let mut succeeded_count = 0;
    for (node, payment_hash) in all_sents {
        nodes[node].wait_until_final_status(payment_hash).await;
        let status = nodes[node]
            .get_payment_session(payment_hash)
            .expect("payment session exist")
            .status;
        if status == PaymentStatus::Success {
            succeeded_count += 1;
        }
    }
    assert_eq!(succeeded_count, 1);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_tlc_removed_while_waiting_for_forwarding_result() {
    init_tracing();
    let (nodes, _channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters::new(MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB),
            ),
            (
                (1, 2),
                ChannelParameters::new(MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB),
            ),
        ],
        3,
        Some(gen_rpc_config()),
    )
    .await;

    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    // 1. Successful payment to ensure route and preimage on Node 1 (Router)
    let amount = 1000000;
    let preimage = gen_rand_sha256_hash();

    let invoice_params = NewInvoiceParams {
        amount,
        payment_preimage: Some(preimage.into()),
        description: Some("Description".to_string()),
        ..Default::default()
    };

    let invoice_result = node_2.gen_invoice(invoice_params).await;
    let invoice = invoice_result.invoice;
    let payment_hash: InternalHash256 = invoice.data.payment_hash.into();
    let invoice_address = invoice_result.invoice_address;

    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(amount),
            payment_hash: Some(payment_hash),
            invoice: Some(invoice_address.clone()),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());
    node_0.wait_until_success(payment_hash).await;

    // Node 1 should now have the preimage.
    // Intermediate nodes don't persist preimages by default in this implementation,
    // but the bug relies on the node having it (possibly from race or other source).
    // We manually insert it to simulate the condition.
    if node_1.store.get_preimage(&payment_hash).is_none() {
        node_1.store.insert_preimage(payment_hash, preimage);
    }
    assert!(node_1.store.get_preimage(&payment_hash).is_some());

    // 2. Send duplicate payment with SAME payment hash
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(amount),
            payment_hash: Some(payment_hash),
            invoice: Some(invoice_address.clone()),
            ..Default::default()
        })
        .await;

    if res.is_ok() {
        node_0.wait_until_failed(payment_hash).await;
    }

    // 3. Verify Node 1 is alive by making a fresh payment
    let invoice_params2 = NewInvoiceParams {
        amount,
        description: Some("Fresh Payment".to_string()),
        ..Default::default()
    };
    let invoice_result2 = node_2.gen_invoice(invoice_params2).await;
    let invoice2 = invoice_result2.invoice;
    let payment_hash2: InternalHash256 = invoice2.data.payment_hash.into();
    let invoice_address2 = invoice_result2.invoice_address;

    let res2 = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2.pubkey),
            amount: Some(amount),
            invoice: Some(invoice_address2),
            ..Default::default()
        })
        .await;
    assert!(res2.is_ok());
    node_0.wait_until_success(payment_hash2).await;
}

#[tokio::test]
async fn test_send_payment_max_fee_rate_limit() {
    let payment_data = SendPaymentData::new(SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: Some(1000),
        keysend: Some(true),
        ..Default::default()
    })
    .expect("payment data ok");

    assert_eq!(payment_data.max_fee_amount, Some(5));

    let payment_data = SendPaymentData::new(SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: Some(1000),
        keysend: Some(true),
        max_fee_rate: Some(10),
        max_fee_amount: Some(6),
        ..Default::default()
    })
    .expect("payment data ok");

    assert_eq!(payment_data.max_fee_amount, Some(6));

    let payment_data = SendPaymentData::new(SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: Some(1000),
        keysend: Some(true),
        max_fee_rate: Some(10),
        max_fee_amount: Some(20),
        ..Default::default()
    })
    .expect("payment data ok");

    assert_eq!(payment_data.max_fee_amount, Some(10));
}

#[test]
fn test_send_payment_uses_invoice_trampoline_route_hint() {
    let (payee_private_key, payee) = gen_rand_secp256k1_keypair_tuple();
    let trampoline = gen_rand_fiber_public_key();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1_000))
        .payment_preimage(gen_rand_sha256_hash())
        .payee_pub_key(payee)
        .trampoline_route_hint(trampoline.into())
        .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &payee_private_key))
        .expect("build invoice with trampoline route hint");

    let payment = SendPaymentData::new(SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        ..Default::default()
    })
    .expect("build trampoline payment from invoice hint");
    assert_eq!(payment.trampoline_hops, Some(vec![trampoline]));

    let explicit_trampoline = gen_rand_fiber_public_key();
    let payment = SendPaymentData::new(SendPaymentCommand {
        invoice: Some(invoice.to_string()),
        trampoline_hops: Some(vec![explicit_trampoline]),
        ..Default::default()
    })
    .expect("explicit trampoline hop overrides invoice hint");
    assert_eq!(payment.trampoline_hops, Some(vec![explicit_trampoline]));
}

fn malicious_invoice_that_used_to_panic_parser() -> String {
    let mut data = vec![u5::try_from_u8(0).expect("valid unsigned invoice marker")];
    data.extend(std::iter::repeat_n(
        u5::try_from_u8(31).expect("valid u5"),
        SIGNATURE_U5_SIZE,
    ));
    encode("fibb", data, Variant::Bech32m).expect("valid bech32m invoice wrapper")
}

#[test]
fn test_send_payment_malicious_invoice_does_not_crash_node_entrypoint() {
    let command = SendPaymentCommand {
        target_pubkey: Some(gen_rand_fiber_public_key()),
        amount: Some(1000),
        payment_hash: Some(gen_rand_sha256_hash()),
        invoice: Some(malicious_invoice_that_used_to_panic_parser()),
        ..Default::default()
    };

    // NetworkActor handles SendPayment by calling this builder first. Before
    // invoice parser errors were propagated, this malformed invoice payload
    // could panic there and take down the actor.
    let result = panic::catch_unwind(panic::AssertUnwindSafe(move || {
        command.build_send_payment_data()
    }));
    let err = result
        .expect("malicious invoice must be rejected without panicking")
        .expect_err("malicious invoice must not build a payment request");

    match err {
        crate::Error::InvalidParameter(message) => {
            assert!(
                message.contains("invoice is invalid"),
                "unexpected validation error: {message}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_send_payment_dry_run_with_too_large_hop_hint_expiry_delta() {
    init_tracing();
    let (nodes, _channels) = create_n_nodes_network(
        &[((0, 1), (MIN_RESERVED_CKB + 40000000000, MIN_RESERVED_CKB))],
        3,
    )
    .await;
    let [node1, mut node2, mut node3] = nodes.try_into().expect("3 nodes");

    let (_new_channel_id, funding_tx_hash) = establish_channel_between_nodes(
        &mut node2,
        &mut node3,
        ChannelParameters {
            public: false,
            node_a_funding_amount: MIN_RESERVED_CKB + 20000000000,
            node_b_funding_amount: MIN_RESERVED_CKB,
            ..Default::default()
        },
    )
    .await;
    let funding_tx = node2
        .get_transaction_view_from_hash(funding_tx_hash)
        .await
        .expect("get funding tx");
    let outpoint = funding_tx.output_pts_iter().next().unwrap();

    let res = node1
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node3.pubkey),
            amount: Some(10000000000),
            keysend: Some(true),
            dry_run: true,
            hop_hints: Some(vec![HopHint {
                pubkey: node2.pubkey,
                channel_outpoint: outpoint,
                fee_rate: DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS as u64,
                tlc_expiry_delta: u64::MAX,
            }]),
            ..Default::default()
        })
        .await;

    assert!(res.is_err(), "Expect send payment failed: {:?}", res);
    assert_eq!(node1.get_inflight_payment_count().await, 0);
}
