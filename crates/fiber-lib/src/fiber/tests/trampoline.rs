#![cfg(not(target_arch = "wasm32"))]

use crate::fiber::features::FeatureVector;
use crate::fiber::network::SendPaymentCommand;
use crate::invoice::{Currency, InvoiceBuilder};
use crate::tests::test_utils::{create_n_nodes_network_with_visibility, init_tracing};
use crate::{gen_rand_sha256_hash, HUGE_CKB_AMOUNT, MIN_RESERVED_CKB};

#[tokio::test]
async fn test_trampoline_routing_private_last_hop_payment_success() {
    init_tracing();

    // A --(public)--> C --(private)--> D
    // A cannot find a route to D from gossip graph; C can forward to D using its direct channel.
    let (nodes, _channels) = create_n_nodes_network_with_visibility(
        &[
            ((0, 1), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), true),
            ((1, 2), (MIN_RESERVED_CKB + 100000, HUGE_CKB_AMOUNT), false),
        ],
        3,
    )
    .await;

    let [node_a, node_c, node_d] = nodes.try_into().expect("3 nodes");

    // Enable trampoline capability on C.
    let mut features = FeatureVector::default();
    features.set_trampoline_routing_optional();
    node_c.update_node_features(features).await;

    // Wait until A learns C supports trampoline routing.
    let trampoline_pubkey = node_c.get_public_key();
    for _ in 0..50 {
        let ok = node_a
            .get_network_nodes()
            .await
            .into_iter()
            .find(|n| n.node_id == trampoline_pubkey)
            .is_some_and(|n| n.features.supports_trampoline_routing());
        if ok {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    // Create an invoice on D that explicitly NOT allows trampoline routing.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_d.get_public_key().into())
        .allow_trampoline_routing(false)
        .build()
        .expect("build invoice");
    node_d.insert_invoice(invoice.clone(), Some(preimage));

    let res = node_a
        .assert_send_payment_failure(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            ..Default::default()
        })
        .await;
    eprintln!("payment failure reason: {}", res);
    assert!(res.contains("Failed to build route"));

    // Create an invoice on D that explicitly allows trampoline routing.
    let amount: u128 = 1000;
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_preimage(preimage)
        .payee_pub_key(node_d.get_public_key().into())
        .allow_trampoline_routing(true)
        .build()
        .expect("build invoice");
    node_d.insert_invoice(invoice.clone(), Some(preimage));

    let payment_hash = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(5_000),
            ..Default::default()
        })
        .await;
    assert!(payment_hash.is_ok());

    // TODO: assert payment will success
    // node_a
    //     .assert_send_payment_success(SendPaymentCommand {
    //         invoice: Some(invoice.to_string()),
    //         max_fee_amount: Some(5_000),
    //         ..Default::default()
    //     })
    //     .await;
}
