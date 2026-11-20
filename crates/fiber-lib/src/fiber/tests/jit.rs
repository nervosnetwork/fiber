use clap_serde_derive::ClapSerde;

use crate::fiber::types::Privkey;
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
