use ckb_sdk::{
    transaction::{
        builder::{sudt::SudtTransactionBuilder, CkbTransactionBuilder},
        handler::{sighash::Secp256k1Blake160SighashAllScriptHandler, sudt::SudtHandler},
        input::InputIterator,
        signer::{SignContexts, TransactionSigner},
        TransactionBuilderConfiguration,
    },
    Address, CkbRpcClient, NetworkInfo, ScriptId,
};
use ckb_types::{
    core::{DepType, ScriptHashType},
    h256,
    packed::{OutPoint, Script},
    prelude::{Entity, Pack},
    H256,
};
use ckb_types::{packed::CellDep, prelude::Builder};
use serde::{Deserialize, Serialize};

use serde_yaml;
use std::{error::Error as StdErr, str::FromStr};

const SIMPLE_CODE_HASH: H256 =
    h256!("0xe1e354d6d643ad42724d40967e334984534e0367405c5ae42a9d7d63d77df419");
const CKB_SHANNONS: u64 = 100_000_000;

fn get_env_hex(name: &str) -> H256 {
    let value = std::env::var(name).expect("env var");
    // strip prefix 0x
    let value = value.trim_start_matches("0x");
    H256::from_str(&value).expect("parse hex")
}

fn gen_dev_udt_handler() -> SudtHandler {
    let simple_udt_tx = get_env_hex("NEXT_PUBLIC_SIMPLE_UDT_TX_HASH");

    let (out_point, script_id) = (
        OutPoint::new_builder()
            .tx_hash(simple_udt_tx.pack())
            .index(0u32.pack())
            .build(),
        ScriptId::new_data1(SIMPLE_CODE_HASH.clone()),
    );

    let cell_dep = CellDep::new_builder()
        .out_point(out_point)
        .dep_type(DepType::Code.into())
        .build();

    let udt_handler = ckb_sdk::transaction::handler::sudt::SudtHandler::new_with_customize(
        vec![cell_dep],
        script_id,
    );
    return udt_handler;
}

fn gen_dev_sighash_handler() -> Secp256k1Blake160SighashAllScriptHandler {
    let sighash_tx = get_env_hex("NEXT_PUBLIC_CKB_GENESIS_TX_1");

    let out_point = OutPoint::new_builder()
        .tx_hash(sighash_tx.pack())
        .index(0u32.pack())
        .build();

    let cell_dep = CellDep::new_builder()
        .out_point(out_point)
        .dep_type(DepType::DepGroup.into())
        .build();

    let sighash_handler =
        Secp256k1Blake160SighashAllScriptHandler::new_with_customize(vec![cell_dep]);
    return sighash_handler;
}

fn generate_configuration(
) -> Result<(NetworkInfo, TransactionBuilderConfiguration), Box<dyn StdErr>> {
    let network_info = NetworkInfo::devnet();
    let mut configuration =
        TransactionBuilderConfiguration::new_devnet().expect("new devnet configuration");

    configuration.register_script_handler(Box::new(gen_dev_sighash_handler()));
    configuration.register_script_handler(Box::new(gen_dev_udt_handler()));
    return Ok((network_info, configuration));
}

fn init_or_send_udt(
    issuer_address: &str,
    sender_info: &(String, H256),
    receiver_address: Option<&str>,
    sudt_amount: u128,
    apply: bool,
) -> Result<(), Box<dyn StdErr>> {
    let (network_info, configuration) = generate_configuration()?;

    let issuer = Address::from_str(issuer_address)?;
    let sender = Address::from_str(&sender_info.0)?;
    let receiver = if let Some(addr) = receiver_address {
        Address::from_str(addr)?
    } else {
        sender.clone()
    };

    let iterator = InputIterator::new_with_address(&[sender.clone()], &network_info);
    let owner_mode = receiver_address.is_none();
    let mut builder = SudtTransactionBuilder::new(configuration, iterator, &issuer, owner_mode)?;
    builder.add_output(&receiver, sudt_amount);

    let mut tx_with_groups = builder.build(&Default::default())?;

    let private_keys = vec![sender_info.1.clone()];

    TransactionSigner::new(&network_info).sign_transaction(
        &mut tx_with_groups,
        &SignContexts::new_sighash_h256(private_keys)?,
    )?;

    let json_tx = ckb_jsonrpc_types::TransactionView::from(tx_with_groups.get_tx_view().clone());
    println!("tx: {}", serde_json::to_string_pretty(&json_tx).unwrap());

    if apply {
        let tx_hash = CkbRpcClient::new(network_info.url.as_str())
            .send_transaction(json_tx.inner, None)
            .expect("send transaction");
        println!(">>> tx {} sent! <<<", tx_hash);
    } else {
        let result = CkbRpcClient::new(network_info.url.as_str())
            .test_tx_pool_accept(json_tx.inner.clone(), None)
            .expect("accept transaction");
        println!(">>> check tx result: {:?}  <<<", result);
    }

    Ok(())
}

fn check_account(address: &str, issuer_address: &str) -> Result<(), Box<dyn StdErr>> {
    let (network_info, configuration) = generate_configuration()?;
    let issuer = Address::from_str(issuer_address)?;
    let sender = Address::from_str(address)?;
    let iterator = InputIterator::new_with_address(&[sender.clone()], &network_info);
    let builder = SudtTransactionBuilder::new(configuration, iterator, &issuer, false)?;

    let (account_ckb_amount, account_udt_amount) = builder.check()?;
    eprintln!(
        "account: {:?} udt_amount: {}",
        account_ckb_amount / CKB_SHANNONS,
        account_udt_amount
    );
    Ok(())
}

