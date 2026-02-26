use crate::ckb::config::{UdtArgInfo, UdtCellDep, UdtCfgInfos, UdtCfgInfosExt, UdtDep, UdtScript};
use crate::fiber::gen::fiber::UdtCfgInfos as MoleculeUdtCfgInfos;
use ckb_jsonrpc_types::OutPoint;
use ckb_types::core::{DepType, ScriptHashType};
use ckb_types::packed::Script;
use ckb_types::prelude::{Builder, Pack};
use ckb_types::H256;
use hex;
use molecule::prelude::Entity;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_udt_whitelist() {
    let udt_whitelist = UdtCfgInfos(vec![UdtArgInfo {
        name: "SimpleUDT".to_string(),
        script: UdtScript {
            code_hash: H256::from([0u8; 32]),
            hash_type: ScriptHashType::Data,
            args: "0x00".to_string(),
        },
        auto_accept_amount: Some(100),
        cell_deps: vec![UdtDep::with_cell_dep(UdtCellDep {
            dep_type: DepType::Code,
            out_point: OutPoint {
                tx_hash: H256::from([0u8; 32]),
                index: 0.into(),
            },
        })],
    }]);

    let serialized = MoleculeUdtCfgInfos::from(udt_whitelist.clone()).as_bytes();
    let deserialized =
        UdtCfgInfos::try_from(MoleculeUdtCfgInfos::from_slice(&serialized).expect("invalid mol"))
            .expect("valid UdtCfgInfos");
    assert_eq!(udt_whitelist, deserialized);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_find_matching_udt_exact_match() {
    let code_hash = H256::from([1u8; 32]);
    let args = "0x1234".to_string();
    let udt_whitelist = UdtCfgInfos(vec![UdtArgInfo {
        name: "TestUDT".to_string(),
        script: UdtScript {
            code_hash: code_hash.clone(),
            hash_type: ScriptHashType::Data,
            args: args.clone(),
        },
        auto_accept_amount: Some(100),
        cell_deps: vec![],
    }]);

    let args_bytes = hex::decode(&args[2..]).unwrap();
    let script = Script::new_builder()
        .code_hash(code_hash.pack())
        .hash_type(ScriptHashType::Data.into())
        .args(args_bytes.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script);
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "TestUDT");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_find_matching_udt_regex_pattern() {
    let code_hash = H256::from([2u8; 32]);
    let udt_whitelist = UdtCfgInfos(vec![UdtArgInfo {
        name: "RegexUDT".to_string(),
        script: UdtScript {
            code_hash: code_hash.clone(),
            hash_type: ScriptHashType::Data,
            args: "0x[0-9a-f]{4}".to_string(), // Regex pattern matching 4 hex digits
        },
        auto_accept_amount: Some(200),
        cell_deps: vec![],
    }]);

    // Test with matching args
    let args_bytes = hex::decode("abcd").unwrap();
    let script = Script::new_builder()
        .code_hash(code_hash.pack())
        .hash_type(ScriptHashType::Data.into())
        .args(args_bytes.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script);
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "RegexUDT");

    // Test with non-matching args
    let args_bytes_no_match = hex::decode("ab").unwrap(); // Only 2 hex digits (1 byte), doesn't match pattern
    let script_no_match = Script::new_builder()
        .code_hash(code_hash.pack())
        .hash_type(ScriptHashType::Data.into())
        .args(args_bytes_no_match.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script_no_match);
    assert!(found.is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_find_matching_udt_wrong_code_hash() {
    let code_hash1 = H256::from([3u8; 32]);
    let code_hash2 = H256::from([4u8; 32]);
    let udt_whitelist = UdtCfgInfos(vec![UdtArgInfo {
        name: "TestUDT".to_string(),
        script: UdtScript {
            code_hash: code_hash1.clone(),
            hash_type: ScriptHashType::Data,
            args: "0x00".to_string(),
        },
        auto_accept_amount: Some(100),
        cell_deps: vec![],
    }]);

    let args_bytes = hex::decode("00").unwrap();
    let script = Script::new_builder()
        .code_hash(code_hash2.pack()) // Different code_hash
        .hash_type(ScriptHashType::Data.into())
        .args(args_bytes.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script);
    assert!(found.is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_find_matching_udt_wrong_hash_type() {
    let code_hash = H256::from([5u8; 32]);
    let udt_whitelist = UdtCfgInfos(vec![UdtArgInfo {
        name: "TestUDT".to_string(),
        script: UdtScript {
            code_hash: code_hash.clone(),
            hash_type: ScriptHashType::Data,
            args: "0x00".to_string(),
        },
        auto_accept_amount: Some(100),
        cell_deps: vec![],
    }]);

    let args_bytes = hex::decode("00").unwrap();
    let script = Script::new_builder()
        .code_hash(code_hash.pack())
        .hash_type(ScriptHashType::Type.into()) // Different hash_type
        .args(args_bytes.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script);
    assert!(found.is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_find_matching_udt_multiple_udts() {
    let code_hash1 = H256::from([6u8; 32]);
    let code_hash2 = H256::from([7u8; 32]);
    let udt_whitelist = UdtCfgInfos(vec![
        UdtArgInfo {
            name: "FirstUDT".to_string(),
            script: UdtScript {
                code_hash: code_hash1.clone(),
                hash_type: ScriptHashType::Data,
                args: "0x00".to_string(),
            },
            auto_accept_amount: Some(100),
            cell_deps: vec![],
        },
        UdtArgInfo {
            name: "SecondUDT".to_string(),
            script: UdtScript {
                code_hash: code_hash2.clone(),
                hash_type: ScriptHashType::Data,
                args: "0x01".to_string(),
            },
            auto_accept_amount: Some(200),
            cell_deps: vec![],
        },
    ]);

    // Test finding first UDT
    let args_bytes1 = hex::decode("00").unwrap();
    let script1 = Script::new_builder()
        .code_hash(code_hash1.pack())
        .hash_type(ScriptHashType::Data.into())
        .args(args_bytes1.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script1);
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "FirstUDT");

    // Test finding second UDT
    let args_bytes2 = hex::decode("01").unwrap();
    let script2 = Script::new_builder()
        .code_hash(code_hash2.pack())
        .hash_type(ScriptHashType::Data.into())
        .args(args_bytes2.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script2);
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "SecondUDT");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_find_matching_udt_empty_whitelist() {
    let udt_whitelist = UdtCfgInfos(vec![]);
    let code_hash = H256::from([8u8; 32]);

    let args_bytes = hex::decode("00").unwrap();
    let script = Script::new_builder()
        .code_hash(code_hash.pack())
        .hash_type(ScriptHashType::Data.into())
        .args(args_bytes.pack())
        .build();

    let found = udt_whitelist.find_matching_udt(&script);
    assert!(found.is_none());
}
