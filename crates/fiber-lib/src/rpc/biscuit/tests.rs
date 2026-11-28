use std::time::{Duration, SystemTime, UNIX_EPOCH};

use biscuit_auth::{
    macros::{biscuit, block},
    KeyPair,
};

#[cfg(feature = "lsp")]
use crate::lsp::BiscuitTokenIssuer;
use crate::rpc::biscuit::{extract_node_id, extract_tenant_id};
use crate::rpc::tenant::scoped_rpc_node_id;
use fiber_types::{NodeId, TenantId};

use super::{build_authorizer, AuthRule, BiscuitAuth};

#[cfg(feature = "lsp")]
#[test]
fn tenant_token_issuer_binds_identity_and_data_plane_capabilities() {
    let root = KeyPair::new();
    let issuer = BiscuitTokenIssuer::from_private_key(
        &root.private().to_prefixed_string(),
        &root.public().to_string(),
    )
    .unwrap();
    let node_id = NodeId::from_bytes(vec![1, 2, 3]);
    let token = issuer
        .issue_tenant_token(
            &TenantId::new("u1".to_string()).unwrap(),
            &node_id,
            crate::now_timestamp_as_millis_u64() / 1_000 + 3_600,
            0,
        )
        .unwrap();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    let (biscuit, _) = auth
        .check_permission("get_channel_signing_status", &token)
        .unwrap();
    assert_eq!(
        extract_tenant_id(&biscuit).unwrap(),
        Some(TenantId::new("u1".to_string()).unwrap())
    );
    assert_eq!(extract_node_id(&biscuit).unwrap(), node_id);
    auth.check_permission("submit_channel_signature", &token)
        .unwrap();
    auth.check_permission("list_channels", &token).unwrap();
    auth.check_permission("open_channel_with_external_funding", &token)
        .unwrap();
    assert!(auth
        .check_permission("open_tenant_channel", &token)
        .is_err());
    auth.check_permission("new_invoice", &token).unwrap();
    auth.check_permission("settle_invoice", &token).unwrap();
    auth.check_permission("send_payment", &token).unwrap();
    auth.check_permission("get_invoice", &token).unwrap();
    auth.check_permission("get_payment", &token).unwrap();
    auth.check_permission("list_payments", &token).unwrap();
    auth.check_permission("get_watchtower_signing_status", &token)
        .unwrap();
    auth.check_permission("submit_watchtower_signature", &token)
        .unwrap();
    for method in ["subscribe", "unsubscribe"] {
        auth.check_permission(method, &token).unwrap();
        crate::rpc::tenant::enforce_tenant_method_allowlist(method, &biscuit).unwrap();
    }
    crate::rpc::tenant::enforce_tenant_method_allowlist("get_channel_signing_status", &biscuit)
        .unwrap();
    crate::rpc::tenant::enforce_tenant_method_allowlist(
        "open_channel_with_external_funding",
        &biscuit,
    )
    .unwrap();
    assert!(crate::rpc::tenant::enforce_tenant_method_allowlist("open_channel", &biscuit).is_err());
    assert!(
        crate::rpc::tenant::enforce_tenant_method_allowlist("open_tenant_channel", &biscuit)
            .is_err()
    );
    // write("channels") still exists so biscuit allows node-operation
    // methods; middleware is the tenant method gate.
    auth.check_permission("open_channel", &token).unwrap();
    auth.check_permission("create_preimage", &token).unwrap();
    crate::rpc::tenant::enforce_tenant_method_allowlist("create_preimage", &biscuit).unwrap();
    crate::rpc::tenant::enforce_tenant_method_allowlist("new_invoice", &biscuit).unwrap();
    crate::rpc::tenant::enforce_tenant_method_allowlist("settle_invoice", &biscuit).unwrap();
    crate::rpc::tenant::enforce_tenant_method_allowlist("send_payment", &biscuit).unwrap();
    assert!(auth
        .check_permission("lsp_register_tenant", &token)
        .is_err());
    #[cfg(all(debug_assertions, feature = "debug-add-tlc"))]
    auth.check_permission("check_channel_shutdown", &token)
        .unwrap();
    #[cfg(not(all(debug_assertions, feature = "debug-add-tlc")))]
    assert!(auth
        .check_permission("check_channel_shutdown", &token)
        .is_err());

    let other_root = KeyPair::new();
    assert!(BiscuitTokenIssuer::from_private_key(
        &other_root.private().to_prefixed_string(),
        &root.public().to_string(),
    )
    .is_err());
}