fn generate_blocks(num: u64) -> Result<(), Box<dyn StdErr>> {
    let network_info = NetworkInfo::devnet();
    let rpc_client = CkbRpcClient::new(network_info.url.as_str());
    for i in 0..num {
        rpc_client.generate_block()?;
        // sleep 200ms
        std::thread::sleep(std::time::Duration::from_millis(200));
        eprintln!("block generated: {}", i);
    }
    Ok(())
}

fn generate_udt_type_script(address: &str) -> ckb_types::packed::Script {
    let address = Address::from_str(address).expect("parse address");
    let sudt_owner_lock_script: Script = (&address).into();

    let res = Script::new_builder()
        .code_hash(SIMPLE_CODE_HASH.pack())
        .hash_type(ScriptHashType::Data1.into())
        .args(sudt_owner_lock_script.calc_script_hash().as_bytes().pack())
        .build();
    res
}

fn get_nodes_info(node: &str) -> (String, H256) {
    let nodes_dir = std::env::var("NODES_DIR").expect("env var");
    let node_dir = format!("{}/{}", nodes_dir, node);
    let wallet =
        std::fs::read_to_string(format!("{}/ckb-chain/wallet", node_dir)).expect("read failed");
    let key = std::fs::read_to_string(format!("{}/ckb-chain/key", node_dir)).expect("read failed");
    (wallet, H256::from_str(&key.trim()).expect("parse hex"))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtScript {
    code_hash: H256,
    hash_type: String,
    /// args may be used in pattern matching
    args: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtCellDep {
    dep_type: String,
    tx_hash: H256,
    index: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtInfo {
    name: String,
    script: UdtScript,
    cell_deps: Vec<UdtCellDep>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtInfos {
    infos: Vec<UdtInfo>,
}

fn genrate_nodes_config() {
    let nodes_dir = std::env::var("NODES_DIR").expect("env var");
    let yaml_file_path = format!("{}/config.yml", nodes_dir);
    let content = std::fs::read_to_string(yaml_file_path).expect("read failed");
    let data: serde_yaml::Value = serde_yaml::from_str(&content).expect("Unable to parse YAML");
    let udt_info = UdtInfo {
        name: "simple_udt".to_string(),
        script: UdtScript {
            code_hash: SIMPLE_CODE_HASH.clone(),
            hash_type: "Data1".to_string(),
            args: "0x.*".to_string(),
        },
        cell_deps: vec![UdtCellDep {
            dep_type: "code".to_string(),
            tx_hash: get_env_hex("NEXT_PUBLIC_SIMPLE_UDT_TX_HASH"),
            index: 0,
        }],
    };

    let header = "# generated from nodes/config.yml, don't edit it manually\n";
    for i in 1..=3 {
        let mut data = data.clone();
        data["ckb"]["listening_port"] =
            serde_yaml::Value::Number(serde_yaml::Number::from(8344 + i - 1));
        data["ckb"]["annouced_name"] = serde_yaml::Value::String(format!("ckb-{}", i));
        data["rpc"]["listening_addr"] =
            serde_yaml::Value::String(format!("127.0.0.1:{}", 41714 + i - 1));
        data["ckb_chain"]["udt_whitelist"] = serde_yaml::to_value(vec![&udt_info]).unwrap();
        let new_yaml = header.to_string() + &serde_yaml::to_string(&data).unwrap();
        let config_path = format!("{}/{}/config.yml", nodes_dir, i);
        std::fs::write(config_path, new_yaml).expect("write failed");
    }
}

fn init_udt_accounts() -> Result<(), Box<dyn StdErr>> {
    let udt_owner = get_nodes_info("deployer");
    let wallets = [
        get_nodes_info("1"),
        get_nodes_info("2"),
        get_nodes_info("3"),
    ];

    init_or_send_udt(&udt_owner.0, &udt_owner, None, 1000000000000, true).expect("init udt");
    generate_blocks(4).expect("ok");

    init_or_send_udt(
        &udt_owner.0,
        &udt_owner,
        Some(&wallets[0].0),
        200000000000,
        true,
    )?;
    generate_blocks(4).expect("ok");

    init_or_send_udt(
        &udt_owner.0,
        &udt_owner,
        Some(&wallets[1].0),
        200000000000,
        true,
    )?;
    generate_blocks(4).expect("ok");

    init_or_send_udt(
        &udt_owner.0,
        &udt_owner,
        Some(&wallets[2].0),
        200000000000,
        true,
    )?;
    generate_blocks(4).expect("ok");

    check_account(&udt_owner.0, &udt_owner.0)?;
    check_account(&wallets[0].0, &udt_owner.0)?;
    check_account(&wallets[1].0, &udt_owner.0)?;
    check_account(&wallets[2].0, &udt_owner.0)?;

    let script = generate_udt_type_script(&udt_owner.0);
    println!("initialized udt_type_script: {} ...", script);
    Ok(())
}

fn main() -> Result<(), Box<dyn StdErr>> {
    init_udt_accounts()?;
    genrate_nodes_config();
    Ok(())
}
