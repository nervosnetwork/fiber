use std::sync::{atomic::AtomicUsize, Arc};

use biscuit_auth::{macros::biscuit, KeyPair};
use clap_serde_derive::ClapSerde;
use fiber_json_types::RegisterLspTenantResult;
use fiber_lsp_sdk::{HostedSession, RootKey, RootSigner};
use jsonrpsee::{core::client::ClientT, rpc_params};
use ractor::Actor;

use crate::{
    fiber_types::Privkey,
    lsp::{BiscuitTokenIssuer, LspConfig, LspService, LspServiceArgs, TenantRegistry},
    rpc::{biscuit::BiscuitAuth, server::start_rpc},
    tests::{gen_rpc_config, get_test_root_actor},
};

use super::{
    fresh_registration_request_id, lsp_config, open_lsp_store, FakeRuntimeFactory, NoopNetworkActor,
};

#[test]
fn token_ttl_defaults_and_configuration_validation() {
    let mut config = LspConfig::from(<LspConfig as ClapSerde>::Opt::default());
    assert_eq!(config.token_ttl_secs, 30 * 24 * 60 * 60);
    config.validate().unwrap();
    for invalid in [0, u64::MAX, u64::MAX / 1_000] {
        config.token_ttl_secs = invalid;
        assert!(config.validate().is_err());
    }
    config.token_ttl_secs = 1;
    config.validate().unwrap();
    let opt =
        serde_yaml::from_str::<<LspConfig as ClapSerde>::Opt>("token_ttl_secs: 7200").unwrap();
    assert_eq!(LspConfig::from(opt).token_ttl_secs, 7_200);
}

