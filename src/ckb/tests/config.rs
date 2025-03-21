use crate::ckb::config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtDep, UdtScript};
use crate::fiber::gen::fiber::UdtCfgInfos as MoleculeUdtCfgInfos;
use ckb_types::core::{DepType, ScriptHashType};
use ckb_types::H256;
use molecule::prelude::Entity;

#[test]
fn test_udt_whitelist() {
    let udt_whitelist = UdtCfgInfos(vec![UdtArgInfo {
        name: "SimpleUDT".to_string(),
        script: UdtScript {
            code_hash: H256::from([0u8; 32]),
            hash_type: ScriptHashType::Data,
            args: "0x00".to_string(),
        },
        auto_accept_amount: Some(100),
        cell_deps: vec![UdtDep::CellDep(UdtCellDep {
            dep_type: DepType::Code,
            tx_hash: H256::from([0u8; 32]),
            index: 0,
        })],
    }]);

    let serialized = MoleculeUdtCfgInfos::from(udt_whitelist.clone()).as_bytes();
    let deserialized =
        UdtCfgInfos::from(MoleculeUdtCfgInfos::from_slice(&serialized).expect("invalid mol"));
    assert_eq!(udt_whitelist, deserialized);
}
