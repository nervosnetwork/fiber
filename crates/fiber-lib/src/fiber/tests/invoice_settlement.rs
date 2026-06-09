use crate::ckb::tests::test_utils::{MockChainActorMiddleware, MockChainActorState};
use crate::ckb::CkbChainMessage;
use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::payment::SendPaymentCommand;
use crate::fiber::{NetworkActorEvent, NetworkActorMessage};
use crate::gen_rand_sha256_hash;
use crate::invoice::{
    CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore, PreimageStore, SettleInvoiceError,
};
use crate::rpc::invoice::NewInvoiceParams;
use crate::tests::test_utils::{
    create_n_nodes_network, create_n_nodes_network_with_params, establish_channel_between_nodes,
    gen_rpc_config, init_tracing, wait_for_network_graph_update, wait_until_timeout,
    ChannelParameters, NetworkNode, NetworkNodeConfigBuilder, HUGE_CKB_AMOUNT, MIN_RESERVED_CKB,
};
use crate::watchtower::WatchtowerStore;
use crate::NetworkServiceEvent;
use ckb_sdk::core::TransactionBuilder;
use ckb_types::{core::tx_pool::TxStatus, packed::OutPoint};
use fiber_types::{
    ChannelState, CloseFlags, Hash256, HashAlgorithm, NodeId, Privkey, SettlementData,
    SettlementTlc, ShuttingDownFlags, TLCId,
};
use ractor::{ActorProcessingErr, ActorRef};
use secp256k1::SECP256K1;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
struct PendingClosingTxBlocker {
    blocked_funding_outpoint: Arc<RwLock<Option<OutPoint>>>,
}

#[async_trait::async_trait]
impl MockChainActorMiddleware for PendingClosingTxBlocker {
    async fn handle(
        &mut self,
        _inner_self: ActorRef<CkbChainMessage>,
        message: CkbChainMessage,
        _state: &mut MockChainActorState,
    ) -> Result<Option<CkbChainMessage>, ActorProcessingErr> {
        let CkbChainMessage::SendTx(tx, reply) = message else {
            return Ok(Some(message));
        };

        let should_block = self
            .blocked_funding_outpoint
            .read()
            .expect("closing tx blocker lock")
            .as_ref()
            .is_some_and(|blocked| tx.input_pts_iter().any(|input| input == *blocked));

        if should_block {
            let _ = reply.send(Err(ckb_sdk::RpcError::Other(anyhow::anyhow!(
                "blocked closing transaction for pending-confirmation race reproduction"
            ))));
            return Ok(None);
        }

        Ok(Some(CkbChainMessage::SendTx(tx, reply)))
    }

    fn clone_box(&self) -> Box<dyn MockChainActorMiddleware> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Copy, Debug)]
enum WatchtowerPreimageEvent {
    Created(Hash256),
    Removed,
}