#[tokio::test]
async fn provider_reissues_expiring_tokens_through_existing_registration_rpc() {
    crate::tests::init_tracing();
    let root = tempfile::tempdir().unwrap();
    let mut config = lsp_config(root.path().join("lsp"));
    config.token_ttl_secs = 2;
    let store = open_lsp_store(&config);
    let registry = TenantRegistry::new(store.clone());
    let biscuit_root = KeyPair::new();
    let auth = BiscuitAuth::from_pubkey(biscuit_root.public().to_string()).unwrap();
    let issuer = BiscuitTokenIssuer::from_private_key(
        &biscuit_root.private().to_prefixed_string(),
        &biscuit_root.public().to_string(),
    )
    .unwrap();
    let lsp_key = Privkey::from(&[9; 32]);
    let noop = Actor::spawn(None, NoopNetworkActor, ()).await.unwrap().0;
    let service = Actor::spawn(
        None,
        LspService,
        LspServiceArgs {
            config,
            public_node_id: lsp_key.pubkey(),
            public_network_actor: noop.clone(),
            store: store.clone(),
            watchtower_store: store.clone(),
            runtime_factory: Arc::new(FakeRuntimeFactory::new(Arc::new(AtomicUsize::new(0)))),
            signing_key: lsp_key.clone(),
            token_issuer: issuer,
            signing_events: Default::default(),
        },
    )
    .await
    .unwrap()
    .0;
    let supervisor = get_test_root_actor().await;
    let mut rpc_config = gen_rpc_config();
    rpc_config.biscuit_public_key = Some(biscuit_root.public().to_string());
    rpc_config.enabled_modules = vec!["lsp".into()];
    let server = start_rpc(
        rpc_config,
        None,
        None,
        None,
        None,
        Some(service.clone()),
        store,
        None,
        None,
        supervisor.get_cell(),
        None,
        Default::default(),
        #[cfg(debug_assertions)]
        None,
        #[cfg(debug_assertions)]
        None,
    )
    .await
    .unwrap();
    let operator_token = biscuit!(r#"read("lsp"); write("lsp");"#)
        .build(&biscuit_root)
        .unwrap()
        .to_base64()
        .unwrap();
    let operator = super::integration::authenticated_client(server.http_address, &operator_token);
    let anonymous = jsonrpsee::http_client::HttpClientBuilder::default()
        .build(format!("http://{}", server.http_address))
        .unwrap();
    let root_signer = RootSigner::create(
        RootKey::import([42; 32]).unwrap(),
        fiber_lsp_sdk::MemoryStore::default(),
    )
    .await
    .unwrap();
    let mut session = HostedSession::new(root_signer);
    session.allocate_pending_channel().await.unwrap();
    let proof = |session: &HostedSession<_>| {
        session
            .begin_registration(
                lsp_key.pubkey(),
                fresh_registration_request_id().into(),
                crate::now_timestamp_as_millis_u64(),
            )
            .unwrap()
    };

    // The client creates the proof, while only the hosting provider can call registration.
    let request = proof(&session);
    assert!(anonymous
        .request::<RegisterLspTenantResult, _>("lsp_register_tenant", rpc_params![request.clone()])
        .await
        .is_err());
    assert_eq!(registry.count().unwrap(), 0);
    let before_issued_ms = crate::now_timestamp_as_millis_u64() / 1_000 * 1_000;
    let initial: RegisterLspTenantResult = operator
        .request("lsp_register_tenant", rpc_params![request])
        .await
        .unwrap();
    assert!((before_issued_ms + 2_000
        ..=crate::now_timestamp_as_millis_u64() / 1_000 * 1_000 + 2_000)
        .contains(&initial.expires_at_ms));
    let record = registry.get(&session.tenant_id()).unwrap().unwrap();
    let old_token = initial.access_token.clone();
    for (time, valid) in [
        (initial.expires_at_ms - 1, true),
        (initial.expires_at_ms, false),
        (initial.expires_at_ms + 1, false),
    ] {
        assert_eq!(
            auth.check_permission_with_time("list_channels", &old_token, time)
                .is_ok(),
            valid
        );
    }
    // A holder cannot bypass the authority's expiry by appending its own clock fact.
    let forged = auth
        .extract_biscuit(&old_token)
        .unwrap()
        .append(
            biscuit_auth::BlockBuilder::new()
                .code("time(1970-01-01T00:00:00Z);")
                .unwrap(),
        )
        .unwrap()
        .to_base64()
        .unwrap();
    assert!(auth
        .check_permission_with_time("list_channels", &forged, initial.expires_at_ms)
        .is_err());
    session.finish_registration(initial).unwrap();
    let expires = session.state().token_expires_at_ms.unwrap();
    assert!(!session.is_token_near_expiry(expires - 201, 200));
    assert!(session.is_token_near_expiry(expires - 200, 200));
    // An expired client token does not affect the provider's operator credential.
    tokio::time::sleep(std::time::Duration::from_millis(
        expires.saturating_sub(crate::now_timestamp_as_millis_u64()) + 10,
    ))
    .await;
    assert!(session.is_token_near_expiry(crate::now_timestamp_as_millis_u64(), 200));
    assert!(auth.check_permission("list_channels", &old_token).is_err());
    let expired_client = super::integration::authenticated_client(server.http_address, &old_token);
    assert!(expired_client
        .request::<RegisterLspTenantResult, _>("lsp_register_tenant", rpc_params![proof(&session)])
        .await
        .is_err());

    let mut bad = proof(&session);
    bad.signature = "00".into();
    assert!(operator
        .request::<RegisterLspTenantResult, _>("lsp_register_tenant", rpc_params![bad])
        .await
        .is_err());
    let other_lsp = Privkey::from(&[8; 32]).pubkey();
    let bad = session
        .begin_registration(
            other_lsp,
            fresh_registration_request_id().into(),
            crate::now_timestamp_as_millis_u64(),
        )
        .unwrap();
    assert!(operator
        .request::<RegisterLspTenantResult, _>("lsp_register_tenant", rpc_params![bad])
        .await
        .is_err());
    let bad = session
        .begin_registration(
            lsp_key.pubkey(),
            fresh_registration_request_id().into(),
            crate::now_timestamp_as_millis_u64() - 600_000,
        )
        .unwrap();
    assert!(operator
        .request::<RegisterLspTenantResult, _>("lsp_register_tenant", rpc_params![bad])
        .await
        .is_err());

    let before = session.state().clone();
    let renewed: RegisterLspTenantResult = operator
        .request("lsp_register_tenant", rpc_params![proof(&session)])
        .await
        .unwrap();
    session
        .replace_token(renewed.access_token, renewed.expires_at_ms)
        .unwrap();
    assert_ne!(session.tenant_token(), before.tenant_token.as_deref());
    auth.check_permission("list_channels", session.tenant_token().unwrap())
        .unwrap();
    assert_eq!(session.state().pending, before.pending);
    assert_eq!(registry.get(&session.tenant_id()).unwrap().unwrap(), record);
    assert_eq!(registry.count().unwrap(), 1);
    // Valid tenant credentials still confer no operator rights.
    let tenant_client = super::integration::authenticated_client(
        server.http_address,
        session.tenant_token().unwrap(),
    );
    assert!(tenant_client
        .request::<serde_json::Value, _>("lsp_list_tenants", rpc_params![])
        .await
        .is_err());
    assert!(tenant_client
        .request::<RegisterLspTenantResult, _>("lsp_register_tenant", rpc_params![proof(&session)])
        .await
        .is_err());
    let replay = proof(&session);
    let _: RegisterLspTenantResult = operator
        .request("lsp_register_tenant", rpc_params![replay.clone()])
        .await
        .unwrap();
    let error = operator
        .request::<RegisterLspTenantResult, _>("lsp_register_tenant", rpc_params![replay])
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("already consumed"), "{error}");
    server.handle.stop().unwrap();
    server.handle.stopped().await;
    service.stop(None);
    noop.stop(None);
}
