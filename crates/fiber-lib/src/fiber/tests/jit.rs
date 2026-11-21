use clap_serde_derive::ClapSerde;
use ractor::call;
use secp256k1::{PublicKey, Secp256k1, SecretKey, SECP256K1};

use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::network::{OpenChannelCommand, PeerDisconnectReason};
use crate::fiber::payment::{SendPaymentCommand, SendPaymentDataExt};
use crate::fiber::types::Privkey;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::gen_rand_sha256_hash;
use crate::invoice::{CkbInvoice, Currency, InvoiceBuilder};
use crate::rpc::invoice::{InvoiceRpcServerImpl, NewInvoiceParams};
use crate::test_utils::{
    establish_channel_between_nodes, init_tracing, wait_for_network_graph_update,
    wait_until_node_supports_trampoline_routing, ChannelParameters, NetworkNode,
    NetworkNodeConfigBuilder, HUGE_CKB_AMOUNT, MIN_RESERVED_CKB,
};
use crate::FiberConfig;
use crate::NetworkServiceEvent;
use ckb_types::packed::OutPoint;
use fiber_types::HopHint;
use fiber_types::{ChannelState, Hash256, SendPaymentData};

fn sample_pubkey_hex() -> (String, crate::fiber::types::Pubkey) {
    let pubkey = Privkey::from_slice(&[7u8; 32]).pubkey();
    (hex::encode(pubkey.serialize()), pubkey)
}

#[test]
fn test_jit_config_defaults_do_not_advertise() {
    let config = FiberConfig::default();
    assert!(config.jit_lsp_pubkey().is_none());
    assert!(!config.jit_service_enabled());
    assert!(config.jit_open_fee().is_none());
    assert!(!config.jit_channel_config().open_fee_required());
    assert_eq!(config.jit_min_amount(), 0);
    assert_eq!(config.jit_max_funding(), 0);
    assert!(config.jit_max_amount().is_none());
    let funded = FiberConfig {
        jit_service_enabled: Some(true),
        jit_max_funding: Some(1),
        ..FiberConfig::default()
    };
    assert!(funded.jit_channel_config().open_fee_required());
    let priced = FiberConfig {
        jit_open_fee: Some(0),
        ..funded
    };
    assert!(!priced.jit_channel_config().open_fee_required());
    assert!(!config.jit_channel_advertised());
    assert!(!config.gen_node_features().supports_jit_channel());
}

#[test]
fn test_jit_config_receiver_pubkey_advertises_feature() {
    let (hex_pk, pubkey) = sample_pubkey_hex();
    let opt: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str(&format!("jit_lsp_pubkey: {hex_pk}")).expect("parse fiber config");
    let config = FiberConfig::from(opt);
    assert_eq!(config.jit_lsp_pubkey(), Some(pubkey));
    assert!(config.jit_channel_advertised());
    let features = config.gen_node_features();
    assert!(features.supports_jit_channel());
    assert!(!features.requires_jit_channel());
}

#[test]
fn test_jit_config_service_yaml() {
    let yaml = r#"
jit_service_enabled: true
jit_open_fee: 1000
jit_min_amount: 50
jit_max_funding: 25000000000
jit_max_amount: 500
"#;
    let opt: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str(yaml).expect("parse fiber config");
    let config = FiberConfig::from(opt);
    assert!(config.jit_service_enabled());
    assert_eq!(config.jit_open_fee(), Some(1000));
    assert_eq!(config.jit_min_amount(), 50);
    assert_eq!(config.jit_max_funding(), 25_000_000_000);
    assert_eq!(config.jit_max_amount(), Some(500));
    assert!(config.jit_channel_advertised());
    assert!(config.gen_node_features().supports_jit_channel());
}

#[test]
fn test_jit_config_pubkey_accepts_0x_prefix() {
    let (hex_pk, pubkey) = sample_pubkey_hex();
    let opt: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str(&format!("jit_lsp_pubkey: 0x{hex_pk}")).expect("parse fiber config");
    let config = FiberConfig::from(opt);
    assert_eq!(config.jit_lsp_pubkey(), Some(pubkey));
}

async fn start_jit_lsp() -> NetworkNode {
    NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-lsp".to_string()))
            .fiber_config_updater(|config| {
                config.jit_service_enabled = Some(true);
                config.jit_max_funding = Some(HUGE_CKB_AMOUNT);
                config.jit_open_fee = Some(0);
            })
            .build(),
    )
    .await
}

