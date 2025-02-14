use std::{str::FromStr, time::Duration};

use bitcoin::hashes::Hash;
use ckb_types::packed::Script;
use lightning_invoice::Bolt11Invoice;
use ractor::call_t;

use crate::{
    cch::{
        tests::lnd::{LndBitcoinDConf, LndNode},
        CchMessage, ReceiveBTC, ReceiveBTCOrder, SendBTC, SendBTCOrder,
    },
    ckb::contracts::{get_script_by_contract, Contract},
    fiber::{
        graph::PaymentSessionStatus,
        hash_algorithm::HashAlgorithm,
        network::SendPaymentCommand,
        serde_utils::serialize_entity_to_hex_string,
        tests::test_utils::{
            establish_udt_channel_between_nodes, init_tracing, NetworkNode,
            NetworkNodeConfigBuilder, HUGE_CKB_AMOUNT, MIN_RESERVED_CKB,
        },
        types::Hash256,
    },
    gen_rand_sha256_hash,
    invoice::{CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder},
    CchConfig,
};

pub const CALL_ACTOR_TIMEOUT_MS: u64 = 3 * 1000;

fn get_simple_udt_script() -> Script {
    let args =
        hex::decode("32e555f3ff8e135cece1351a6a2971518392c1e30375c1e006ad0ce8eac07947").unwrap();
    get_script_by_contract(Contract::SimpleUDT, &args)
}

fn get_always_success_script() -> Script {
    get_script_by_contract(Contract::AlwaysSuccess, &vec![])
}

