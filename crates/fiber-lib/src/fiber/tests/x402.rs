use crate::fiber::payment::SendPaymentCommand;
use crate::gen_rand_sha256_hash;
use crate::invoice::CkbInvoiceStatus;
use crate::rpc::config::RpcConfig;
use crate::tests::*;
use fiber_json_types::{Currency, NewInvoiceParams};
use fiber_types::Hash256;
use reqwest::StatusCode;
use serde_json::{json, Value};

const X402_ENABLED_MODULE: &str = "x402";

fn gen_x402_rpc_config() -> RpcConfig {
    let mut config = gen_rpc_config();
    config.enabled_modules.push(X402_ENABLED_MODULE.to_string());
    config
}

fn gen_x402_testnet_config(node_name: &str) -> NetworkNodeConfig {
    NetworkNodeConfigBuilder::new()
        .node_name(Some(node_name.to_string()))
        .base_dir_prefix(&format!("test-fnn-node-{node_name}-"))
        .rpc_config(Some(gen_x402_rpc_config()))
        .fiber_config_updater(|config| {
            config.chain = "testnet".to_string();
        })
        .build()
}

fn x402_verify_request(pay_to: String, invoice: String, payment_preimage: Hash256) -> Value {
    json!({
        "x402Version": 2,
        "paymentPayload": {
            "x402Version": 2,
            "accepted": {
                "scheme": "exact",
                "network": "fiber:testnet",
                "asset": "ckb",
                "amount": "1000",
                "payTo": pay_to,
                "maxTimeoutSeconds": 60,
                "extra": {},
            },
            "payload": {
                "invoice": invoice,
                "paymentPreimage": payment_preimage,
            },
        },
        "paymentRequirements": {
            "scheme": "exact",
            "network": "fiber:testnet",
            "asset": "ckb",
            "amount": "1000",
            "payTo": pay_to,
            "maxTimeoutSeconds": 60,
            "extra": {},
        },
    })
}

#[tokio::test]
async fn test_x402_supported_lists_exact_fiber_kind() {
    let node = NetworkNode::new_with_config(gen_x402_testnet_config("x402-supported-test")).await;

    let rpc_addr = node
        .rpc_server
        .as_ref()
        .map(|(_, addr)| addr)
        .expect("RPC server should be running");

    let response = reqwest::get(format!("http://{}/supported", rpc_addr))
        .await
        .expect("send request");
    assert_eq!(response.status(), StatusCode::OK);

    let json: Value = response.json().await.expect("parse supported response");

    let kinds = json
        .get("kinds")
        .and_then(Value::as_array)
        .expect("kinds array");
    assert!(kinds.iter().any(|kind| {
        kind.get("x402Version") == Some(&Value::from(2))
            && kind.get("scheme") == Some(&Value::from("exact"))
            && kind.get("network") == Some(&Value::from("fiber:testnet"))
    }));
}

#[tokio::test]
async fn test_x402_verify_rejects_invalid_preimage() {
    let merchant =
        NetworkNode::new_with_config(gen_x402_testnet_config("x402-verify-invalid")).await;

    let invoice = merchant
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("x402 invalid proof".to_string()),
            currency: Currency::Fibt,
            ..Default::default()
        })
        .await;
    let pay_to = hex::encode(merchant.get_public_key().serialize());

    let rpc_addr = merchant
        .rpc_server
        .as_ref()
        .map(|(_, addr)| addr)
        .expect("RPC server should be running");

    let response = reqwest::Client::new()
        .post(format!("http://{}/verify", rpc_addr))
        .json(&x402_verify_request(
            pay_to,
            invoice.invoice_address,
            gen_rand_sha256_hash(),
        ))
        .send()
        .await
        .expect("send verify request");
    assert_eq!(response.status(), StatusCode::OK);

    let json: Value = response.json().await.expect("parse verify response");
    assert_eq!(json.get("isValid"), Some(&Value::Bool(false)));
    assert_eq!(
        json.get("invalidReason"),
        Some(&Value::from("invalid_payment_preimage"))
    );
}