async fn start_jit_receiver(lsp_pubkey: crate::fiber::types::Pubkey) -> NetworkNode {
    start_jit_receiver_with_cap(lsp_pubkey, None).await
}

async fn wait_jit_registration(receiver: &mut NetworkNode, lsp: &mut NetworkNode) {
    receiver.expect_debug_event("JitChannelRegisterSent").await;
    lsp.expect_debug_event("JitChannelRegistered").await;
    receiver
        .expect_debug_event("JitChannelRegisterAccepted")
        .await;
}

async fn start_jit_receiver_with_cap(
    lsp_pubkey: crate::fiber::types::Pubkey,
    max_amount: Option<u128>,
) -> NetworkNode {
    let hex_pk = hex::encode(lsp_pubkey.serialize());
    NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-receiver".to_string()))
            .fiber_config_updater(move |config| {
                config.jit_lsp_pubkey = Some(hex_pk);
                config.jit_max_amount = max_amount;
            })
            .build(),
    )
    .await
}

#[tokio::test]
async fn test_jit_register_on_connect() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;
}

#[tokio::test]
async fn test_jit_register_rejected_when_service_disabled() {
    let mut lsp = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-lsp-disabled".to_string()))
            .build(),
    )
    .await;
    let mut features = lsp.node_info().await.features;
    features.set_jit_channel_optional();
    lsp.update_node_features_and_wait(features).await;

    let hex_pk = hex::encode(lsp.pubkey.serialize());
    let mut receiver = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-receiver".to_string()))
            .fiber_config_updater(move |config| {
                config.jit_lsp_pubkey = Some(hex_pk);
                // Valid opens also meet the normal auto-accept minimum.
                config.auto_accept_channel_ckb_funding_amount = Some(0);
            })
            .build(),
    )
    .await;
    receiver.connect_to(&mut lsp).await;
    receiver.expect_debug_event("JitChannelRegisterSent").await;
    lsp.expect_debug_event("JitChannelRegisterRejected").await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                pubkey: receiver.pubkey,
                public: false,
                one_way: false,
                shutdown_script: None,
                funding_amount: MIN_RESERVED_CKB + 10_000_000_000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    call!(lsp.network_actor, message)
        .expect("lsp alive")
        .expect("open channel");
    receiver
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::ChannelPendingToBeAccepted(pubkey, _)
                    if *pubkey == lsp.pubkey
            )
        })
        .await;
}

#[tokio::test]
async fn test_jit_unregister_on_disconnect() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    lsp.network_actor
        .send_message(NetworkActorMessage::new_command(
            NetworkActorCommand::DisconnectPeer(
                receiver.pubkey,
                PeerDisconnectReason::Requested,
                None,
            ),
        ))
        .expect("lsp alive");
    lsp.expect_debug_event("JitChannelUnregistered").await;
}

#[tokio::test]
async fn test_jit_new_invoice_embeds_liquidity_provider() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;
    let rpc = InvoiceRpcServerImpl::new(
        receiver.store.clone(),
        Some(receiver.network_actor.clone()),
        Some(receiver.fiber_config.clone()),
    );
    let result = rpc
        .new_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("jit".to_string()),
            currency: fiber_json_types::Currency::Fibd,
            payment_preimage: Some(gen_rand_sha256_hash().into()),
            payment_hash: None,
            expiry: None,
            fallback_address: None,
            final_expiry_delta: None,
            udt_type_script: None,
            hash_algorithm: None,
            allow_mpp: None,
            allow_trampoline_routing: None,
        })
        .await
        .expect("new invoice");

    let invoice: CkbInvoice = result.invoice_address.parse().unwrap();
    assert_eq!(
        invoice.liquidity_provider().copied().map(Into::into),
        Some(lsp.pubkey)
    );
    assert_eq!(invoice.jit_open_fee(), Some(0));
    assert_eq!(
        invoice.hop_hints(),
        vec![HopHint {
            pubkey: lsp.pubkey,
            channel_outpoint: OutPoint::default(),
            fee_rate: 0,
            tlc_expiry_delta: 0,
        }]
    );
    assert!(invoice.allow_trampoline_routing());

    let opted_out = rpc
        .new_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("direct".to_string()),
            currency: fiber_json_types::Currency::Fibd,
            payment_preimage: Some(gen_rand_sha256_hash().into()),
            payment_hash: None,
            expiry: None,
            fallback_address: None,
            final_expiry_delta: None,
            udt_type_script: None,
            hash_algorithm: None,
            allow_mpp: None,
            allow_trampoline_routing: Some(false),
        })
        .await
        .expect("direct invoice");
    let opted_out: CkbInvoice = opted_out.invoice_address.parse().unwrap();
    assert!(opted_out.liquidity_provider().is_none());
    assert!(opted_out.jit_open_fee().is_none());
    assert!(opted_out.hop_hints().is_empty());
    assert!(!opted_out.allow_trampoline_routing());

    let mpp_rejected = rpc
        .new_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("mpp".to_string()),
            currency: fiber_json_types::Currency::Fibd,
            payment_preimage: Some(gen_rand_sha256_hash().into()),
            payment_hash: None,
            expiry: None,
            fallback_address: None,
            final_expiry_delta: None,
            udt_type_script: None,
            hash_algorithm: None,
            allow_mpp: Some(true),
            allow_trampoline_routing: None,
        })
        .await;
    assert!(mpp_rejected.is_err());
}