#[test]
fn scoped_rpc_node_id_keeps_tenant_tokens_off_the_host_namespace() {
    let root = KeyPair::new();
    let tenant_id = TenantId::new("u1").unwrap();
    let tenant_node = NodeId::from_bytes(vec![1, 2, 3]);
    let tenant_token = biscuit!(
        r#"
            tenant({tenant});
            node({node});
            write("watchtower");
        "#,
        tenant = tenant_id.as_str(),
        node = tenant_node.to_string(),
    )
    .build(&root)
    .unwrap();
    assert_eq!(scoped_rpc_node_id(&tenant_token).unwrap(), tenant_node);

    let host_tenant_token = biscuit!(
        r#"
            tenant({tenant});
            node({node});
            write("watchtower");
        "#,
        tenant = tenant_id.as_str(),
        node = NodeId::local().to_string(),
    )
    .build(&root)
    .unwrap();
    assert!(scoped_rpc_node_id(&host_tenant_token)
        .unwrap_err()
        .to_string()
        .contains("host watchtower namespace"));

    let tenant_without_node = biscuit!(
        r#"
            tenant({tenant});
            write("watchtower");
        "#,
        tenant = tenant_id.as_str(),
    )
    .build(&root)
    .unwrap();
    assert!(scoped_rpc_node_id(&tenant_without_node)
        .unwrap_err()
        .to_string()
        .contains("missing a node fact"));

    let operator = biscuit!(r#"write("watchtower");"#).build(&root).unwrap();
    assert_eq!(scoped_rpc_node_id(&operator).unwrap(), NodeId::local());
}

#[test]
fn test_biscuit_auth_uses_ten_millisecond_authorizer_limit() {
    let rule = AuthRule::new(r#"allow if read("payments");"#);
    let authorizer = rule.build_rule().unwrap();
    let limits = authorizer.limits();
    let default_limits = biscuit_auth::AuthorizerLimits::default();

    assert_eq!(limits.max_time, Duration::from_millis(10));
    assert_eq!(limits.max_facts, default_limits.max_facts);
    assert_eq!(limits.max_iterations, default_limits.max_iterations);
}

#[test]
fn test_node_id_query_uses_ten_millisecond_authorizer_limit() {
    let root = KeyPair::new();
    let token = biscuit!(r#"node("test-node-id");"#).build(&root).unwrap();
    let authorizer = build_authorizer(&token).unwrap();
    let limits = authorizer.limits();

    assert_eq!(limits.max_time, Duration::from_millis(10));
}

#[test]
fn test_extract_tenant_id_only_trusts_the_authority_block() {
    let root = KeyPair::new();
    let tenant_token = biscuit!(r#"tenant("u1");"#).build(&root).unwrap();
    assert_eq!(
        extract_tenant_id(&tenant_token).unwrap(),
        Some(TenantId::new("u1").unwrap())
    );

    let public_token = biscuit!(r#"read("channels");"#).build(&root).unwrap();
    assert_eq!(extract_tenant_id(&public_token).unwrap(), None);

    let attenuated = public_token
        .append(block!(r#"tenant("u1");"#))
        .expect("append untrusted tenant fact");
    assert_eq!(extract_tenant_id(&attenuated).unwrap(), None);
}

#[test]
fn test_extract_tenant_id_rejects_invalid_authority_facts() {
    let root = KeyPair::new();
    let invalid = biscuit!(r#"tenant("../u1");"#).build(&root).unwrap();
    assert!(extract_tenant_id(&invalid).is_err());

    let ambiguous = biscuit!(
        r#"
            tenant("u1");
            tenant("u2");
        "#
    )
    .build(&root)
    .unwrap();
    assert!(extract_tenant_id(&ambiguous).is_err());
}

#[test]
fn test_biscuit_auth_accepts_padded_and_unpadded_tokens() {
    let root = KeyPair::new();

    // auth
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    // sign a biscuit
    let token = biscuit!(
        r#"
      write("payments");
      read("peers");
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();

    for token in [token.as_str(), token.trim_end_matches('=')] {
        assert!(auth.check_permission("send_payment", token).is_ok());
        // Write permission does not imply read permission.
        assert!(auth.check_permission("get_payment", token).is_err());
        assert!(auth.check_permission("list_peers", token).is_ok());
        assert!(auth.check_permission("connect_peer", token).is_err());
        // Methods without a matching rule are denied.
        assert!(auth.check_permission("unknown", token).is_err());
    }
}

#[test]
fn test_biscuit_auth_backup_requires_node_write() {
    let root = KeyPair::new();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    let write_token = biscuit!(r#"write("node");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let read_token = biscuit!(r#"read("node");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let unrelated_token = biscuit!(r#"write("payments");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();

    assert!(auth.check_permission("backup", &write_token).is_ok());
    assert!(auth.check_permission("backup", &read_token).is_err());
    assert!(auth.check_permission("backup", &unrelated_token).is_err());
}

#[test]
fn test_biscuit_auth_cch_receive_btc_requires_write() {
    let root = KeyPair::new();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    let read_token = biscuit!(
        r#"
      read("cch");
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();
    let write_token = biscuit!(
        r#"
      write("cch");
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();
    let internal_token = biscuit!(
        r#"
      internal("store_changes");
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();

    assert!(auth.check_permission("get_cch_order", &read_token).is_ok());
    assert!(auth
        .check_permission("subscribe_store_changes", &read_token)
        .is_err());
    assert!(auth.check_permission("send_btc", &read_token).is_err());
    assert!(auth.check_permission("receive_btc", &read_token).is_err());

    assert!(auth.check_permission("send_btc", &write_token).is_ok());
    assert!(auth.check_permission("receive_btc", &write_token).is_ok());
    assert!(auth
        .check_permission("subscribe_store_changes", &write_token)
        .is_err());
    assert!(auth
        .check_permission("get_cch_order", &write_token)
        .is_err());
    assert!(auth
        .check_permission("subscribe_store_changes", &internal_token)
        .is_ok());
}

#[test]
fn test_biscuit_auth_lsp_separates_read_and_write_methods() {
    let root = KeyPair::new();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();
    let read_token = biscuit!(r#"read("lsp");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let write_token = biscuit!(r#"write("lsp");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();

    let read_methods = [
        "lsp_get_status",
        "lsp_list_tenants",
        "lsp_get_payment_delivery",
    ];
    let write_methods = [
        "lsp_register_tenant",
        "lsp_ensure_tenant",
        "lsp_evict_tenant",
    ];

    for method in read_methods {
        assert!(auth.check_permission(method, &read_token).is_ok());
        assert!(auth.check_permission(method, &write_token).is_err());
    }
    for method in write_methods {
        assert!(auth.check_permission(method, &write_token).is_ok());
        assert!(auth.check_permission(method, &read_token).is_err());
    }
    for method in ["lsp_get_invoice", "lsp_get_payment"] {
        assert!(
            auth.get_rule(method).is_err(),
            "{method} rule must be removed"
        );
    }
}

#[test]
fn test_biscuit_auth_channel_signing_separates_read_and_write() {
    let root = KeyPair::new();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();
    let read_token = biscuit!(r#"read("channels");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let write_token = biscuit!(r#"write("channels");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();

    assert!(auth
        .check_permission("get_channel_signing_status", &read_token)
        .is_ok());
    assert!(auth
        .check_permission("get_channel_signing_status", &write_token)
        .is_err());
    assert!(auth
        .check_permission("submit_channel_signature", &write_token)
        .is_ok());
    assert!(auth
        .check_permission("submit_channel_signature", &read_token)
        .is_err());
}

#[test]
fn test_biscuit_auth_watchtower_signing_separates_read_and_write() {
    let root = KeyPair::new();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();
    let read_token = biscuit!(r#"read("watchtower");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let write_token = biscuit!(r#"write("watchtower");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let channel_read_token = biscuit!(r#"read("channels");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    let channel_write_token = biscuit!(r#"write("channels");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    assert!(auth
        .check_permission("get_watchtower_signing_status", &read_token)
        .is_ok());
    assert!(auth
        .check_permission("get_watchtower_signing_status", &write_token)
        .is_ok());
    assert!(auth
        .check_permission("get_watchtower_signing_status", &channel_read_token)
        .is_ok());
    assert!(auth
        .check_permission("get_watchtower_signing_status", &channel_write_token)
        .is_err());
    assert!(auth
        .check_permission("submit_watchtower_signature", &write_token)
        .is_ok());
    assert!(auth
        .check_permission("submit_watchtower_signature", &read_token)
        .is_err());
    assert!(auth
        .check_permission("submit_watchtower_signature", &channel_write_token)
        .is_ok());
    assert!(auth
        .check_permission("submit_watchtower_signature", &channel_read_token)
        .is_err());
    let invoice_write_token = biscuit!(r#"write("invoices");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();
    assert!(auth
        .check_permission("create_preimage", &invoice_write_token)
        .is_err());
}

#[test]
fn test_biscuit_auth_dev_methods_require_write_dev() {
    let root = KeyPair::new();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    let production_write_token = biscuit!(
        r#"
      write("channels");
      write("messages");
      write("chain");
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();

    let dev_write_token = biscuit!(
        r#"
      write("dev");
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();

    let dev_methods = [
        "commitment_signed",
        "add_tlc",
        "remove_tlc",
        "check_channel_shutdown",
        "sign_external_funding_tx",
        "submit_commitment_transaction",
    ];

    for method in dev_methods {
        assert!(auth
            .check_permission(method, &production_write_token)
            .is_err());
        assert!(auth.check_permission(method, &dev_write_token).is_ok());
    }

    assert!(auth
        .check_permission("open_channel", &production_write_token)
        .is_ok());
    assert!(auth
        .check_permission("submit_signed_funding_tx", &dev_write_token)
        .is_err());
}

#[test]
fn test_biscuit_auth_with_wrong_token() {
    let root = KeyPair::new();
    let invalid_root = KeyPair::new();
    assert_ne!(root.public(), invalid_root.public());

    // auth
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    // sign a biscuit
    let token = biscuit!(
        r#"
      write("payments");
"#
    )
    .build(&invalid_root)
    .unwrap()
    .to_base64()
    .unwrap();

    // check permission
    assert!(auth.check_permission("send_payment", &token).is_err());
}

#[test]
fn test_biscuit_auth_watchtower() {
    let root = KeyPair::new();

    // auth
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    // sign a biscuit
    let token1 = biscuit!(
        r#"
      write("watchtower");
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();

    // check permission
    assert!(auth.check_permission("update_revocation", &token1,).is_ok());
}

#[test]
fn test_biscuit_token_timeout() {
    let root = KeyPair::new();

    // auth
    let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    // sign a biscuit with timeout
    let token = biscuit!(
        r#"
            write("payments");
            check if time($time), $time <= 2022-03-30T20:00:00Z;
"#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();

    // check permission
    let future_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let past_time = Duration::from_millis(10).as_millis() as u64;
    assert!(auth
        .check_permission_with_time("send_payment", &token, future_time)
        .is_err());
    assert!(auth
        .check_permission_with_time("send_payment", &token, past_time)
        .is_ok());
}

#[test]
fn test_biscuit_revocation() {
    let root = KeyPair::new();

    // auth
    let mut auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

    // sign a biscuit
    let token = {
        let biscuit = biscuit!(
            r#"
      write("payments");
      read("peers");
"#
        )
        .build(&root)
        .unwrap();

        biscuit.to_base64().unwrap()
    };

    // sign a exactly same content revoked token
    let (rev_token, rev_id) = {
        let biscuit = biscuit!(
            r#"
      write("payments");
      read("peers");
"#
        )
        .build(&root)
        .unwrap();
        let rev_id = hex::encode(&biscuit.revocation_identifiers()[0]);

        (biscuit.to_base64().unwrap(), rev_id)
    };
    auth.extend_revocation_list(&[rev_id]).unwrap();

    // check permission
    assert!(auth.check_permission("send_payment", &token,).is_ok());
    // write permission do not implies read
    assert!(auth.check_permission("list_peers", &token).is_ok());

    // check revoked token
    assert!(auth.check_permission("send_payment", &rev_token,).is_err());
    // write permission do not implies read
    assert!(auth.check_permission("list_peers", &rev_token).is_err());
    assert!(auth.check_permission("get_payment", &token).is_err());

    // if not match any rule, it should be denied
    assert!(auth.check_permission("unknown", &token).is_err());
    assert!(auth.check_permission("unknown", &rev_token).is_err());
}

#[test]
fn test_parse_biscuit_token() {
    // Pubkey
    let pubkey = "ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79";
    // token with all permission
    let token = "EpoDCq8CCi5RbWJ2UmpKSEFRRG1qM2NnblVCR1E1elZuR3hVS3diMnFKeWd3TnMyd2s0MWg4CgNjY2gKCGNoYW5uZWxzCghtZXNzYWdlcwoFY2hhaW4KBWdyYXBoCghpbnZvaWNlcwoIcGF5bWVudHMKBXBlZXJzCgp3YXRjaHRvd2VyGAMiCQoHCBgSAxiACCIJCgcIARIDGIEIIgkKBwgAEgMYgQgiCQoHCAESAxiCCCIJCgcIABIDGIIIIgkKBwgBEgMYgwgiCQoHCAESAxiECCIJCgcIABIDGIUIIggKBggAEgIYGCIJCgcIARIDGIYIIgkKBwgAEgMYhggiCQoHCAESAxiHCCIJCgcIABIDGIcIIgkKBwgBEgMYiAgiCQoHCAASAxiICCIJCgcIARIDGIkIEiQIABIgdmX3T0R0Fu6vXZUeVD21AZU7K85xSAmsFAOQSqBd1aYaQNdmA5kqLQ6qNVmOxB0uiEFjUq_OQ0DkZgtYt0mSHktbpubKHzJo-imaFx6Pz7f4NzkOvcLLLFUwBlyHxGyEkQAiIgogAEX-eNhS6VprsYL7moBGaJeVUAnn5VzKLr77Rz9y3AI=";
    // auth
    let auth = BiscuitAuth::from_pubkey(pubkey.to_string()).unwrap();

    // check permission
    assert!(auth.check_permission("send_payment", token).is_ok());
    // write permission do not implies read
    assert!(auth.check_permission("get_payment", token).is_ok());
    assert!(auth.check_permission("list_peers", token).is_ok());
    assert!(auth.check_permission("connect_peer", token).is_ok());

    // if not match any rule, it should be denied
    assert!(auth.check_permission("unknown", token).is_err());

    let b = auth.extract_biscuit(token).unwrap();
    let node_id = extract_node_id(&b).unwrap();
    assert!(!node_id.as_ref().is_empty());
    println!("node_id: {node_id:?}");
}

#[cfg(feature = "lsp")]
#[test]
fn test_runtime_lsp_disabled_rejects_tenant_token_but_allows_operator() {
    let root = KeyPair::new();
    let issuer = BiscuitTokenIssuer::from_private_key(
        &root.private().to_prefixed_string(),
        &root.public().to_string(),
    )
    .unwrap();
    let tenant_id = TenantId::new("tenant-runtime-off").unwrap();
    let node_id = NodeId::from_bytes(vec![9, 8, 7]);
    let tenant_token = issuer
        .issue_tenant_token(
            &tenant_id,
            &node_id,
            crate::now_timestamp_as_millis_u64() / 1_000 + 3_600,
            0,
        )
        .unwrap();
    let operator_token = biscuit!(r#"write("watchtower");"#)
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();

    let auth = BiscuitAuth::from_pubkey(root.public().to_string())
        .unwrap()
        .with_lsp_enabled(false);
    let err = auth
        .check_permission("create_watch_channel", &tenant_token)
        .err()
        .expect("tenant token must be rejected when LSP is disabled");
    assert!(
        err.to_string()
            .contains("hosted LSP service is not enabled"),
        "{err}"
    );
    assert!(auth
        .check_permission("create_watch_channel", &operator_token)
        .is_ok());
}

#[cfg(feature = "lsp")]
#[test]
fn test_tenant_token_epoch_revocation() {
    use crate::lsp::{HostedTenantRecord, TenantRegistry};

    let (store, _dir) = crate::generate_store();
    let registry = TenantRegistry::new(store);
    let root = KeyPair::new();
    let issuer = BiscuitTokenIssuer::from_private_key(
        &root.private().to_prefixed_string(),
        &root.public().to_string(),
    )
    .unwrap();
    let tenant_id = TenantId::new("tenant-revoke").unwrap();
    let tenant_pubkey = fiber_types::Privkey::from([7u8; 32]).pubkey();
    let node_id = NodeId::from_bytes(tenant_pubkey.serialize().to_vec());
    registry
        .register(HostedTenantRecord {
            tenant_id: tenant_id.clone(),
            root_signer_pubkey: None,
            tenant_pubkey,
            created_at: 1,
            token_epoch: 0,
        })
        .unwrap();
    let expires = crate::now_timestamp_as_millis_u64() / 1_000 + 3_600;

    let token_epoch_0 = issuer
        .issue_tenant_token(&tenant_id, &node_id, expires, 0)
        .unwrap();
    let token_epoch_1 = issuer
        .issue_tenant_token(&tenant_id, &node_id, expires, 1)
        .unwrap();
    // Legacy token without an explicit token_epoch fact defaults to epoch 0.
    let legacy_token = biscuit!(
        r#"
        tenant("tenant-revoke");
        write("invoices");
        "#
    )
    .build(&root)
    .unwrap()
    .to_base64()
    .unwrap();

    let epochs: super::TenantTokenEpochs = Default::default();
    let auth = BiscuitAuth::from_pubkey(root.public().to_string())
        .unwrap()
        .with_lsp_enabled(true)
        .with_tenant_token_epochs(epochs.clone());

    assert!(auth.check_permission("new_invoice", &token_epoch_0).is_ok());
    assert!(auth.check_permission("new_invoice", &legacy_token).is_ok());

    // Revoke in DB and update shared epochs map -> epoch 0 and legacy tokens are immediately rejected.
    let new_epoch = registry.revoke_tenant_tokens(&tenant_id).unwrap();
    assert_eq!(new_epoch, 1);
    epochs.set(tenant_id, new_epoch);
    assert!(auth
        .check_permission("new_invoice", &token_epoch_0)
        .err()
        .expect("epoch 0 token must be revoked")
        .to_string()
        .contains("revoked"));
    assert!(auth
        .check_permission("new_invoice", &legacy_token)
        .err()
        .expect("legacy token must be revoked")
        .to_string()
        .contains("revoked"));
    assert!(auth.check_permission("new_invoice", &token_epoch_1).is_ok());
}
