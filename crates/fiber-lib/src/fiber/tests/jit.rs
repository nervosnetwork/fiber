use clap_serde_derive::ClapSerde;

use crate::fiber::network::PeerDisconnectReason;
use crate::fiber::types::Privkey;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::test_utils::{NetworkNode, NetworkNodeConfigBuilder};
use crate::FiberConfig;

fn sample_pubkey_hex() -> (String, crate::fiber::types::Pubkey) {
    let pubkey = Privkey::from_slice(&[7u8; 32]).pubkey();
    (hex::encode(pubkey.serialize()), pubkey)
}

#[test]
fn test_jit_config_defaults_do_not_advertise() {
    let config = FiberConfig::default();
    assert!(config.jit_lsp_pubkey().is_none());
    assert!(!config.jit_service_enabled());
    assert_eq!(config.jit_open_fee(), 0);
    assert_eq!(config.jit_min_amount(), 0);
    assert_eq!(config.jit_max_funding(), 0);
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
"#;
    let opt: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str(yaml).expect("parse fiber config");
    let config = FiberConfig::from(opt);
    assert!(config.jit_service_enabled());
    assert_eq!(config.jit_open_fee(), 1000);
    assert_eq!(config.jit_min_amount(), 50);
    assert_eq!(config.jit_max_funding(), 25_000_000_000);
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
            })
            .build(),
    )
    .await
}

async fn start_jit_receiver(lsp_pubkey: crate::fiber::types::Pubkey) -> NetworkNode {
    let hex_pk = hex::encode(lsp_pubkey.serialize());
    NetworkNode::new_with_config(
        NetworkNodeConfigBuilder::new()
            .node_name(Some("jit-receiver".to_string()))
            .fiber_config_updater(move |config| {
                config.jit_lsp_pubkey = Some(hex_pk);
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
    receiver.expect_debug_event("JitChannelRegisterSent").await;
    lsp.expect_debug_event("JitChannelRegistered").await;
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

    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    receiver.expect_debug_event("JitChannelRegisterSent").await;
    lsp.expect_debug_event("JitChannelRegisterRejected").await;
}

#[tokio::test]
async fn test_jit_unregister_on_disconnect() {
    let mut lsp = start_jit_lsp().await;
    let mut receiver = start_jit_receiver(lsp.pubkey).await;
    receiver.connect_to(&mut lsp).await;
    receiver.expect_debug_event("JitChannelRegisterSent").await;
    lsp.expect_debug_event("JitChannelRegistered").await;

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