fn signed_jit_invoice(payee_sk: &SecretKey, payee_pk: PublicKey, lsp_pk: PublicKey) -> CkbInvoice {
    InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(gen_rand_sha256_hash())
        .payee_pub_key(payee_pk)
        .liquidity_provider(lsp_pk)
        .allow_trampoline_routing(true)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, payee_sk))
        .expect("build jit invoice")
}

#[test]
fn test_send_payment_uses_invoice_liquidity_provider_as_trampoline_hop() {
    let (payee_sk, payee_pk) = crate::gen_rand_secp256k1_keypair_tuple();
    let lsp_pk: PublicKey = crate::gen_rand_fiber_public_key().into();
    let other_hop: PublicKey = crate::gen_rand_fiber_public_key().into();
    let invoice = signed_jit_invoice(&payee_sk, payee_pk, lsp_pk);

    let payment = SendPaymentData::new(
        SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            ..Default::default()
        },
        Currency::Fibd,
    )
    .expect("build payment");
    assert_eq!(
        payment.trampoline_hops.as_deref(),
        Some([lsp_pk.into()].as_slice())
    );

    let payment = SendPaymentData::new(
        SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            trampoline_hops: Some(vec![other_hop.into()]),
            ..Default::default()
        },
        Currency::Fibd,
    )
    .expect("build payment with explicit hops");
    assert_eq!(
        payment.trampoline_hops.as_deref(),
        Some([other_hop.into()].as_slice())
    );
}

#[test]
fn test_send_payment_uses_jit_hop_hint_without_liquidity_provider() {
    let (payee_sk, payee_pk) = crate::gen_rand_secp256k1_keypair_tuple();
    let lsp_pk: PublicKey = crate::gen_rand_fiber_public_key().into();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(gen_rand_sha256_hash())
        .payee_pub_key(payee_pk)
        .hop_hints(vec![HopHint {
            pubkey: lsp_pk.into(),
            channel_outpoint: OutPoint::default(),
            fee_rate: 0,
            tlc_expiry_delta: 0,
        }])
        .allow_trampoline_routing(true)
        .jit_open_fee(50_000)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &payee_sk))
        .expect("build jit invoice");

    let payment = SendPaymentData::new(
        SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            ..Default::default()
        },
        Currency::Fibd,
    )
    .expect("build payment");
    assert_eq!(
        payment.trampoline_hops.as_deref(),
        Some([lsp_pk.into()].as_slice())
    );
    assert!(payment.hop_hints.is_empty());
    // Default routing allowance is 0.5% of (amount + open fee), added on top of the quote.
    assert_eq!(payment.max_fee_amount, Some(50_255));

    let capped = SendPaymentData::new(
        SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(60_000),
            ..Default::default()
        },
        Currency::Fibd,
    )
    .expect("cap above the quote");
    assert_eq!(capped.max_fee_amount, Some(50_255));

    let rejected = SendPaymentData::new(
        SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10),
            ..Default::default()
        },
        Currency::Fibd,
    );
    assert!(rejected
        .expect_err("fee below invoice")
        .contains("JIT open fee"));

    let rate_rejected = SendPaymentData::new(
        SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_rate: Some(1),
            ..Default::default()
        },
        Currency::Fibd,
    );
    assert!(rate_rejected
        .expect_err("rate below invoice")
        .contains("JIT open fee"));

    let direct = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(gen_rand_sha256_hash())
        .payee_pub_key(payee_pk)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &payee_sk))
        .expect("build direct invoice");
    let hinted = SendPaymentData::new(
        SendPaymentCommand {
            invoice: Some(direct.to_string()),
            hop_hints: Some(vec![HopHint {
                pubkey: lsp_pk.into(),
                channel_outpoint: OutPoint::default(),
                fee_rate: 0,
                tlc_expiry_delta: 0,
            }]),
            ..Default::default()
        },
        Currency::Fibd,
    );
    assert!(hinted
        .expect_err("sentinel hint on an invoice that disallows trampoline")
        .contains("hop hints"));
}

