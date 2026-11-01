//! Cross-tenant coverage for the ordinary `get_invoice`/`get_payment` query RPCs.

use std::str::FromStr;

use biscuit_auth::{macros::biscuit, KeyPair, PrivateKey};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};

use super::integration::{authenticated_client, create_lsp_test_network_inner};
use crate::{
    fiber::{
        graph::NetworkGraphStateStore,
        payment::{PaymentSession, SendPaymentData},
    },
    fiber_types::{Hash256, PaymentStatus, Pubkey},
    invoice::{CkbInvoice, Currency, InvoiceBuilder, InvoiceStore},
    rpc::{
        invoice::{GetInvoiceParams, GetInvoiceResult},
        payment::{GetPaymentCommandParams, GetPaymentCommandResult},
        server::start_rpc,
    },
    tests::{gen_rpc_config, get_test_root_actor, init_tracing, HUGE_CKB_AMOUNT},
};

fn invoice(payment_hash: Hash256) -> CkbInvoice {
    InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1_000))
        .payment_hash(payment_hash)
        .build()
        .expect("build test invoice")
}

fn payment_session(payment_hash: Hash256, target_pubkey: Pubkey) -> PaymentSession {
    PaymentSession {
        request: SendPaymentData {
            target_pubkey,
            amount: 1_000,
            payment_hash,
            invoice: None,
            final_tlc_expiry_delta: 0,
            tlc_expiry_limit: 0,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: false,
            udt_type_script: None,
            preimage: None,
            custom_records: None,
            allow_self_payment: false,
            hop_hints: Vec::new(),
            router: Vec::new(),
            allow_mpp: false,
            dry_run: false,
            trampoline_hops: None,
            trampoline_context: None,
        },
        last_error: None,
        last_error_code: None,
        try_limit: 3,
        status: PaymentStatus::Created,
        created_at: 1,
        last_updated_at: 1,
        cached_attempts: Vec::new(),
    }
}

fn invoice_params(payment_hash: Hash256, tenant_id: Option<&str>) -> GetInvoiceParams {
    GetInvoiceParams {
        payment_hash: payment_hash.into(),
        tenant_id: tenant_id.map(str::to_string),
    }
}

fn payment_params(payment_hash: Hash256, tenant_id: Option<&str>) -> GetPaymentCommandParams {
    GetPaymentCommandParams {
        payment_hash: payment_hash.into(),
        tenant_id: tenant_id.map(str::to_string),
    }
}