async fn collect_preimage_events(
    node: &mut NetworkNode,
    payment_hash: Hash256,
) -> Vec<WatchtowerPreimageEvent> {
    let started_at = tokio::time::Instant::now();
    let mut last_progress = started_at;
    let mut events = Vec::new();

    loop {
        let mut made_progress = false;
        while let Ok(event) = node.event_emitter.try_recv() {
            made_progress = true;
            match event {
                NetworkServiceEvent::PreimageCreated(hash, preimage) if hash == payment_hash => {
                    events.push(WatchtowerPreimageEvent::Created(preimage));
                }
                NetworkServiceEvent::PreimageRemoved(hash) if hash == payment_hash => {
                    events.push(WatchtowerPreimageEvent::Removed);
                }
                _ => {}
            }
        }

        if made_progress {
            last_progress = tokio::time::Instant::now();
        }

        if started_at.elapsed() > Duration::from_secs(5)
            || (!events.is_empty() && last_progress.elapsed() > Duration::from_secs(2))
        {
            return events;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn replay_watchtower_preimage_events(
    node: &NetworkNode,
    payment_hash: Hash256,
    events: &[WatchtowerPreimageEvent],
) {
    for event in events {
        match event {
            WatchtowerPreimageEvent::Created(preimage) => {
                node.store
                    .insert_watch_preimage(NodeId::local(), payment_hash, *preimage);
            }
            WatchtowerPreimageEvent::Removed => {
                node.store
                    .remove_watch_preimage(NodeId::local(), payment_hash);
            }
        }
    }
}

fn insert_watch_channel_with_pending_tlc(
    node: &NetworkNode,
    channel_id: Hash256,
    payment_hash: Hash256,
) {
    let local_settlement_key = Privkey::from(&[1; 32]);
    let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
    let local_funding_pubkey = Privkey::from(&[3; 32]).pubkey();
    let remote_funding_pubkey = Privkey::from(&[4; 32]).pubkey();
    let settlement_data = SettlementData {
        local_amount: 100,
        remote_amount: 200,
        tlcs: vec![SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::default(),
            payment_amount: 42,
            payment_hash,
            expiry: u64::MAX,
            local_key: Privkey::from(&[5; 32]),
            remote_key: Privkey::from(&[6; 32]).pubkey(),
        }],
    };

    node.store.insert_watch_channel(
        NodeId::local(),
        channel_id,
        None,
        local_settlement_key,
        remote_settlement_key,
        local_funding_pubkey,
        remote_funding_pubkey,
        settlement_data,
    );
}

#[tokio::test]
async fn test_settle_invoice_status_checks() {
    init_tracing();
    let node = NetworkNode::new().await;

    // 1. Test InvoiceNotFound
    let random_hash = gen_rand_sha256_hash();
    let random_preimage = gen_rand_sha256_hash();
    let res = node.settle_invoice(&random_hash, random_preimage).await;
    assert_eq!(
        res.unwrap_err(),
        SettleInvoiceError::InvoiceNotFound.to_string()
    );

    // 2. Test InvoiceStillOpen
    let preimage = gen_rand_sha256_hash();
    let payment_hash = Hash256::from(ckb_hash::blake2b_256(preimage));
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .payment_hash(payment_hash)
        .amount(Some(1000))
        .fallback_address("ckt1qyq29z5c5ct9qvzdh5xs7a4d43uyvc253ptq5axtlf".to_string())
        .expiry_time(Duration::from_secs(3600))
        .build()
        .unwrap();

    node.store.insert_invoice(invoice.clone(), None).unwrap();

    let res = node.settle_invoice(&payment_hash, preimage).await;
    assert_eq!(
        res.unwrap_err(),
        SettleInvoiceError::InvoiceStillOpen.to_string()
    );

    // 3. Test InvoiceAlreadyExpired (Status is Open but time expired)
    let preimage_expired = gen_rand_sha256_hash();
    let payment_hash_expired = Hash256::from(ckb_hash::blake2b_256(preimage_expired));
    // Create an invoice that is already expired (created 20s ago, valid for 10s)
    let mut invoice_expired = InvoiceBuilder::new(Currency::Fibb)
        .payment_hash(payment_hash_expired)
        .amount(Some(1000))
        .fallback_address("ckt1qyq29z5c5ct9qvzdh5xs7a4d43uyvc253ptq5axtlf".to_string())
        .expiry_time(Duration::from_secs(10))
        .build()
        .unwrap();

    invoice_expired.data.timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        - 20000;

    node.store
        .insert_invoice(invoice_expired.clone(), None)
        .unwrap();

    // Ensure it is Open in store
    assert_eq!(
        node.store.get_invoice_status(&payment_hash_expired),
        Some(CkbInvoiceStatus::Open)
    );
    assert!(invoice_expired.is_expired());

    let res = node
        .settle_invoice(&payment_hash_expired, preimage_expired)
        .await;
    assert_eq!(
        res.unwrap_err(),
        SettleInvoiceError::InvoiceAlreadyExpired.to_string()
    );

    // 4. Test InvoiceAlreadyCancelled
    let preimage_cancelled = gen_rand_sha256_hash();
    let payment_hash_cancelled = Hash256::from(ckb_hash::blake2b_256(preimage_cancelled));
    let invoice_cancelled = InvoiceBuilder::new(Currency::Fibb)
        .payment_hash(payment_hash_cancelled)
        .amount(Some(1000))
        .fallback_address("ckt1qyq29z5c5ct9qvzdh5xs7a4d43uyvc253ptq5axtlf".to_string())
        .expiry_time(Duration::from_secs(3600))
        .build()
        .unwrap();

    node.store
        .insert_invoice(invoice_cancelled.clone(), None)
        .unwrap();
    node.store
        .update_invoice_status(&payment_hash_cancelled, CkbInvoiceStatus::Cancelled)
        .unwrap();

    let res = node
        .settle_invoice(&payment_hash_cancelled, preimage_cancelled)
        .await;
    assert_eq!(
        res.unwrap_err(),
        SettleInvoiceError::InvoiceAlreadyCancelled.to_string()
    );

    // 5. Test InvoiceAlreadyPaid
    let preimage_paid = gen_rand_sha256_hash();
    let payment_hash_paid = Hash256::from(ckb_hash::blake2b_256(preimage_paid));
    let invoice_paid = InvoiceBuilder::new(Currency::Fibb)
        .payment_hash(payment_hash_paid)
        .amount(Some(1000))
        .fallback_address("ckt1qyq29z5c5ct9qvzdh5xs7a4d43uyvc253ptq5axtlf".to_string())
        .expiry_time(Duration::from_secs(3600))
        .build()
        .unwrap();

    node.store
        .insert_invoice(invoice_paid.clone(), None)
        .unwrap();
    node.store
        .update_invoice_status(&payment_hash_paid, CkbInvoiceStatus::Paid)
        .unwrap();

    let res = node.settle_invoice(&payment_hash_paid, preimage_paid).await;
    assert_eq!(
        res.unwrap_err(),
        SettleInvoiceError::InvoiceAlreadyPaid.to_string()
    );

    // 6. Test Success (Received)
    let preimage_success = gen_rand_sha256_hash();
    let payment_hash_success = Hash256::from(ckb_hash::blake2b_256(preimage_success));
    let invoice_success = InvoiceBuilder::new(Currency::Fibb)
        .payment_hash(payment_hash_success)
        .amount(Some(1000))
        .fallback_address("ckt1qyq29z5c5ct9qvzdh5xs7a4d43uyvc253ptq5axtlf".to_string())
        .expiry_time(Duration::from_secs(3600))
        .build()
        .unwrap();

    node.store
        .insert_invoice(invoice_success.clone(), None)
        .unwrap();
    node.store
        .update_invoice_status(&payment_hash_success, CkbInvoiceStatus::Received)
        .unwrap();

    let res = node
        .settle_invoice(&payment_hash_success, preimage_success)
        .await;
    assert!(res.is_ok());

    // 7. Test Success (Received but Expired) - Should succeed because it is already Received
    let preimage_success_expired = gen_rand_sha256_hash();
    let payment_hash_success_expired =
        Hash256::from(ckb_hash::blake2b_256(preimage_success_expired));
    let mut invoice_success_expired = InvoiceBuilder::new(Currency::Fibb)
        .payment_hash(payment_hash_success_expired)
        .amount(Some(1000))
        .fallback_address("ckt1qyq29z5c5ct9qvzdh5xs7a4d43uyvc253ptq5axtlf".to_string())
        .expiry_time(Duration::from_secs(10))
        .build()
        .unwrap();

    invoice_success_expired.data.timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        - 20000;

    node.store
        .insert_invoice(invoice_success_expired.clone(), None)
        .unwrap();
    node.store
        .update_invoice_status(&payment_hash_success_expired, CkbInvoiceStatus::Received)
        .unwrap();

    assert!(invoice_success_expired.is_expired());
    let res = node
        .settle_invoice(&payment_hash_success_expired, preimage_success_expired)
        .await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn test_send_payment_with_hold_invoice_workflow() {
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

    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = HashAlgorithm::CkbHash
        .hash(payment_preimage.as_ref())
        .into();
    let invoice = node_1
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("test invoice".to_string()),
            payment_hash: Some(payment_hash.into()),
            ..Default::default()
        })
        .await;

    // node_0 -> node_1 will be ok for hold invoice
    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());
    println!("res: {:?}", res);

    // wait until invoice in received
    for _ in 0..30 {
        let status = node_1
            .store
            .get_invoice_status(&payment_hash)
            .expect("invoice status");
        if status == CkbInvoiceStatus::Received {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }

    node_1
        .settle_invoice(&payment_hash, payment_preimage)
        .await
        .expect("settle invoice");

    node_0.wait_until_success(payment_hash).await;
}

#[tokio::test]
async fn test_cancel_hold_invoice_fails_pending_tlcs() {
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

    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash: Hash256 = HashAlgorithm::CkbHash
        .hash(payment_preimage.as_ref())
        .into();
    let invoice = node_1
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("hold invoice to cancel".to_string()),
            payment_hash: Some(payment_hash.into()),
            ..Default::default()
        })
        .await;

    let res = node_0
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            ..Default::default()
        })
        .await;
    assert!(res.is_ok());

    for _ in 0..30 {
        let status = node_1
            .store
            .get_invoice_status(&payment_hash)
            .expect("invoice status");
        if status == CkbInvoiceStatus::Received {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }
    assert_eq!(
        node_1.store.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Received)
    );

    node_1.cancel_invoice(&payment_hash);

    node_0.wait_until_failed(payment_hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_mpp_to_hold_invoice() {
    init_tracing();

    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
            ((0, 1), (MIN_RESERVED_CKB + 10000000000, MIN_RESERVED_CKB)),
        ],
        2,
    )
    .await;
    let [node_0, node_1] = nodes.try_into().expect("2 nodes");

    let target_pubkey = node_1.get_public_key();
    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::default()
        .hash(payment_preimage.as_ref())
        .into();
    // Add a hold mpp invoice
    let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(20000000000))
        .payment_hash(payment_hash)
        .payee_pub_key(target_pubkey.into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_1.private_key.0))
        .expect("build invoice success");
    node_1.insert_invoice(ckb_invoice.clone(), None);

    let command = SendPaymentCommand {
        max_parts: Some(2),
        dry_run: false,
        invoice: Some(ckb_invoice.to_string()),
        ..Default::default()
    };

    let res = node_0.send_payment(command).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    node_1
        .settle_invoice(&payment_hash, payment_preimage)
        .await
        .expect("settle invoice");

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
    assert_eq!(node_1_balance, 10000000000);

    let node_0_balance = node_0.get_local_balance_from_channel(channels[1]);
    let node_1_balance = node_1.get_local_balance_from_channel(channels[1]);
    dbg!(node_0_balance, node_1_balance);
    assert_eq!(node_0_balance, 0);
    assert_eq!(node_1_balance, 10000000000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_force_close_keeps_preimage_for_onchain_split() {
    fn has_received_tlc(node: &NetworkNode, channel_id: Hash256, payment_hash: Hash256) -> bool {
        node.store
            .get_channel_actor_state(&channel_id)
            .is_some_and(|state| {
                state
                    .tlc_state
                    .all_tlcs()
                    .any(|tlc| tlc.is_received() && tlc.payment_hash == payment_hash)
            })
    }

    init_tracing();

    let amount = 20000000000;
    let upstream_capacity = 12000000000;
    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + upstream_capacity, MIN_RESERVED_CKB),
            ),
            (
                (0, 1),
                (MIN_RESERVED_CKB + upstream_capacity, MIN_RESERVED_CKB),
            ),
            ((1, 2), (MIN_RESERVED_CKB + amount * 2, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, node_1, node_2] = nodes.try_into().expect("3 nodes");

    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::default()
        .hash(payment_preimage.as_ref())
        .into();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_hash(payment_hash)
        .payee_pub_key(node_2.get_public_key().into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build invoice success");
    node_2.insert_invoice(invoice.clone(), None);

    let response = node_0
        .send_payment(SendPaymentCommand {
            max_parts: Some(2),
            dry_run: false,
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send mpp payment");
    assert_eq!(response.payment_hash, payment_hash);

    node_0.wait_until_inflight(payment_hash).await;
    wait_until_timeout(30_000, || {
        node_2.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    wait_until_timeout(30_000, || {
        has_received_tlc(&node_1, channels[0], payment_hash)
            && has_received_tlc(&node_1, channels[1], payment_hash)
    })
    .await;

    node_0
        .send_shutdown(channels[0], true)
        .await
        .expect("force shutdown one upstream channel");
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    let tx_hash = TransactionBuilder::default().build().hash();
    node_1
        .network_actor
        .send_message(NetworkActorMessage::Event(
            NetworkActorEvent::ClosingTransactionConfirmed(
                node_0.pubkey,
                channels[0],
                tx_hash,
                true,
                false,
            ),
        ))
        .expect("node_1 network actor alive");

    node_2
        .settle_invoice(&payment_hash, payment_preimage)
        .await
        .expect("settle invoice");

    wait_until_timeout(30_000, || {
        matches!(
            node_1.get_channel_actor_state(channels[0]).state,
            ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
        )
    })
    .await;

    wait_until_timeout(30_000, || {
        !has_received_tlc(&node_1, channels[1], payment_hash)
    })
    .await;

    assert!(
        has_received_tlc(&node_1, channels[0], payment_hash),
        "the force-closed split should remain pending for on-chain settlement"
    );
    assert!(
        node_1.store.get_preimage(&payment_hash).is_some(),
        "the forwarding node must keep the preimage for the on-chain split"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_payer_force_close_keeps_watchtower_preimage_for_onchain_split() {
    fn has_received_tlc(node: &NetworkNode, channel_id: Hash256, payment_hash: Hash256) -> bool {
        node.store
            .get_channel_actor_state(&channel_id)
            .is_some_and(|state| {
                state
                    .tlc_state
                    .all_tlcs()
                    .any(|tlc| tlc.is_received() && tlc.payment_hash == payment_hash)
            })
    }

    init_tracing();

    let amount = 20000000000;
    let upstream_capacity = 12000000000;
    let (nodes, channels) = create_n_nodes_network(
        &[
            (
                (0, 1),
                (MIN_RESERVED_CKB + upstream_capacity, MIN_RESERVED_CKB),
            ),
            (
                (0, 1),
                (MIN_RESERVED_CKB + upstream_capacity, MIN_RESERVED_CKB),
            ),
            ((1, 2), (MIN_RESERVED_CKB + amount * 2, MIN_RESERVED_CKB)),
        ],
        3,
    )
    .await;
    let [node_0, mut node_1, node_2] = nodes.try_into().expect("3 nodes");

    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::default()
        .hash(payment_preimage.as_ref())
        .into();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_hash(payment_hash)
        .payee_pub_key(node_2.get_public_key().into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build invoice success");
    node_2.insert_invoice(invoice.clone(), None);

    let response = node_0
        .send_payment(SendPaymentCommand {
            max_parts: Some(2),
            dry_run: false,
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send mpp payment");
    assert_eq!(response.payment_hash, payment_hash);

    node_0.wait_until_inflight(payment_hash).await;
    wait_until_timeout(30_000, || {
        node_2.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    wait_until_timeout(30_000, || {
        has_received_tlc(&node_1, channels[0], payment_hash)
            && has_received_tlc(&node_1, channels[1], payment_hash)
    })
    .await;

    node_0
        .send_shutdown(channels[0], true)
        .await
        .expect("payer force shutdowns one upstream channel");
    let tx_hash = TransactionBuilder::default().build().hash();
    node_1
        .network_actor
        .send_message(NetworkActorMessage::Event(
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
            ChannelState::Closed(flags) if flags.contains(CloseFlags::UNCOOPERATIVE_REMOTE)
        )
    })
    .await;

    node_2
        .settle_invoice(&payment_hash, payment_preimage)
        .await
        .expect("settle invoice after the payer force-close tx is confirmed");

    wait_until_timeout(30_000, || {
        !has_received_tlc(&node_1, channels[1], payment_hash)
    })
    .await;

    assert!(
        has_received_tlc(&node_1, channels[0], payment_hash),
        "the payer-force-closed split should remain pending for on-chain settlement"
    );
    assert!(
        node_1.store.get_preimage(&payment_hash).is_some(),
        "the forwarding node must keep the local preimage for the on-chain split"
    );

    let preimage_events = collect_preimage_events(&mut node_1, payment_hash).await;
    assert!(
        preimage_events
            .iter()
            .any(|event| matches!(event, WatchtowerPreimageEvent::Created(_))),
        "the forwarding node should reveal the preimage to watchtower, preimage events: {preimage_events:?}"
    );
    insert_watch_channel_with_pending_tlc(&node_1, channels[0], payment_hash);
    replay_watchtower_preimage_events(&node_1, payment_hash, &preimage_events);
    assert!(
        node_1
            .store
            .get_watch_preimage(&NodeId::local(), &payment_hash)
            .is_some(),
        "watchtower must keep the preimage after the payer force-closes one same-hash MPP split; preimage events: {preimage_events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mpp_force_close_pending_confirmation_removes_watchtower_preimage_repro() {
    fn has_received_tlc(node: &NetworkNode, channel_id: Hash256, payment_hash: Hash256) -> bool {
        node.store
            .get_channel_actor_state(&channel_id)
            .is_some_and(|state| {
                state
                    .tlc_state
                    .all_tlcs()
                    .any(|tlc| tlc.is_received() && tlc.payment_hash == payment_hash)
            })
    }

    async fn establish_channel_for_all_nodes(
        nodes: &mut [NetworkNode],
        node_a_index: usize,
        node_b_index: usize,
        params: ChannelParameters,
    ) -> Hash256 {
        assert!(node_a_index < node_b_index);
        let (channel_id, funding_tx_hash) = {
            let (left, right) = nodes.split_at_mut(node_b_index);
            establish_channel_between_nodes(&mut left[node_a_index], &mut right[0], params).await
        };
        let funding_tx = nodes[node_a_index]
            .get_transaction_view_from_hash(funding_tx_hash)
            .await
            .expect("get funding tx");

        for node in nodes.iter_mut() {
            let res = node.submit_tx(funding_tx.clone()).await;
            node.add_channel_tx(channel_id, funding_tx.hash().into());
            assert!(
                matches!(res, TxStatus::Committed(..)),
                "funding tx should be committed on every mock chain, got {res:?}"
            );
        }

        channel_id
    }

    init_tracing();

    let blocked_funding_outpoint = Arc::new(RwLock::new(None));
    let blocked_funding_outpoint_for_config = blocked_funding_outpoint.clone();
    let mut nodes = NetworkNode::new_n_interconnected_nodes_with_config(3, move |i| {
        let builder = NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("node-{}", i)))
            .base_dir_prefix(&format!("test-fnn-node-{}-", i));
        if i == 1 {
            builder
                .mock_chain_actor_middleware(Box::new(PendingClosingTxBlocker {
                    blocked_funding_outpoint: blocked_funding_outpoint_for_config.clone(),
                }))
                .build()
        } else {
            builder.build()
        }
    })
    .await;

    let amount = 20000000000;
    let upstream_capacity = 12000000000;
    let channels = [
        establish_channel_for_all_nodes(
            &mut nodes,
            0,
            1,
            ChannelParameters::new(MIN_RESERVED_CKB + upstream_capacity, MIN_RESERVED_CKB),
        )
        .await,
        establish_channel_for_all_nodes(
            &mut nodes,
            0,
            1,
            ChannelParameters::new(MIN_RESERVED_CKB + upstream_capacity, MIN_RESERVED_CKB),
        )
        .await,
        establish_channel_for_all_nodes(
            &mut nodes,
            1,
            2,
            ChannelParameters::new(MIN_RESERVED_CKB + amount * 2, MIN_RESERVED_CKB),
        )
        .await,
    ];
    wait_for_network_graph_update(&nodes[0], 3).await;
    wait_for_network_graph_update(&nodes[1], 3).await;
    wait_for_network_graph_update(&nodes[2], 3).await;

    let force_closed_outpoint = nodes[1]
        .get_channel_outpoint(&channels[0])
        .expect("force-closed channel funding outpoint");
    *blocked_funding_outpoint
        .write()
        .expect("set blocked funding outpoint") = Some(force_closed_outpoint);

    let [node_0, mut node_1, node_2] = nodes.try_into().expect("3 nodes");

    let payment_preimage = gen_rand_sha256_hash();
    let payment_hash = HashAlgorithm::default()
        .hash(payment_preimage.as_ref())
        .into();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_hash(payment_hash)
        .payee_pub_key(node_2.get_public_key().into())
        .allow_mpp(true)
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &node_2.private_key.0))
        .expect("build invoice success");
    node_2.insert_invoice(invoice.clone(), None);

    let response = node_0
        .send_payment(SendPaymentCommand {
            max_parts: Some(2),
            dry_run: false,
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send mpp payment");
    assert_eq!(response.payment_hash, payment_hash);

    node_0.wait_until_inflight(payment_hash).await;
    wait_until_timeout(30_000, || {
        node_2.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    wait_until_timeout(30_000, || {
        has_received_tlc(&node_1, channels[0], payment_hash)
            && has_received_tlc(&node_1, channels[1], payment_hash)
    })
    .await;

    node_1
        .send_shutdown(channels[0], true)
        .await
        .expect("force shutdown one upstream channel");
    wait_until_timeout(30_000, || {
        matches!(
            node_1.get_channel_actor_state(channels[0]).state,
            ChannelState::ShuttingDown(flags)
                if flags.contains(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
        )
    })
    .await;

    node_2
        .settle_invoice(&payment_hash, payment_preimage)
        .await
        .expect("settle invoice while the force-close tx is pending confirmation");

    wait_until_timeout(30_000, || {
        !has_received_tlc(&node_1, channels[1], payment_hash)
    })
    .await;

    assert!(
        has_received_tlc(&node_1, channels[0], payment_hash),
        "the split in a pending force-close channel should not be removed off-chain"
    );

    let preimage_events = collect_preimage_events(&mut node_1, payment_hash).await;
    assert!(
        preimage_events
            .iter()
            .any(|event| matches!(event, WatchtowerPreimageEvent::Created(_))),
        "the forwarding node should reveal the preimage to watchtower, preimage events: {preimage_events:?}"
    );
    insert_watch_channel_with_pending_tlc(&node_1, channels[0], payment_hash);
    replay_watchtower_preimage_events(&node_1, payment_hash, &preimage_events);
    assert!(
        node_1
            .store
            .get_watch_preimage(&NodeId::local(), &payment_hash)
            .is_some(),
        "watchtower must keep the preimage while another same-hash split is still waiting for on-chain settlement; preimage events: {preimage_events:?}"
    );
}