#[tokio::test]
async fn test_jit_invoice_quotes_lsp_open_fee() {
    let mut lsp = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-lsp-fee".to_string()))
            .fiber_config_updater(|config| {
                config.jit_service_enabled = Some(true);
                config.jit_max_funding = Some(HUGE_CKB_AMOUNT);
                config.jit_open_fee = Some(2_500);
            })
            .build(),
    )
    .await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let rpc = InvoiceRpcServerImpl::new(
        receiver.store.clone(),
        Some(receiver.network_actor.clone()),
        Some(receiver.fiber_config.clone()),
    );
    let result = rpc
        .new_invoice(NewInvoiceParams {
            amount: 1000,
            description: Some("jit fee".to_string()),
            currency: fiber_json_types::Currency::Fibd,
            payment_preimage: Some(gen_rand_sha256_hash().into()),
            payment_hash: None,
            expiry: None,
            fallback_address: None,
            final_expiry_delta: None,
            udt_type_script: None,
            hash_algorithm: None,
            allow_mpp: None,
            allow_trampoline_routing: None,
        })
        .await
        .expect("new invoice");
    let invoice: CkbInvoice = result.invoice_address.parse().unwrap();
    assert_eq!(invoice.jit_open_fee(), Some(2_500));
    assert_eq!(invoice.hop_hints()[0].pubkey, lsp.pubkey);
}

#[tokio::test]
async fn test_jit_funded_service_without_open_fee_rejects_registration() {
    let mut lsp = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-lsp-no-fee".to_string()))
            .fiber_config_updater(|config| {
                config.jit_service_enabled = Some(true);
                config.jit_max_funding = Some(HUGE_CKB_AMOUNT);
            })
            .build(),
    )
    .await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    receiver.expect_debug_event("JitChannelRegisterSent").await;
    lsp.expect_debug_event("JitChannelRegisterRejected").await;
}

#[tokio::test]
async fn test_jit_receiver_auto_accepts_reserve_only_channel_from_lsp() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                pubkey: receiver.pubkey,
                public: false,
                one_way: false,
                shutdown_script: None,
                funding_amount: MIN_RESERVED_CKB + 10_000_000_000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    call!(lsp.network_actor, message)
        .expect("lsp alive")
        .expect("open channel");

    let channel_id = receiver
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(pubkey, channel_id) if *pubkey == lsp.pubkey => {
                Some(*channel_id)
            }
            _ => None,
        })
        .await;
    assert_ne!(
        receiver.get_channel_actor_state(channel_id).state,
        ChannelState::ChannelReady
    );

    receiver
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::ChannelReady(pubkey, id, _)
                    if *pubkey == lsp.pubkey && *id == channel_id
            )
        })
        .await;
    lsp.expect_event(|event| {
        matches!(
            event,
            NetworkServiceEvent::ChannelReady(pubkey, id, _)
                if *pubkey == receiver.pubkey && *id == channel_id
        )
    })
    .await;

    let receiver_state = receiver.get_channel_actor_state(channel_id);
    assert_eq!(receiver_state.state, ChannelState::ChannelReady);
    assert!(!receiver_state.is_public());
    assert!(!receiver_state.is_one_way);
    assert_eq!(receiver.get_local_balance_from_channel(channel_id), 0);
}

#[tokio::test]
async fn test_jit_receiver_rejects_one_way_open_from_lsp() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                pubkey: receiver.pubkey,
                public: false,
                one_way: true,
                shutdown_script: None,
                funding_amount: MIN_RESERVED_CKB + 10_000_000_000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    call!(lsp.network_actor, message)
        .expect("lsp alive")
        .expect("open channel");
    receiver.expect_debug_event("ChannelAutoAcceptFailed").await;
}

