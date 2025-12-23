use crate::fiber::hash_algorithm::HashAlgorithm;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::types::Hash256;
use crate::gen_rand_sha256_hash;
use crate::invoice::{
    CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceStore, SettleInvoiceError,
};
use crate::rpc::invoice::NewInvoiceParams;
use crate::tests::test_utils::{
    create_n_nodes_network, create_n_nodes_network_with_params, gen_rpc_config, init_tracing,
    ChannelParameters, NetworkNode, HUGE_CKB_AMOUNT, MIN_RESERVED_CKB,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    let payment_hash = HashAlgorithm::CkbHash
        .hash(payment_preimage.as_ref())
        .into();
    let invoice = node_1
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("test invoice".to_string()),
            payment_hash: Some(payment_hash),
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
        .build()
        .expect("build invoice success");
    node_1.insert_invoice(ckb_invoice.clone(), None);

    let command = SendPaymentCommand {
        max_parts: Some(2),
        dry_run: false,
        invoice: Some(ckb_invoice.to_string()),
        ..Default::default()
    };

    let res = node_0.send_payment(command).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
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
