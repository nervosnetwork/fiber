use std::str::FromStr;

use ckb_types::packed::Script;
use ractor::call_t;

use crate::{
    cch::{
        tests::lnd::{LndBitcoinDConf, LndNode},
        CchMessage, SendBTC, SendBTCOrder,
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
    invoice::{CkbInvoice, CkbInvoiceStatus, Currency},
    CchConfig,
};

pub const CALL_ACTOR_TIMEOUT_MS: u64 = 3 * 1000;

fn get_udt_args() -> Vec<u8> {
    hex::decode("32e555f3ff8e135cece1351a6a2971518392c1e30375c1e006ad0ce8eac07947").unwrap()
}

fn get_udt_script() -> Script {
    get_script_by_contract(Contract::Secp256k1Lock, &get_udt_args())
}

#[tokio::test]
async fn test_cross_chain_payment() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    let udt_script = get_udt_script();
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
    lnd_node.make_some_money();
    let lightning_channel = lnd_node.open_channel_with(hub.get_lnd_node_mut()).await;

    let lnd_amount = 100000;
    let add_invoice_result = lnd_node.add_invoice(lnd_amount as u64).await;

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

    // assert_eq!(
    //     fiber_invoice.payee_pub_key().copied(),
    //     Some(hub.pubkey.into())
    // );
    let hub_amount = fiber_invoice.amount.expect("has amount");
    // assert!(
    //     hub_amount >= lnd_amount,
    //     "hub should receive more money than lnd, but we have hub_amount: {}, lnd_amount: {}",
    //     hub_amount,
    //     lnd_amount
    // );

    let res = fiber_node
        .send_payment(SendPaymentCommand {
            target_pubkey: Some(hub.pubkey),
            amount: None,
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: Some(send_btc_result.ckb_pay_req.clone()),
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: None,
            hold_payment: true,
            udt_type_script: Some(udt_script.clone()),
            allow_self_payment: false,
            hop_hints: None,
            dry_run: false,
        })
        .await;

    assert!(res.is_ok());

    let payment_hash = res.unwrap().payment_hash;
    assert_eq!(hash, payment_hash);

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // For now, the payment is inflight because node 1 does not have the preimage yet.
    fiber_node
        .assert_payment_status(payment_hash, PaymentSessionStatus::Success, Some(1))
        .await;

    assert_eq!(hub.get_invoice_status(&hash), Some(CkbInvoiceStatus::Paid));
    let hub_new_amount = hub.get_local_balance_from_channel(fiber_channel);
    assert_eq!(hub_new_amount, hub_old_amount + hub_amount);

    // TODO: assert that lnd_node received the payment
}