#[tokio::test]
async fn query_rpcs_resolve_tenant_scope_and_operator_lsp_permission() {
    init_tracing();

    let network = create_lsp_test_network_inner(
        &[],
        2,
        &[
            ((0, "u1"), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, "u2"), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
        ],
        false,
    )
    .await;
    let lsp = network.lsp(0);
    let tenant_a = network.tenant(0, "u1");
    let tenant_b = network.tenant(0, "u2");
    let public_node = &network.nodes[0];

    let tenant_a_invoice_hash = Hash256::from([0xA1; 32]);
    let tenant_b_invoice_hash = Hash256::from([0xB1; 32]);
    let public_invoice_hash = Hash256::from([0xC1; 32]);
    tenant_a
        .node
        .store
        .insert_invoice(invoice(tenant_a_invoice_hash), None)
        .expect("insert tenant A invoice");
    tenant_b
        .node
        .store
        .insert_invoice(invoice(tenant_b_invoice_hash), None)
        .expect("insert tenant B invoice");
    public_node
        .store
        .insert_invoice(invoice(public_invoice_hash), None)
        .expect("insert public invoice");

    let tenant_a_payment_hash = Hash256::from([0xA2; 32]);
    let tenant_b_payment_hash = Hash256::from([0xB2; 32]);
    let public_payment_hash = Hash256::from([0xC2; 32]);
    tenant_a
        .node
        .store
        .insert_payment_session(payment_session(tenant_a_payment_hash, tenant_a.node.pubkey));
    tenant_b
        .node
        .store
        .insert_payment_session(payment_session(tenant_b_payment_hash, tenant_b.node.pubkey));
    public_node
        .store
        .insert_payment_session(payment_session(public_payment_hash, public_node.pubkey));

    let private_key =
        PrivateKey::from_str(&lsp.biscuit_private_key).expect("parse test Biscuit key");
    let biscuit_root = KeyPair::from(&private_key);
    let query_token = biscuit!(r#"read("invoices"); read("payments");"#)
        .build(&biscuit_root)
        .expect("build query operator token")
        .to_base64()
        .expect("encode query operator token");
    let lsp_query_token = biscuit!(r#"read("invoices"); read("payments"); read("lsp");"#)
        .build(&biscuit_root)
        .expect("build LSP query operator token")
        .to_base64()
        .expect("encode LSP query operator token");
    let query_client = authenticated_client(lsp.rpc_addr, &query_token);
    let lsp_query_client = authenticated_client(lsp.rpc_addr, &lsp_query_token);

    // Tenant tokens default to their own namespace and may repeat their own id.
    for tenant_id in [None, Some(tenant_a.tenant_id.as_str())] {
        let invoice: GetInvoiceResult = tenant_a
            .client
            .request(
                "get_invoice",
                rpc_params![invoice_params(tenant_a_invoice_hash, tenant_id)],
            )
            .await
            .expect("tenant A reads its invoice");
        assert_eq!(
            invoice.invoice.data.payment_hash,
            tenant_a_invoice_hash.into()
        );

        let payment: GetPaymentCommandResult = tenant_a
            .client
            .request(
                "get_payment",
                rpc_params![payment_params(tenant_a_payment_hash, tenant_id)],
            )
            .await
            .expect("tenant A reads its payment");
        assert_eq!(payment.payment_hash, tenant_a_payment_hash.into());
    }

    // Cross-tenant selection is always denied for a tenant token.
    let invoice_error = tenant_a
        .client
        .request::<GetInvoiceResult, _>(
            "get_invoice",
            rpc_params![invoice_params(
                tenant_b_invoice_hash,
                Some(tenant_b.tenant_id.as_str())
            )],
        )
        .await
        .err()
        .expect("tenant A must not query tenant B's invoice");
    assert!(
        invoice_error.to_string().contains("another tenant"),
        "{invoice_error}"
    );
    let payment_error = tenant_b
        .client
        .request::<GetPaymentCommandResult, _>(
            "get_payment",
            rpc_params![payment_params(
                tenant_a_payment_hash,
                Some(tenant_a.tenant_id.as_str())
            )],
        )
        .await
        .expect_err("tenant B must not query tenant A's payment");
    assert!(
        payment_error.to_string().contains("another tenant"),
        "{payment_error}"
    );

    // Operator targeting a tenant needs the normal query capability plus read("lsp").
    let invoice_error = query_client
        .request::<GetInvoiceResult, _>(
            "get_invoice",
            rpc_params![invoice_params(
                tenant_a_invoice_hash,
                Some(tenant_a.tenant_id.as_str())
            )],
        )
        .await
        .err()
        .expect("invoice query without read(\"lsp\") must fail");
    assert!(
        matches!(&invoice_error, jsonrpsee::core::ClientError::Call(error)
            if error.message().contains("read(\"lsp\")")),
        "{invoice_error}"
    );
    let payment_error = query_client
        .request::<GetPaymentCommandResult, _>(
            "get_payment",
            rpc_params![payment_params(
                tenant_a_payment_hash,
                Some(tenant_a.tenant_id.as_str())
            )],
        )
        .await
        .expect_err("payment query without read(\"lsp\") must fail");
    assert!(
        matches!(&payment_error, jsonrpsee::core::ClientError::Call(error)
            if error.message().contains("read(\"lsp\")")),
        "{payment_error}"
    );

    // With both capabilities the operator can select either hosted tenant.
    for (tenant, invoice_hash, payment_hash) in [
        (tenant_a, tenant_a_invoice_hash, tenant_a_payment_hash),
        (tenant_b, tenant_b_invoice_hash, tenant_b_payment_hash),
    ] {
        let invoice: GetInvoiceResult = lsp_query_client
            .request(
                "get_invoice",
                rpc_params![invoice_params(
                    invoice_hash,
                    Some(tenant.tenant_id.as_str())
                )],
            )
            .await
            .expect("operator reads selected tenant invoice");
        assert_eq!(invoice.invoice.data.payment_hash, invoice_hash.into());

        let payment: GetPaymentCommandResult = lsp_query_client
            .request(
                "get_payment",
                rpc_params![payment_params(
                    payment_hash,
                    Some(tenant.tenant_id.as_str())
                )],
            )
            .await
            .expect("operator reads selected tenant payment");
        assert_eq!(payment.payment_hash, payment_hash.into());
    }

    // Original clients send only payment_hash; a missing tenant_id keeps the public namespace.
    let public_invoice: GetInvoiceResult = query_client
        .request(
            "get_invoice",
            rpc_params![serde_json::json!({
                "payment_hash": fiber_json_types::Hash256::from(public_invoice_hash),
            })],
        )
        .await
        .expect("operator reads public invoice");
    assert_eq!(
        public_invoice.invoice.data.payment_hash,
        public_invoice_hash.into()
    );
    let public_payment: GetPaymentCommandResult = query_client
        .request(
            "get_payment",
            rpc_params![serde_json::json!({
                "payment_hash": fiber_json_types::Hash256::from(public_payment_hash),
            })],
        )
        .await
        .expect("operator reads public payment");
    assert_eq!(public_payment.payment_hash, public_payment_hash.into());

    // An unknown or unavailable target is an error, never a public fallback.
    let missing_error = lsp_query_client
        .request::<GetInvoiceResult, _>(
            "get_invoice",
            rpc_params![invoice_params(tenant_a_invoice_hash, Some("missing"))],
        )
        .await
        .err()
        .expect("unknown tenant must fail");
    assert!(
        missing_error.to_string().contains("not registered"),
        "{missing_error}"
    );

    // Auth-disabled local RPC may explicitly select a tenant.
    let mut local_config = gen_rpc_config();
    local_config.enabled_modules = vec!["invoice".to_string(), "payment".to_string()];
    let root_actor = get_test_root_actor().await;
    let crate::rpc::server::RpcServer {
        handle: local_handle,
        http_address: local_addr,
        ..
    } = start_rpc(
        local_config,
        None,
        Some(public_node.fiber_config.clone()),
        Some(public_node.network_actor.clone()),
        None,
        Some(lsp.actor.clone()),
        public_node.store.clone(),
        None,
        None,
        root_actor.get_cell(),
        None,
        public_node.signing_events.clone(),
        #[cfg(debug_assertions)]
        None,
        #[cfg(debug_assertions)]
        None,
    )
    .await
    .expect("start auth-disabled local RPC");
    let local_client = HttpClientBuilder::default()
        .build(format!("http://{local_addr}"))
        .expect("build local RPC client");
    let local_invoice: GetInvoiceResult = local_client
        .request(
            "get_invoice",
            rpc_params![invoice_params(
                tenant_a_invoice_hash,
                Some(tenant_a.tenant_id.as_str())
            )],
        )
        .await
        .expect("local RPC selects tenant invoice");
    assert_eq!(
        local_invoice.invoice.data.payment_hash,
        tenant_a_invoice_hash.into()
    );
    let local_payment: GetPaymentCommandResult = local_client
        .request(
            "get_payment",
            rpc_params![payment_params(
                tenant_a_payment_hash,
                Some(tenant_a.tenant_id.as_str())
            )],
        )
        .await
        .expect("local RPC selects tenant payment");
    assert_eq!(local_payment.payment_hash, tenant_a_payment_hash.into());

    local_handle.stop().expect("stop local RPC server");
    local_handle.stopped().await;
    network.stop().await;
}