#[tokio::test]
async fn test_jit_receiver_does_not_auto_accept_open_from_other_peer() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let mut other = NetworkNode::new_with_node_name("other").await;
    other.connect_to(&mut receiver).await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                pubkey: receiver.pubkey,
                public: false,
                one_way: false,
                shutdown_script: None,
                funding_amount: MIN_RESERVED_CKB + 10_000_000_000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    call!(other.network_actor, message)
        .expect("other alive")
        .expect("open channel");

    receiver
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::ChannelPendingToBeAccepted(pubkey, _)
                    if *pubkey == other.pubkey
            )
        })
        .await;
    receiver.expect_debug_event("ChannelAutoAcceptFailed").await;
}

#[tokio::test]
async fn test_jit_receiver_does_not_auto_accept_public_open_from_lsp() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                pubkey: receiver.pubkey,
                public: true,
                one_way: false,
                shutdown_script: None,
                funding_amount: MIN_RESERVED_CKB + 10_000_000_000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    call!(lsp.network_actor, message)
        .expect("lsp alive")
        .expect("open channel");

    receiver
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::ChannelPendingToBeAccepted(pubkey, _)
                    if *pubkey == lsp.pubkey
            )
        })
        .await;
    receiver.expect_debug_event("ChannelAutoAcceptFailed").await;
    assert!(channel_ids_with_peer(&receiver, lsp.pubkey).is_empty());
}

#[tokio::test]
async fn test_jit_receiver_rejects_open_above_max_amount() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver_with_cap(lsp.pubkey, Some(1_000)).await;
    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                pubkey: receiver.pubkey,
                public: false,
                one_way: false,
                shutdown_script: None,
                funding_amount: MIN_RESERVED_CKB + 10_000_000_000,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_number_in_flight: None,
                max_tlc_value_in_flight: None,
            },
            rpc_reply,
        ))
    };
    call!(lsp.network_actor, message)
        .expect("lsp alive")
        .expect("open channel");

    receiver
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::ChannelPendingToBeAccepted(pubkey, _)
                    if *pubkey == lsp.pubkey
            )
        })
        .await;
    receiver.expect_debug_event("ChannelAutoAcceptFailed").await;
    assert!(channel_ids_with_peer(&receiver, lsp.pubkey).is_empty());
}

async fn new_receiver_jit_invoice(receiver: &NetworkNode, amount: u128) -> CkbInvoice {
    let rpc = InvoiceRpcServerImpl::new(
        receiver.store.clone(),
        Some(receiver.network_actor.clone()),
        Some(receiver.fiber_config.clone()),
    );
    let result = rpc
        .new_invoice(NewInvoiceParams {
            amount,
            description: Some("jit".to_string()),
            currency: fiber_json_types::Currency::Fibd,
            payment_preimage: Some(gen_rand_sha256_hash().into()),
            payment_hash: None,
            expiry: None,
            fallback_address: None,
            final_expiry_delta: None,
            udt_type_script: None,
            hash_algorithm: None,
            allow_mpp: None,
            allow_trampoline_routing: None,
        })
        .await
        .expect("new invoice");
    result.invoice_address.parse().unwrap()
}

fn channel_ids_with_peer(node: &NetworkNode, peer: crate::fiber::types::Pubkey) -> Vec<Hash256> {
    node.store.get_channel_ids_by_pubkey(&peer)
}