#[tokio::test]
async fn test_x402_verify_accepts_paid_invoice_proof() {
    let mut payer =
        NetworkNode::new_with_config(gen_x402_testnet_config("x402-verify-payer")).await;
    let mut merchant =
        NetworkNode::new_with_config(gen_x402_testnet_config("x402-verify-merchant")).await;

    payer.connect_to(&mut merchant).await;

    establish_channel_between_nodes(
        &mut payer,
        &mut merchant,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;

    let expected_preimage = gen_rand_sha256_hash();
    let pay_to = hex::encode(merchant.get_public_key().serialize());
    let invoice = merchant
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("x402 paid proof".to_string()),
            currency: Currency::Fibt,
            payment_preimage: Some(expected_preimage.into()),
            ..Default::default()
        })
        .await;

    let payment = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address.clone()),
            ..Default::default()
        })
        .await
        .expect("pay invoice");
    payer.wait_until_success(payment.payment_hash).await;

    assert_eq!(
        merchant.get_invoice_status(&payment.payment_hash),
        Some(CkbInvoiceStatus::Paid)
    );

    let payment_preimage = payer
        .get_payment_result(payment.payment_hash)
        .await
        .payment_preimage
        .expect("payment preimage");

    let rpc_addr = merchant
        .rpc_server
        .as_ref()
        .map(|(_, addr)| addr)
        .expect("RPC server should be running");

    let response = reqwest::Client::new()
        .post(format!("http://{}/verify", rpc_addr))
        .json(&x402_verify_request(
            pay_to,
            invoice.invoice_address,
            payment_preimage,
        ))
        .send()
        .await
        .expect("send verify request");
    assert_eq!(response.status(), StatusCode::OK);

    let json: Value = response.json().await.expect("parse verify response");
    assert_eq!(json.get("isValid"), Some(&Value::Bool(true)));
    assert!(json.get("invalidReason").is_none());
}

#[tokio::test]
async fn test_x402_settle_returns_receipt_for_verified_invoice() {
    let mut payer =
        NetworkNode::new_with_config(gen_x402_testnet_config("x402-settle-payer")).await;
    let mut merchant =
        NetworkNode::new_with_config(gen_x402_testnet_config("x402-settle-merchant")).await;

    payer.connect_to(&mut merchant).await;

    establish_channel_between_nodes(
        &mut payer,
        &mut merchant,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;

    let expected_preimage = gen_rand_sha256_hash();
    let pay_to = hex::encode(merchant.get_public_key().serialize());
    let invoice = merchant
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("x402 settle proof".to_string()),
            currency: Currency::Fibt,
            payment_preimage: Some(expected_preimage.into()),
            ..Default::default()
        })
        .await;

    let payment = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address.clone()),
            ..Default::default()
        })
        .await
        .expect("pay invoice");
    payer.wait_until_success(payment.payment_hash).await;

    let payment_preimage = payer
        .get_payment_result(payment.payment_hash)
        .await
        .payment_preimage
        .expect("payment preimage");

    let rpc_addr = merchant
        .rpc_server
        .as_ref()
        .map(|(_, addr)| addr)
        .expect("RPC server should be running");

    let response = reqwest::Client::new()
        .post(format!("http://{}/settle", rpc_addr))
        .json(&x402_verify_request(
            pay_to,
            invoice.invoice_address,
            payment_preimage,
        ))
        .send()
        .await
        .expect("send settle request");
    assert_eq!(response.status(), StatusCode::OK);

    let json: Value = response.json().await.expect("parse settle response");
    assert_eq!(json.get("success"), Some(&Value::Bool(true)));
    assert_eq!(json.get("network"), Some(&Value::from("fiber:testnet")));
    assert_eq!(json.get("amount"), Some(&Value::from("1000")));
    assert!(json
        .get("transaction")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty()));
}

#[tokio::test]
async fn test_x402_settle_rejects_invalid_preimage() {
    let merchant =
        NetworkNode::new_with_config(gen_x402_testnet_config("x402-settle-invalid")).await;

    let invoice = merchant
        .gen_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("x402 invalid settle proof".to_string()),
            currency: Currency::Fibt,
            ..Default::default()
        })
        .await;
    let pay_to = hex::encode(merchant.get_public_key().serialize());

    let rpc_addr = merchant
        .rpc_server
        .as_ref()
        .map(|(_, addr)| addr)
        .expect("RPC server should be running");

    let response = reqwest::Client::new()
        .post(format!("http://{}/settle", rpc_addr))
        .json(&x402_verify_request(
            pay_to,
            invoice.invoice_address,
            gen_rand_sha256_hash(),
        ))
        .send()
        .await
        .expect("send settle request");
    assert_eq!(response.status(), StatusCode::OK);

    let json: Value = response.json().await.expect("parse settle response");
    assert_eq!(json.get("success"), Some(&Value::Bool(false)));
    assert_eq!(
        json.get("errorReason"),
        Some(&Value::from("invalid_payment_preimage"))
    );
    assert_eq!(json.get("transaction"), Some(&Value::from("")));
}