async fn do_test_cross_chain_payment_hub_send_btc(udt_script: Script) {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let [mut fiber_node, mut hub] = NetworkNode::new_n_interconnected_nodes_with_config(2, |n| {
        let mut builder = NetworkNodeConfigBuilder::new();
        if n == 1 {
            let mut cch_config = CchConfig::default();
            cch_config.wrapped_btc_type_script = serialize_entity_to_hex_string(&udt_script);
            builder = builder.should_start_lnd(true).cch_config(cch_config);
        }
        builder.build()
    })
    .await
    .try_into()
    .expect("2 nodes");

    let (fiber_channel, _funding_tx) = establish_udt_channel_between_nodes(
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
        udt_script.clone(),
    )
    .await;

    let mut lnd_node = LndNode::new(
        Default::default(),
        LndBitcoinDConf::Existing(hub.get_bitcoind()),
    )
    .await;

    let hub_old_amount = hub.get_local_balance_from_channel(fiber_channel);

    hub.get_lnd_node_mut().make_some_money();
    hub.get_lnd_node_mut()
        .open_channel_with(&mut lnd_node)
        .await;

    // TODO: without the sleep below, we may fail to send the payment below. The root cause is unknown to me.
    // We will see two payments in the logs, which tells us the payment is failed because of FailureReasonInsufficientBalance.
    // Payment { payment_hash: "650feb233a22fb60a7e2458d03c0a5afa7043207a39c8c1c8a05d183bb5b7455", value: 100, creation_date: 1739422958, fee: 0, payment_preimage: "0000000000000000000000000000000000000000000000000000000000000000", value_sat: 100, value_msat: 100000, payment_request: "lnbcrt1u1pn66l8wpp5v587kge6ytakpflzgkxs8s9947nsgvs85wwgc8y2qhgc8w6mw32sdqqcqzzsxqyz5vqsp53k09akasd35ldkhl4twt9mmxd63cgu2l9j7jept03g6djv5nkazq9qxpqysgqq2dpmpqrsglycahtz4vsuy29a5kjhjt3w4ea664h0tfs0g5cwyn9dm54c2qe4tzxzatcw7dnfhuht5kewdqmn0zrg4cj7h74xejre2sqnhmf42", status: InFlight, fee_sat: 0, fee_msat: 0, creation_time_ns: 1739422958687770515, htlcs: [], payment_index: 1, failure_reason: FailureReasonNone })
    // Payment { payment_hash: "650feb233a22fb60a7e2458d03c0a5afa7043207a39c8c1c8a05d183bb5b7455", value: 100, creation_date: 1739422958, fee: 0, payment_preimage: "0000000000000000000000000000000000000000000000000000000000000000", value_sat: 100, value_msat: 100000, payment_request: "lnbcrt1u1pn66l8wpp5v587kge6ytakpflzgkxs8s9947nsgvs85wwgc8y2qhgc8w6mw32sdqqcqzzsxqyz5vqsp53k09akasd35ldkhl4twt9mmxd63cgu2l9j7jept03g6djv5nkazq9qxpqysgqq2dpmpqrsglycahtz4vsuy29a5kjhjt3w4ea664h0tfs0g5cwyn9dm54c2qe4tzxzatcw7dnfhuht5kewdqmn0zrg4cj7h74xejre2sqnhmf42", status: Failed, fee_sat: 0, fee_msat: 0, creation_time_ns: 1739422958687770515, htlcs: [], payment_index: 1, failure_reason: FailureReasonInsufficientBalance }
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let lnd_amount_sats = 100;
    let lnd_amount_msats = lnd_amount_sats * 1000;
    let add_invoice_result = lnd_node.add_invoice(lnd_amount_msats).await;
    let lnd_old_amount = lnd_node.get_balance_sats().await;

    let hash = Hash256::try_from(add_invoice_result.r_hash.as_slice()).expect("valid hash");

    let send_btc_result: SendBTCOrder = call_t!(
        hub.get_cch_actor(),
        CchMessage::SendBTC,
        CALL_ACTOR_TIMEOUT_MS,
        SendBTC {
            btc_pay_req: add_invoice_result.payment_request,
            currency: Currency::Fibd,
        }
    )
    .expect("send btc actor call")
    .expect("send btc result");

    let fiber_invoice = CkbInvoice::from_str(&send_btc_result.ckb_pay_req).expect("valid invoice");
    assert_eq!(fiber_invoice.payment_hash(), &hash);
    assert_eq!(fiber_invoice.hash_algorithm(), Some(&HashAlgorithm::Sha256));

    hub.insert_invoice(fiber_invoice.clone(), None);

    let hub_amount = fiber_invoice.amount.expect("has amount");
    assert!(
        hub_amount >= lnd_amount_sats.try_into().expect("valid amount"),
        "hub should receive more money than lnd, but we have hub_amount: {}, lnd_amount: {}",
        hub_amount,
        lnd_amount_sats
    );

    let res = fiber_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(hub.pubkey),
            invoice: Some(send_btc_result.ckb_pay_req.clone()),
            hold_payment: true,
            ..Default::default()
        })
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    assert_eq!(hash, payment_hash);

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    fiber_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

    assert_eq!(hub.get_invoice_status(&hash), Some(CkbInvoiceStatus::Paid));
    let hub_new_amount = hub.get_local_balance_from_channel(fiber_channel);
    assert_eq!(hub_new_amount, hub_old_amount + hub_amount);

    let lnd_new_amount = lnd_node.get_balance_sats().await;
    assert_eq!(lnd_new_amount, lnd_old_amount + lnd_amount_sats);
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_cross_chain_payment_hub_send_btc_always_success() {
    do_test_cross_chain_payment_hub_send_btc(get_always_success_script()).await;
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_cross_chain_payment_hub_send_btc_simple_udt() {
    do_test_cross_chain_payment_hub_send_btc(get_simple_udt_script()).await;
}

async fn do_test_cross_chain_payment_hub_receive_btc(udt_script: Script) {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let [mut fiber_node, mut hub] = NetworkNode::new_n_interconnected_nodes_with_config(2, |n| {
        let mut builder = NetworkNodeConfigBuilder::new();
        if n == 1 {
            let mut cch_config = CchConfig::default();
            cch_config.wrapped_btc_type_script = serialize_entity_to_hex_string(&udt_script);
            builder = builder.should_start_lnd(true).cch_config(cch_config);
        }
        builder.build()
    })
    .await
    .try_into()
    .expect("2 nodes");

    let (fiber_channel, _funding_tx) = establish_udt_channel_between_nodes(
        &mut hub,
        &mut fiber_node,
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
        udt_script.clone(),
    )
    .await;

    let mut lnd_node = LndNode::new(
        Default::default(),
        LndBitcoinDConf::Existing(hub.get_bitcoind()),
    )
    .await;

    lnd_node.make_some_money();
    lnd_node.open_channel_with(hub.get_lnd_node_mut()).await;

    // TODO: without the sleep below, we may fail to send the payment below. The root cause is unknown to me.
    // We will see two payments in the logs, which tells us the payment is failed because of FailureReasonInsufficientBalance.
    // Payment { payment_hash: "650feb233a22fb60a7e2458d03c0a5afa7043207a39c8c1c8a05d183bb5b7455", value: 100, creation_date: 1739422958, fee: 0, payment_preimage: "0000000000000000000000000000000000000000000000000000000000000000", value_sat: 100, value_msat: 100000, payment_request: "lnbcrt1u1pn66l8wpp5v587kge6ytakpflzgkxs8s9947nsgvs85wwgc8y2qhgc8w6mw32sdqqcqzzsxqyz5vqsp53k09akasd35ldkhl4twt9mmxd63cgu2l9j7jept03g6djv5nkazq9qxpqysgqq2dpmpqrsglycahtz4vsuy29a5kjhjt3w4ea664h0tfs0g5cwyn9dm54c2qe4tzxzatcw7dnfhuht5kewdqmn0zrg4cj7h74xejre2sqnhmf42", status: InFlight, fee_sat: 0, fee_msat: 0, creation_time_ns: 1739422958687770515, htlcs: [], payment_index: 1, failure_reason: FailureReasonNone })
    // Payment { payment_hash: "650feb233a22fb60a7e2458d03c0a5afa7043207a39c8c1c8a05d183bb5b7455", value: 100, creation_date: 1739422958, fee: 0, payment_preimage: "0000000000000000000000000000000000000000000000000000000000000000", value_sat: 100, value_msat: 100000, payment_request: "lnbcrt1u1pn66l8wpp5v587kge6ytakpflzgkxs8s9947nsgvs85wwgc8y2qhgc8w6mw32sdqqcqzzsxqyz5vqsp53k09akasd35ldkhl4twt9mmxd63cgu2l9j7jept03g6djv5nkazq9qxpqysgqq2dpmpqrsglycahtz4vsuy29a5kjhjt3w4ea664h0tfs0g5cwyn9dm54c2qe4tzxzatcw7dnfhuht5kewdqmn0zrg4cj7h74xejre2sqnhmf42", status: Failed, fee_sat: 0, fee_msat: 0, creation_time_ns: 1739422958687770515, htlcs: [], payment_index: 1, failure_reason: FailureReasonInsufficientBalance }
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let fiber_amount_sats: u128 = 100;
    let fiber_amount_msats = fiber_amount_sats * 1000;
    let preimage = gen_rand_sha256_hash();
    let fiber_invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(fiber_amount_msats))
        .payment_preimage(preimage.clone())
        .hash_algorithm(HashAlgorithm::Sha256)
        .payee_pub_key(fiber_node.pubkey.into())
        .expiry_time(Duration::from_secs(100))
        .build()
        .expect("build invoice success");
    fiber_node.insert_invoice(fiber_invoice.clone(), Some(preimage));
    let payment_hash = *fiber_invoice.payment_hash();

    let receive_btc_result: ReceiveBTCOrder = call_t!(
        hub.get_cch_actor(),
        CchMessage::ReceiveBTC,
        CALL_ACTOR_TIMEOUT_MS,
        ReceiveBTC {
            payment_hash: hex::encode(payment_hash),
            channel_id: fiber_channel,
            amount_sats: fiber_amount_sats,
            final_tlc_expiry: 10,
        }
    )
    .expect("receive btc actor call")
    .expect("receive btc result");

    let lightning_invoice =
        Bolt11Invoice::from_str(&receive_btc_result.btc_pay_req).expect("valid invoice");
    assert_eq!(
        payment_hash,
        Hash256::from(lightning_invoice.payment_hash().to_byte_array())
    );

    let hub_amount = lightning_invoice
        .amount_milli_satoshis()
        .expect("has amount");
    assert!(
        hub_amount >= fiber_amount_sats.try_into().expect("valid amount"),
        "hub should receive more money than lnd, but we have hub_amount: {}, lnd_amount: {}",
        hub_amount,
        fiber_amount_sats
    );

    let fiber_old_amount = fiber_node.get_local_balance_from_channel(fiber_channel);
    let hub_old_amount = hub.get_lnd_node_mut().get_balance_msats().await;

    lnd_node.send_payment(&lightning_invoice).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;

    hub.assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

    assert_eq!(
        fiber_node.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid)
    );
    let hub_new_amount = hub.get_lnd_node_mut().get_balance_msats().await;
    assert_eq!(hub_new_amount, hub_old_amount + hub_amount);

    let fiber_new_amount = fiber_node.get_local_balance_from_channel(fiber_channel);
    assert_eq!(fiber_new_amount, fiber_old_amount + fiber_amount_msats);
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_cross_chain_payment_hub_receive_btc_always_success() {
    do_test_cross_chain_payment_hub_receive_btc(get_always_success_script()).await;
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_cross_chain_payment_hub_receive_btc_simple_udt() {
    do_test_cross_chain_payment_hub_receive_btc(get_simple_udt_script()).await;
}
