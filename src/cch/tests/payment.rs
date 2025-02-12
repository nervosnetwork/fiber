use std::time::Duration;

use crate::{
    cch::tests::lnd::{LndBitcoinDConf, LndNode},
    fiber::{
        graph::PaymentSessionStatus,
        network::SendPaymentCommand,
        tests::test_utils::{
            create_n_nodes_with_index_and_amounts_with_established_channel,
            establish_channel_between_nodes, init_tracing, NetworkNode, NetworkNodeConfig,
            NetworkNodeConfigBuilder, HUGE_CKB_AMOUNT, MIN_RESERVED_CKB,
        },
    },
    gen_rand_sha256_hash,
    invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder},
    store::subscription::{InvoiceState, InvoiceUpdate, PaymentUpdate},
};

#[tokio::test]
async fn test_cross_chain_payment() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let [mut fiber_node, mut hub] = NetworkNode::new_n_interconnected_nodes_with_config(2, |n| {
        let mut builder = NetworkNodeConfigBuilder::new();
        if n == 1 {
            builder = builder
                .should_start_lnd(true)
                .cch_config(Default::default());
        }
        builder.build()
    })
    .await
    .try_into()
    .expect("2 nodes");

    let (fiber_channel, _funding_tx) = establish_channel_between_nodes(
        &mut fiber_node,
        &mut hub,
        true,
        HUGE_CKB_AMOUNT,
        MIN_RESERVED_CKB,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let mut lnd_node = LndNode::new(
        Default::default(),
        LndBitcoinDConf::Existing(hub.get_bitcoind()),
    )
    .await;

    hub.get_lnd_node_mut().make_some_money();
    lnd_node.make_some_money();
    let lightning_channel = lnd_node.open_channel_with(hub.get_lnd_node_mut()).await;

    let n_nodes = 3;
    let (nodes, channels) = create_n_nodes_with_index_and_amounts_with_established_channel(
        &[
            ((0, 1), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
            ((1, 2), (HUGE_CKB_AMOUNT, MIN_RESERVED_CKB)),
        ],
        n_nodes,
        true,
    )
    .await;

    let preimage = gen_rand_sha256_hash();

    let node_2_pubkey = nodes[n_nodes - 1].pubkey;
    let node_2_amount = 100;
    let node_2_ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(node_2_amount))
        .payment_preimage(preimage.clone())
        .payee_pub_key(node_2_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");

    let hash = *node_2_ckb_invoice.payment_hash();

    let node_1_pubkey = nodes[n_nodes - 2].pubkey;
    let node_1_amount = 110;
    let node_1_ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(node_1_amount))
        .payment_hash(hash)
        .payee_pub_key(node_1_pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");

    // let mut invoice_subscribers = Vec::with_capacity(n_nodes);
    // let mut payment_subscribers = Vec::with_capacity(n_nodes);
    // for node in &nodes {
    //     let invoice_subscriber = MockSubscriber::new(Some(node.network_actor.get_cell())).await;
    //     let payment_subscriber = MockSubscriber::new(Some(node.network_actor.get_cell())).await;

    //     node.get_store_update_subscription()
    //         .subscribe_invoice(hash, invoice_subscriber.get_subscriber())
    //         .await
    //         .expect("subscribe invoice success");

    //     node.get_store_update_subscription()
    //         .subscribe_payment(hash, payment_subscriber.get_subscriber())
    //         .await
    //         .expect("subscribe payment success");

    //     invoice_subscribers.push(invoice_subscriber);
    //     payment_subscribers.push(payment_subscriber);
    // }

    let [mut node_0, mut node_1, mut node_2] = nodes.try_into().expect("3 nodes");
    let node_2_old_amount = node_2.get_local_balance_from_channel(channels[1]);
    let node_1_old_amount = node_1.get_local_balance_from_channel(channels[0]);

    node_2.insert_invoice(node_2_ckb_invoice.clone(), Some(preimage));
    // Don't set the preimage for node 1 invoice now.
    // We will settle it later when we receive the payment preimage from node 2.
    node_1.insert_invoice(node_1_ckb_invoice.clone(), None);

    // Node 0 sends a payment to node 1.
    let res = node_0
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_1_pubkey.clone()),
            amount: Some(node_1_amount),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(node_1_ckb_invoice.to_string()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            hold_payment: true,
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    assert_eq!(hash, payment_hash);

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // For now, the payment is inflight because node 1 does not have the preimage yet.
    node_0
        .assert_payment_status(payment_hash, PaymentSessionStatus::Inflight, Some(1))
        .await;

    assert_eq!(
        node_1.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Received)
    );
    let node_1_new_amount = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_1_new_amount, node_1_old_amount);

    // let mut node_1_invoice_updates = invoice_subscribers[1].message_queue.take().await;
    // // We may push the same update multiple times, so we need to dedup
    // node_1_invoice_updates.dedup();
    // // Before settling the invoice, we should not have the Paid state
    // assert_eq!(
    //     node_1_invoice_updates,
    //     vec![
    //         InvoiceUpdate {
    //             hash,
    //             state: InvoiceState::Open,
    //         },
    //         InvoiceUpdate {
    //             hash,
    //             state: InvoiceState::Received {
    //                 amount: node_1_amount,
    //                 is_finished: true,
    //             },
    //         },
    //     ]
    // );

    // let mut node_0_payment_updates = payment_subscribers[0].message_queue.take().await;
    // // We may push the same update multiple times, so we need to dedup
    // node_0_payment_updates.dedup();
    // // Before settling the invoice, we should not have the Success state
    // assert_eq!(
    //     node_0_payment_updates,
    //     vec![
    //         PaymentUpdate {
    //             hash,
    //             state: PaymentState::Created,
    //         },
    //         PaymentUpdate {
    //             hash,
    //             state: PaymentState::Inflight,
    //         },
    //     ]
    // );

    // Now that node 1 has received the payment from node 0, he should pay node 2 now.
    let res = node_1
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(node_2_pubkey.clone()),
            amount: Some(node_2_amount),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(node_2_ckb_invoice.to_string()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            hold_payment: true,
            udt_type_script: None,
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    assert_eq!(hash, payment_hash);

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    node_1
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

    assert_eq!(
        node_2.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Paid)
    );
    let node_2_new_amount = node_2.get_local_balance_from_channel(channels[1]);
    assert_eq!(node_2_new_amount, node_2_old_amount + node_2_amount);

    // // By now node 2 should have received the invoice updates.
    // let mut node_2_invoice_updates = invoice_subscribers[2].message_queue.take().await;
    // // We may push the same update multiple times, so we need to dedup
    // node_2_invoice_updates.dedup();
    // // Before settling the invoice, we should not have the Paid state
    // assert_eq!(
    //     node_2_invoice_updates,
    //     vec![
    //         InvoiceUpdate {
    //             hash,
    //             state: InvoiceState::Open,
    //         },
    //         InvoiceUpdate {
    //             hash,
    //             state: InvoiceState::Received {
    //                 amount: node_2_amount,
    //                 is_finished: true,
    //             },
    //         },
    //         InvoiceUpdate {
    //             hash,
    //             state: InvoiceState::Paid,
    //         }
    //     ]
    // );

    // // Node 1 should have received the payment updates.
    // let mut node_1_payment_updates = payment_subscribers[1].message_queue.take().await;
    // // We may push the same update multiple times, so we need to dedup
    // node_1_payment_updates.dedup();
    // // Before settling the invoice, we should not have the Success state
    // assert_eq!(
    //     node_1_payment_updates,
    //     vec![
    //         PaymentUpdate {
    //             hash,
    //             state: PaymentState::Created,
    //         },
    //         PaymentUpdate {
    //             hash,
    //             state: PaymentState::Inflight,
    //         },
    //         PaymentUpdate {
    //             hash,
    //             state: PaymentState::Success { preimage },
    //         },
    //     ]
    // );

    // Node 1 can now settle the invoice.
    node_1
        .settle_invoice(&hash, &preimage)
        .expect("settle invoice success");

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    assert_eq!(
        node_1.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Paid)
    );
    let node_1_new_amount = node_1.get_local_balance_from_channel(channels[0]);
    assert_eq!(node_1_new_amount, node_1_old_amount + node_1_amount);

    node_0
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

    // let mut node_1_invoice_updates = invoice_subscribers[1].message_queue.take().await;
    // // We may push the same update multiple times, so we need to dedup
    // node_1_invoice_updates.dedup();
    // // Invoice is now paid.
    // assert_eq!(
    //     node_1_invoice_updates,
    //     vec![InvoiceUpdate {
    //         hash,
    //         state: InvoiceState::Paid,
    //     }]
    // );

    // let mut node_0_payment_updates = payment_subscribers[0].message_queue.take().await;
    // // We may push the same update multiple times, so we need to dedup
    // node_0_payment_updates.dedup();
    // // Payment is now successful.
    // assert_eq!(
    //     node_0_payment_updates,
    //     vec![PaymentUpdate {
    //         hash,
    //         state: PaymentState::Success { preimage },
    //     }]
    // );
}
