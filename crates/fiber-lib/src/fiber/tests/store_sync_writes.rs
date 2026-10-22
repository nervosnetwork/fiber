use clap_serde_derive::ClapSerde;

use crate::tests::test_utils::generate_store;
use crate::FiberConfig;

#[test]
fn test_store_sync_writes_is_on_by_default() {
    assert!(FiberConfig::default().store_sync_writes());
}

#[test]
fn test_store_sync_writes_is_read_from_the_config_file() {
    let off: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str("store_sync_writes: false").expect("parse fiber config");
    assert!(!FiberConfig::from(off).store_sync_writes());

    let unset: <FiberConfig as ClapSerde>::Opt =
        serde_yaml::from_str("sync_network_graph: false").expect("parse fiber config");
    assert!(FiberConfig::from(unset).store_sync_writes());
}

#[test]
fn test_store_sync_writes_reaches_the_backend() {
    let (mut store, _dir) = generate_store();
    assert!(!store.sync_writes());
    store.set_sync_writes(true);
    assert!(store.sync_writes());
    assert!(store.clone().sync_writes());
}