async fn start_jit_payment_nodes() -> (NetworkNode, NetworkNode, NetworkNode) {
    let mut lsp = start_jit_lsp().await;
    let mut payer = NetworkNode::new_with_node_name("jit-payer").await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;

    payer.connect_to(&mut lsp).await;
    establish_channel_between_nodes(
        &mut payer,
        &mut lsp,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    wait_for_network_graph_update(&payer, 1).await;
    wait_until_node_supports_trampoline_routing(&payer, &lsp).await;

    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    (payer, lsp, receiver)
}

#[tokio::test]
async fn test_jit_payment_opens_private_channel_and_succeeds() {
    init_tracing();

    let (payer, mut lsp, mut receiver) = start_jit_payment_nodes().await;
    let amount = 1000;
    let invoice = new_receiver_jit_invoice(&receiver, amount).await;

    let res = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            ..Default::default()
        })
        .await
        .expect("send payment");
    let payment_hash = res.payment_hash;

    let channel_id = receiver
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelCreated(pubkey, channel_id) if *pubkey == lsp.pubkey => {
                Some(*channel_id)
            }
            _ => None,
        })
        .await;
    assert_ne!(
        receiver.get_channel_actor_state(channel_id).state,
        ChannelState::ChannelReady
    );

    receiver
        .expect_event(|event| {
            matches!(
                event,
                NetworkServiceEvent::ChannelReady(pubkey, id, _)
                    if *pubkey == lsp.pubkey && *id == channel_id
            )
        })
        .await;
    lsp.expect_event(|event| {
        matches!(
            event,
            NetworkServiceEvent::ChannelReady(pubkey, id, _)
                if *pubkey == receiver.pubkey && *id == channel_id
        )
    })
    .await;

    payer.wait_until_success(payment_hash).await;

    let receiver_state = receiver.get_channel_actor_state(channel_id);
    assert_eq!(receiver_state.state, ChannelState::ChannelReady);
    assert!(!receiver_state.is_public());
    assert!(!receiver_state.is_one_way);
    assert_eq!(receiver.get_local_balance_from_channel(channel_id), amount);
    assert_eq!(channel_ids_with_peer(&receiver, lsp.pubkey).len(), 1);
}

#[tokio::test]
async fn test_jit_second_payment_reuses_existing_channel() {
    init_tracing();

    let (payer, lsp, mut receiver) = start_jit_payment_nodes().await;
    let amount = 1000;
    let invoice = new_receiver_jit_invoice(&receiver, amount).await;

    let res = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            ..Default::default()
        })
        .await
        .expect("send payment");
    receiver
        .expect_event(|event| {
            matches!(event, NetworkServiceEvent::ChannelReady(pubkey, _, _) if *pubkey == lsp.pubkey)
        })
        .await;
    payer.wait_until_success(res.payment_hash).await;
    assert_eq!(channel_ids_with_peer(&receiver, lsp.pubkey).len(), 1);

    let spend = receiver
        .send_payment_keysend(&lsp, 500, false)
        .await
        .expect("spend received balance");
    receiver.wait_until_success(spend.payment_hash).await;
    assert_eq!(channel_ids_with_peer(&receiver, lsp.pubkey).len(), 1);
}

#[tokio::test]
async fn test_jit_unregistered_peer_does_not_open_channel() {
    init_tracing();

    let mut lsp = start_jit_lsp().await;
    let mut payer = NetworkNode::new_with_node_name("jit-payer").await;
    let mut receiver = NetworkNode::new_with_node_name("jit-unregistered").await;

    payer.connect_to(&mut lsp).await;
    establish_channel_between_nodes(
        &mut payer,
        &mut lsp,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    wait_for_network_graph_update(&payer, 1).await;
    wait_until_node_supports_trampoline_routing(&payer, &lsp).await;
    receiver.connect_to(&mut lsp).await;

    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000))
        .payment_preimage(preimage)
        .payee_pub_key(receiver.pubkey.into())
        .liquidity_provider(lsp.pubkey.into())
        .allow_trampoline_routing(true)
        .build_with_sign(|hash| SECP256K1.sign_ecdsa_recoverable(hash, &receiver.private_key.0))
        .expect("build invoice");
    receiver.insert_invoice(invoice.clone(), Some(preimage));

    let res = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            ..Default::default()
        })
        .await
        .expect("send payment");
    payer.wait_until_failed(res.payment_hash).await;
    assert!(channel_ids_with_peer(&receiver, lsp.pubkey).is_empty());
    assert!(channel_ids_with_peer(&lsp, receiver.pubkey).is_empty());
}

#[tokio::test]
async fn test_jit_insufficient_open_fee_does_not_open_channel() {
    init_tracing();

    let mut lsp = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-lsp".to_string()))
            .fiber_config_updater(|config| {
                config.jit_service_enabled = Some(true);
                config.jit_open_fee = Some(10_000);
                config.jit_max_funding = Some(HUGE_CKB_AMOUNT);
            })
            .build(),
    )
    .await;
    let mut payer = NetworkNode::new_with_node_name("jit-payer").await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;

    payer.connect_to(&mut lsp).await;
    establish_channel_between_nodes(
        &mut payer,
        &mut lsp,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    wait_for_network_graph_update(&payer, 1).await;
    wait_until_node_supports_trampoline_routing(&payer, &lsp).await;

    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let invoice = new_receiver_jit_invoice(&receiver, 1000).await;
    let err = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(1_000),
            ..Default::default()
        })
        .await
        .expect_err("fee below the invoice quote");
    assert!(
        err.contains("JIT open fee"),
        "unexpected payment error: {err}"
    );
    assert!(channel_ids_with_peer(&receiver, lsp.pubkey).is_empty());
    assert!(channel_ids_with_peer(&lsp, receiver.pubkey).is_empty());
}

#[tokio::test]
async fn test_jit_zero_max_funding_does_not_open_channel() {
    init_tracing();

    let mut lsp = NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-lsp-uncapped".to_string()))
            .fiber_config_updater(|config| {
                config.jit_service_enabled = Some(true);
            })
            .build(),
    )
    .await;
    let mut payer = NetworkNode::new_with_node_name("jit-payer").await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;

    payer.connect_to(&mut lsp).await;
    establish_channel_between_nodes(
        &mut payer,
        &mut lsp,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    wait_for_network_graph_update(&payer, 1).await;
    wait_until_node_supports_trampoline_routing(&payer, &lsp).await;

    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let invoice = new_receiver_jit_invoice(&receiver, 1000).await;
    let res = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            ..Default::default()
        })
        .await
        .expect("send payment");
    payer.wait_until_failed(res.payment_hash).await;
    assert!(channel_ids_with_peer(&receiver, lsp.pubkey).is_empty());
    assert!(channel_ids_with_peer(&lsp, receiver.pubkey).is_empty());
}

#[tokio::test]
async fn test_jit_second_inbound_payment_opens_another_channel() {
    init_tracing();

    let (payer, lsp, mut receiver) = start_jit_payment_nodes().await;
    let amount = 1000;
    let first = new_receiver_jit_invoice(&receiver, amount).await;
    let first_payment = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(first.to_string()),
            max_fee_amount: Some(10_000),
            ..Default::default()
        })
        .await
        .expect("first payment");
    receiver
        .expect_event(|event| {
            matches!(event, NetworkServiceEvent::ChannelReady(pubkey, _, _) if *pubkey == lsp.pubkey)
        })
        .await;
    payer.wait_until_success(first_payment.payment_hash).await;
    assert_eq!(channel_ids_with_peer(&receiver, lsp.pubkey).len(), 1);

    let second = new_receiver_jit_invoice(&receiver, amount).await;
    let second_payment = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(second.to_string()),
            max_fee_amount: Some(10_000),
            ..Default::default()
        })
        .await
        .expect("second payment");
    payer.wait_until_success(second_payment.payment_hash).await;
    let channel_ids = channel_ids_with_peer(&receiver, lsp.pubkey);
    assert_eq!(channel_ids.len(), 2);
    let received: u128 = channel_ids
        .iter()
        .map(|channel_id| receiver.get_local_balance_from_channel(*channel_id))
        .sum();
    assert_eq!(received, amount * 2);
}

#[tokio::test]
async fn test_jit_payment_above_receiver_max_does_not_open_channel() {
    init_tracing();

    let mut lsp = start_jit_lsp().await;
    let mut payer = NetworkNode::new_with_node_name("jit-payer").await;
    let mut receiver = start_jit_receiver_with_cap(lsp.pubkey, Some(100)).await;

    payer.connect_to(&mut lsp).await;
    establish_channel_between_nodes(
        &mut payer,
        &mut lsp,
        ChannelParameters {
            public: true,
            node_a_funding_amount: HUGE_CKB_AMOUNT,
            node_b_funding_amount: HUGE_CKB_AMOUNT,
            ..Default::default()
        },
    )
    .await;
    wait_for_network_graph_update(&payer, 1).await;
    wait_until_node_supports_trampoline_routing(&payer, &lsp).await;

    receiver.connect_to(&mut lsp).await;
    wait_jit_registration(&mut receiver, &mut lsp).await;

    let invoice = new_receiver_jit_invoice(&receiver, 1_000).await;
    let res = payer
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(10_000),
            ..Default::default()
        })
        .await
        .expect("send payment");
    payer.wait_until_failed(res.payment_hash).await;
    assert!(channel_ids_with_peer(&receiver, lsp.pubkey).is_empty());
    assert!(channel_ids_with_peer(&lsp, receiver.pubkey).is_empty());
}
