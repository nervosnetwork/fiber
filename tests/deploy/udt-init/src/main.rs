use ckb_chain_spec::ChainSpec;
use ckb_resource::Resource;
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
    core::BlockView,
    packed::CellOutput,
    prelude::{Entity, Unpack},
};
use ckb_types::{
    core::{DepType, ScriptHashType},
    packed::{OutPoint, Script},
    prelude::Pack,
    H256,
};
use ckb_types::{packed::CellDep, prelude::Builder};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};
use std::{fs, net::TcpListener};

use std::{error::Error as StdErr, str::FromStr};

const UDT_KINDS: [&str; 2] = ["SIMPLE_UDT", "XUDT"];

fn get_udt_info(udt_kind: &str) -> (H256, H256, usize) {
    let genesis_block = build_gensis_block();
    let genesis_tx = genesis_block
        .transaction(0)
        .expect("genesis block transaction #0 should exist");

    let index = if udt_kind == "SIMPLE_UDT" { 8 } else { 9 };
    let output_data = genesis_tx.outputs_data().get(index).unwrap().raw_data();
    (
        CellOutput::calc_data_hash(&output_data).unpack(),
        genesis_tx.hash().unpack(),
        index,
    )
}

fn get_genesis_contract_info(genesis_block: &BlockView, index: usize) -> (H256, H256) {
    let genesis_tx = genesis_block
        .transaction(0)
        .expect("genesis block transaction #0 should exist");
    let output_data = genesis_tx
        .outputs_data()
        .get(index)
        .expect("genesis contract output should exist")
        .raw_data();
    (
        CellOutput::calc_data_hash(&output_data).unpack(),
        genesis_tx.hash().unpack(),
    )
}

fn gen_dev_udt_handler(udt_kind: &str) -> SudtHandler {
    let (data_hash, genesis_tx, index) = get_udt_info(udt_kind);
    let script_id = ScriptId::new(data_hash, ScriptHashType::Data2);

    let udt_cell_dep = CellDep::new_builder()
        .out_point(
            OutPoint::new_builder()
                .tx_hash(genesis_tx.pack())
                .index(index)
                .build(),
        )
        .dep_type(DepType::Code)
        .build();

    ckb_sdk::transaction::handler::sudt::SudtHandler::new_with_customize(
        vec![udt_cell_dep],
        script_id,
    )
}

fn gen_dev_sighash_handler() -> Secp256k1Blake160SighashAllScriptHandler {
    let genesis_block = build_gensis_block();
    let secp256k1_dep_group_tx_hash = genesis_block
        .transaction(1)
        .expect("genesis block transaction #1 should exist")
        .hash();
    let secp256k1_dep_group_out_point = OutPoint::new_builder()
        .tx_hash(secp256k1_dep_group_tx_hash)
        .index(0u32)
        .build();
    let cell_dep = CellDep::new_builder()
        .out_point(secp256k1_dep_group_out_point)
        .dep_type(DepType::DepGroup)
        .build();

    Secp256k1Blake160SighashAllScriptHandler::new_with_customize(vec![cell_dep])
}

fn generate_configuration(
    udt_kind: &str,
) -> Result<(NetworkInfo, TransactionBuilderConfiguration), Box<dyn StdErr>> {
    let network_info = NetworkInfo::devnet();
    let mut configuration =
        TransactionBuilderConfiguration::new_devnet().expect("new devnet configuration");

    configuration.register_script_handler(Box::new(gen_dev_sighash_handler()));
    configuration.register_script_handler(Box::new(gen_dev_udt_handler(udt_kind)));
    Ok((network_info, configuration))
}

fn init_or_send_udt(
    udt_kind: &str,
    issuer_address: &str,
    sender_info: &(String, H256),
    receiver_address: Option<&str>,
    sudt_amount: u128,
    apply: bool,
) -> Result<(), Box<dyn StdErr>> {
    let (network_info, configuration) = generate_configuration(udt_kind)?;

    let issuer = Address::from_str(issuer_address)?;
    let sender = Address::from_str(&sender_info.0)?;
    let receiver = if let Some(addr) = receiver_address {
        Address::from_str(addr)?
    } else {
        sender.clone()
    };

    let iterator = InputIterator::new_with_address(&[sender], &network_info);
    let owner_mode = receiver_address.is_none();
    let mut builder = SudtTransactionBuilder::new(configuration, iterator, &issuer, owner_mode)?;
    builder.set_sudt_type_script(generate_udt_type_script(udt_kind, issuer_address));
    builder.add_output(&receiver, sudt_amount);

    let mut tx_with_groups = builder.build(&Default::default())?;

    let private_keys = vec![sender_info.1.clone()];

    TransactionSigner::new(&network_info).sign_transaction(
        &mut tx_with_groups,
        &SignContexts::new_sighash_h256(private_keys)?,
    )?;

    let json_tx = ckb_jsonrpc_types::TransactionView::from(tx_with_groups.get_tx_view().clone());
    if apply {
        let tx_hash = CkbRpcClient::new(network_info.url.as_str())
            .send_transaction(json_tx.inner, None)
            .expect("send transaction");
        println!(">>> tx {} sent! <<<", tx_hash);
    } else {
        let result = CkbRpcClient::new(network_info.url.as_str())
            .test_tx_pool_accept(json_tx.inner, None)
            .expect("accept transaction");
        println!(">>> check tx result: {:?}  <<<", result);
    }

    Ok(())
}

fn generate_blocks(num: u64) -> Result<(), Box<dyn StdErr>> {
    let network_info = NetworkInfo::devnet();
    let rpc_client = CkbRpcClient::new(network_info.url.as_str());
    for _i in 0..num {
        rpc_client.generate_block()?;
        // sleep 200ms
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(())
}

fn generate_udt_type_script(udt_kind: &str, address: &str) -> ckb_types::packed::Script {
    let address = Address::from_str(address).expect("parse address");
    let sudt_owner_lock_script: Script = (&address).into();
    let (code_hash, _, _) = get_udt_info(udt_kind);
    Script::new_builder()
        .code_hash(code_hash.pack())
        .hash_type(ScriptHashType::Data2)
        .args(sudt_owner_lock_script.calc_script_hash().as_bytes().pack())
        .build()
}

fn generate_udt_type_script_from_genesis(
    genesis_block: &BlockView,
    index: usize,
    address: &str,
) -> Script {
    let address = Address::from_str(address).expect("parse address");
    let owner_lock_script: Script = (&address).into();
    let (code_hash, _) = get_genesis_contract_info(genesis_block, index);
    Script::new_builder()
        .code_hash(code_hash.pack())
        .hash_type(ScriptHashType::Data2)
        .args(owner_lock_script.calc_script_hash().as_bytes().pack())
        .build()
}

fn get_nodes_info(node: &str) -> (String, H256) {
    let nodes_dir = std::env::var("NODES_DIR").expect("env var");
    let node_dir = format!("{}/{}", nodes_dir, node);
    let wallet = std::fs::read_to_string(format!("{}/ckb/wallet", node_dir)).expect("read failed");
    let key = std::fs::read_to_string(format!("{}/ckb/plain_key", node_dir)).expect("read failed");
    (wallet, H256::from_str(key.trim()).expect("parse hex"))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtScript {
    code_hash: H256,
    hash_type: String,
    /// args may be used in pattern matching
    args: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtDep {
    cell_dep: Option<UdtCellDep>,
    type_id: Option<ckb_jsonrpc_types::Script>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtCellDep {
    out_point: ckb_jsonrpc_types::OutPoint,
    dep_type: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UdtInfo {
    name: String,
    script: UdtScript,
    auto_accept_amount: Option<u128>,
    cell_deps: Vec<UdtDep>,
}

fn generate_udt_infos(genesis_block: &BlockView, issuer_address: &str) -> Vec<UdtInfo> {
    UDT_KINDS
        .into_iter()
        .enumerate()
        .map(|(offset, udt)| {
            let index = 8 + offset;
            let (code_hash, genesis_tx) = get_genesis_contract_info(genesis_block, index);
            let args = if udt == "SIMPLE_UDT" {
                let script =
                    generate_udt_type_script_from_genesis(genesis_block, index, issuer_address);
                format!("0x{:x}", script.args().raw_data())
            } else {
                "0x.*".to_string()
            };

            UdtInfo {
                name: udt.to_string(),
                auto_accept_amount: Some(1000),
                script: UdtScript {
                    code_hash,
                    hash_type: "Data2".to_string(),
                    args,
                },
                cell_deps: vec![UdtDep {
                    cell_dep: Some(UdtCellDep {
                        dep_type: "code".to_string(),
                        out_point: ckb_jsonrpc_types::OutPoint {
                            tx_hash: genesis_tx,
                            index: (index as u32).into(),
                        },
                    }),
                    type_id: None,
                }],
            }
        })
        .collect()
}

fn is_port_available(port: u16) -> bool {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener); // Close the listener
            true
        }
        Err(_) => false,
    }
}

fn generate_ports(num_ports: usize) -> Vec<u16> {
    let mut ports = HashSet::new();
    let mut rng = rand::rng();

    while ports.len() < num_ports {
        // avoid https://en.wikipedia.org/wiki/Ephemeral_port
        let port: u16 = rng.random_range(1024..32768);
        if is_port_available(port) {
            ports.insert(port);
        }
    }

    ports.into_iter().collect()
}

fn create_node_config(
    base_data: &serde_yaml::Value,
    udt_infos: &[UdtInfo],
    node_index: usize,
    fiber_port: u16,
    rpc_port: u16,
    node3_rpc_port: Option<u16>,
) -> serde_yaml::Value {
    let mut config = base_data.clone();

    config["fiber"]["listening_addr"] =
        serde_yaml::Value::String(format!("/ip4/0.0.0.0/tcp/{}", fiber_port));
    config["fiber"]["announced_addrs"] =
        serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(format!(
            "/ip4/127.0.0.1/tcp/{}",
            fiber_port
        ))]);
    config["fiber"]["announced_node_name"] =
        serde_yaml::Value::String(format!("fiber-{}", node_index));
    config["rpc"]["listening_addr"] = serde_yaml::Value::String(format!("127.0.0.1:{}", rpc_port));
    config["ckb"]["udt_whitelist"] = serde_yaml::to_value(udt_infos).unwrap();

    if matches!(node_index, 1 | 2) {
        config["rpc"]["enabled_modules"]
            .as_sequence_mut()
            .expect("rpc.enabled_modules must be a sequence")
            .push(serde_yaml::Value::String("liquidity".to_string()));
    }

    // Node 3 acts as a CCH node (in-process mode)
    if node_index == 3 {
        config["services"]
            .as_sequence_mut()
            .unwrap()
            .push(serde_yaml::Value::String("cch".to_string()));
    }

    // Node 4 (cch) is a standalone CCH service connecting to node 3 via RPC
    if node_index == 4 {
        // Remove fiber and ckb sections — CCH-only node doesn't run these
        config.as_mapping_mut().unwrap().remove("fiber");
        config.as_mapping_mut().unwrap().remove("ckb");

        // Remove ignore_startup_failure from CCH config — must succeed for CCH-only node
        if let Some(cch) = config.get_mut("cch") {
            cch.as_mapping_mut()
                .unwrap()
                .remove("ignore_startup_failure");
            // Point to node 3's RPC
            let target_rpc_port = node3_rpc_port.unwrap_or(21713 + 3);
            cch["fiber_rpc_url"] =
                serde_yaml::Value::String(format!("http://127.0.0.1:{}", target_rpc_port));

            // Standalone CCH needs the full wrapped BTC type script because
            // the contracts context is not available without fiber/ckb services.
            let wrapped_btc_args = cch["wrapped_btc_type_script_args"]
                .as_str()
                .expect("wrapped_btc_type_script_args must be set")
                .to_string();
            let (code_hash, _, _) = get_udt_info("SIMPLE_UDT");
            let script_json = format!(
                r#"{{"code_hash":"0x{:x}","hash_type":"data2","args":"{}"}}"#,
                code_hash, wrapped_btc_args
            );
            cch["wrapped_btc_type_script"] = serde_yaml::Value::String(script_json);
        }

        // Only enable cch module in RPC
        config["rpc"]["enabled_modules"] =
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("cch".to_string())]);

        // Services: only rpc and cch
        config["services"] = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("rpc".to_string()),
            serde_yaml::Value::String("cch".to_string()),
        ]);
    }

    config
}

fn write_node_config(
    nodes_dir: &Path,
    config_dir: &str,
    header: &str,
    config_data: &serde_yaml::Value,
    dev_config: &Path,
) {
    let yaml_content = header.to_string() + &serde_yaml::to_string(config_data).unwrap();
    let config_path = nodes_dir.join(config_dir).join("config.yml");
    std::fs::write(config_path, yaml_content).expect("write config failed");

    let node_dev_config = nodes_dir.join(config_dir).join("dev.toml");
    // CCH-only node doesn't need dev.toml (no fiber/ckb sections)
    if config_dir != "cch" {
        fs::copy(dev_config, node_dev_config).expect("copy dev.toml failed");
    }
}

fn generate_nodes_config() {
    let node_dir_env = std::env::var("NODES_DIR").expect("env var");
    let nodes_dir = Path::new(&node_dir_env);
    let yaml_file_path = nodes_dir.join("deployer/config.yml");
    let content = std::fs::read_to_string(yaml_file_path).expect("read failed");
    let data: serde_yaml::Value = serde_yaml::from_str(&content).expect("Unable to parse YAML");
    let genesis_block = build_gensis_block();
    let issuer_address =
        fs::read_to_string(nodes_dir.join("deployer/ckb/wallet")).expect("read deployer wallet");
    let udt_infos = generate_udt_infos(&genesis_block, issuer_address.trim());
    let header = format!(
        "{}\n{}\n\n",
        "# this is generated from nodes/deployer/config.yml, any changes will not be checked in",
        "# you can edit nodes/deployer/config.yml and run `REMOVE_OLD_STATE=y ./tests/nodes/start.sh TESTCASE` to regenerate"
    );
    let config_dirs = ["bootnode", "1", "2", "3", "cch"];
    let on_github_action = std::env::var("ON_GITHUB_ACTION").is_ok();
    let gen_ports = if on_github_action {
        Some(generate_ports(8).into_iter())
    } else {
        None
    };
    let mut gen_ports_iter = gen_ports;
    let mut ports_map: Vec<(u16, u16)> = Vec::new();
    let dev_config = nodes_dir.join("deployer/dev.toml");

    let mut node3_rpc_port: Option<u16> = None;

    for (i, &config_dir) in config_dirs.iter().enumerate() {
        let default_ports = (8343 + i as u16, 21713 + i as u16);
        let (fiber_port, rpc_port) = match (&mut gen_ports_iter, i) {
            (Some(iter), i) if i != 0 => (iter.next().unwrap(), iter.next().unwrap()),
            _ => default_ports,
        };
        ports_map.extend([(8343 + i as u16, fiber_port), (21713 + i as u16, rpc_port)]);

        // Remember node 3's actual RPC port for the CCH-only node
        if i == 3 {
            node3_rpc_port = Some(rpc_port);
        }

        let config_data =
            create_node_config(&data, &udt_infos, i, fiber_port, rpc_port, node3_rpc_port);
        write_node_config(nodes_dir, config_dir, &header, &config_data, &dev_config);
    }

    if on_github_action {
        if let Err(e) = update_bruno_configs(nodes_dir, &ports_map) {
            eprintln!("Warning: Failed to update Bruno configs: {}", e);
        }
    }

    // Write ports for nodes that always start (1, 2, 3) so wait.sh can verify they're ready.
    // Bootnode (first 2 entries) and CCH-only node (last 2 entries) start conditionally,
    // so their ports are excluded.
    let content = ports_map
        .iter()
        .skip(2)
        .take(6)
        .map(|(_, port)| port.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let port_file_path = nodes_dir.join(".ports");
    std::fs::write(port_file_path, content).expect("write ports list");
}

struct BrunoEnvironmentValues {
    simple_udt_code_hash: H256,
    simple_udt_script_args: String,
    simple_udt_script_hash: H256,
    xudt_code_hash: H256,
    xudt_script_args: String,
    xudt_script_hash: H256,
    liquidity_lock_code_hash: H256,
    liquidity_lock_tx_hash: H256,
}

impl BrunoEnvironmentValues {
    fn from_nodes_dir(nodes_dir: &Path) -> Result<Self, Box<dyn StdErr>> {
        let chain_spec =
            ChainSpec::load_from(&Resource::file_system(nodes_dir.join("deployer/dev.toml")))?;
        let genesis_block = chain_spec.build_genesis()?;
        let issuer_address = fs::read_to_string(nodes_dir.join("deployer/ckb/wallet"))?;
        let simple_udt =
            generate_udt_type_script_from_genesis(&genesis_block, 8, issuer_address.trim());
        let xudt = generate_udt_type_script_from_genesis(&genesis_block, 9, issuer_address.trim());
        let (simple_udt_code_hash, _) = get_genesis_contract_info(&genesis_block, 8);
        let (xudt_code_hash, _) = get_genesis_contract_info(&genesis_block, 9);
        let (liquidity_lock_code_hash, liquidity_lock_tx_hash) =
            get_genesis_contract_info(&genesis_block, 10);

        Ok(Self {
            simple_udt_code_hash,
            simple_udt_script_args: format!("0x{:x}", simple_udt.args().raw_data()),
            simple_udt_script_hash: simple_udt.calc_script_hash().unpack(),
            xudt_code_hash,
            xudt_script_args: format!("0x{:x}", xudt.args().raw_data()),
            xudt_script_hash: xudt.calc_script_hash().unpack(),
            liquidity_lock_code_hash,
            liquidity_lock_tx_hash,
        })
    }

    fn generated_vars(&self) -> Vec<(&'static str, String)> {
        let simple_udt_code_hash = format!("{:#x}", self.simple_udt_code_hash);
        let xudt_code_hash = format!("{:#x}", self.xudt_code_hash);
        let liquidity_lock_tx_hash = format!("{:#x}", self.liquidity_lock_tx_hash);

        vec![
            ("LIQUIDITY_CKB_ASSET_ID", "ckb".to_string()),
            ("LIQUIDITY_SIMPLE_UDT_ASSET_ID", "simple-udt".to_string()),
            ("SIMPLE_UDT_CODE_HASH", simple_udt_code_hash.clone()),
            ("SIMPLE_UDT_HASH_TYPE", "data2".to_string()),
            (
                "SIMPLE_UDT_SCRIPT_ARGS",
                self.simple_udt_script_args.clone(),
            ),
            (
                "SIMPLE_UDT_TYPE_SCRIPT",
                format!(
                    r#"{{"code_hash":"{simple_udt_code_hash}","hash_type":"data2","args":"{}"}}"#,
                    self.simple_udt_script_args
                ),
            ),
            (
                "SIMPLE_UDT_SCRIPT_HASH",
                format!("{:#x}", self.simple_udt_script_hash),
            ),
            ("XUDT_CODE_HASH", xudt_code_hash.clone()),
            ("XUDT_HASH_TYPE", "data2".to_string()),
            ("XUDT_SCRIPT_ARGS", self.xudt_script_args.clone()),
            (
                "XUDT_TYPE_SCRIPT",
                format!(
                    r#"{{"code_hash":"{xudt_code_hash}","hash_type":"data2","args":"{}"}}"#,
                    self.xudt_script_args
                ),
            ),
            ("XUDT_SCRIPT_HASH", format!("{:#x}", self.xudt_script_hash)),
            (
                "LIQUIDITY_LOCK_CODE_HASH",
                format!("{:#x}", self.liquidity_lock_code_hash),
            ),
            ("LIQUIDITY_LOCK_TX_HASH", liquidity_lock_tx_hash.clone()),
            ("LIQUIDITY_LOCK_INDEX", "0xa".to_string()),
            ("LIQUIDITY_LOCK_HASH_TYPE", "data2".to_string()),
            ("LIQUIDITY_LOCK_DEP_TYPE", "code".to_string()),
            (
                "LIQUIDITY_LOCK_OUTPOINT",
                format!(r#"{{"tx_hash":"{liquidity_lock_tx_hash}","index":"0xa"}}"#),
            ),
        ]
    }
}

fn get_bruno_var(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let (candidate, value) = line.trim().split_once(':')?;
        (candidate == key).then(|| value.trim().to_string())
    })
}

fn set_bruno_var(content: &mut String, key: &str, value: &str) {
    let prefix = format!("  {key}:");
    if let Some(line) = content.lines().find(|line| line.starts_with(&prefix)) {
        let replacement = format!("  {key}: {value}");
        *content = content.replacen(line, &replacement, 1);
        return;
    }

    let insertion = format!("  {key}: {value}\n");
    let closing = content.rfind('}').expect("Bruno vars block must close");
    content.insert_str(closing, &insertion);
}

fn render_bruno_environment(
    mut content: String,
    ports_map: &[(u16, u16)],
    values: &BrunoEnvironmentValues,
) -> String {
    for (default_port, actual_port) in ports_map {
        content = content.replace(&default_port.to_string(), &actual_port.to_string());
    }

    for (key, value) in [
        (
            "LIQUIDITY_PROVIDER_RPC_URL",
            get_bruno_var(&content, "NODE1_RPC_URL").expect("NODE1_RPC_URL must be set"),
        ),
        (
            "LIQUIDITY_CLIENT_RPC_URL",
            get_bruno_var(&content, "NODE2_RPC_URL").expect("NODE2_RPC_URL must be set"),
        ),
        (
            "LIQUIDITY_PROVIDER_PUBKEY",
            get_bruno_var(&content, "NODE1_PUBKEY").expect("NODE1_PUBKEY must be set"),
        ),
        (
            "LIQUIDITY_CLIENT_PUBKEY",
            get_bruno_var(&content, "NODE2_PUBKEY").expect("NODE2_PUBKEY must be set"),
        ),
    ] {
        set_bruno_var(&mut content, key, &value);
    }
    for (key, value) in values.generated_vars() {
        set_bruno_var(&mut content, key, &value);
    }

    content
}

fn update_bruno_configs(nodes_dir: &Path, ports_map: &[(u16, u16)]) -> Result<(), Box<dyn StdErr>> {
    let bruno_dir = nodes_dir.join("../bruno/environments/");
    let values = BrunoEnvironmentValues::from_nodes_dir(nodes_dir)?;

    for config_entry in std::fs::read_dir(bruno_dir)? {
        let config_path = config_entry?.path();
        let content =
            render_bruno_environment(std::fs::read_to_string(&config_path)?, ports_map, &values);
        std::fs::write(&config_path, content)?;
    }

    Ok(())
}

fn init_udt_accounts() -> Result<(), Box<dyn StdErr>> {
    let udt_owner = get_nodes_info("deployer");
    for udt in UDT_KINDS {
        init_or_send_udt(
            udt,
            &udt_owner.0,
            &udt_owner,
            None,
            0xfffffffffffffffffffffffffffffff,
            true,
        )
        .expect("init udt");
        generate_blocks(8).expect("ok");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        for i in 0..3 {
            let wallet = get_nodes_info(&(i + 1).to_string());
            init_or_send_udt(
                udt,
                &udt_owner.0,
                &udt_owner,
                Some(&wallet.0),
                0xffffffffffffffffffffffffffffff,
                true,
            )?;
            generate_blocks(8).expect("ok");
        }

        let script = generate_udt_type_script(udt, &udt_owner.0);
        println!("initialized udt_type_script: {} ...", script);
    }
    Ok(())
}

fn build_gensis_block() -> BlockView {
    let node_dir_env = std::env::var("NODES_DIR").expect("env var");
    let nodes_dir = Path::new(&node_dir_env);
    let dev_toml = nodes_dir.join("deployer/dev.toml");
    let chain_spec =
        ChainSpec::load_from(&Resource::file_system(dev_toml)).expect("load chain spec");
    chain_spec.build_genesis().expect("build genesis block")
}

fn main() -> Result<(), Box<dyn StdErr>> {
    if std::env::var("GENERATE_BRUNO_ENVIRONMENTS_ONLY").is_ok() {
        let nodes_dir = std::env::var("NODES_DIR").expect("NODES_DIR must be set");
        update_bruno_configs(Path::new(&nodes_dir), &[])?;
        return Ok(());
    }

    generate_nodes_config();
    init_udt_accounts()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, time::SystemTime};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("udt-init-{name}-{unique}"));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove temp dir");
        }
    }

    fn parse_bruno_vars(content: &str) -> HashMap<String, String> {
        content
            .lines()
            .filter_map(|line| {
                let (key, value) = line.trim().split_once(':')?;
                Some((key.to_string(), value.trim().to_string()))
            })
            .collect()
    }

    #[test]
    fn generated_nodes_one_and_two_enable_liquidity() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let deployer_dir = manifest_dir.join("../../nodes/deployer");
        let template: serde_yaml::Value = serde_yaml::from_str(
            &fs::read_to_string(deployer_dir.join("config.yml")).expect("read config template"),
        )
        .expect("parse config template");

        let configs = (0..=3)
            .map(|node_index| {
                create_node_config(
                    &template,
                    &[],
                    node_index,
                    8343 + node_index as u16,
                    21713 + node_index as u16,
                    None,
                )
            })
            .collect::<Vec<_>>();

        for node_index in [1, 2] {
            let config = &configs[node_index];
            assert!(config["rpc"]["enabled_modules"]
                .as_sequence()
                .expect("enabled modules")
                .contains(&serde_yaml::Value::String("liquidity".to_string())));
            assert_eq!(
                config["fiber"]["announced_node_name"],
                format!("fiber-{node_index}")
            );
            assert_eq!(
                config["fiber"]["listening_addr"],
                format!("/ip4/0.0.0.0/tcp/{}", 8343 + node_index as u16)
            );
            assert_eq!(
                config["rpc"]["listening_addr"],
                format!("127.0.0.1:{}", 21713 + node_index as u16)
            );
            assert_eq!(config["fiber"]["chain"], "dev.toml");
        }

        for node_index in [0, 3] {
            assert!(!configs[node_index]["rpc"]["enabled_modules"]
                .as_sequence()
                .expect("enabled modules")
                .contains(&serde_yaml::Value::String("liquidity".to_string())));
        }
        let node_names = configs
            .iter()
            .map(|config| {
                config["fiber"]["announced_node_name"]
                    .as_str()
                    .expect("announced node name")
            })
            .collect::<HashSet<_>>();
        let rpc_addresses = configs
            .iter()
            .map(|config| {
                config["rpc"]["listening_addr"]
                    .as_str()
                    .expect("RPC address")
            })
            .collect::<HashSet<_>>();
        assert_eq!(node_names.len(), configs.len());
        assert_eq!(rpc_addresses.len(), configs.len());
        assert!(configs
            .iter()
            .all(|config| config["fiber"]["base_dir"].is_null()));
        assert!(configs
            .iter()
            .all(|config| config["ckb"]["base_dir"].is_null()));

        let node_keys = ["bootnode", "1", "2", "3"].map(|node| {
            fs::read(manifest_dir.join("../../nodes").join(node).join("fiber/sk"))
                .expect("read node key")
        });
        assert!(node_keys.iter().all(|key| !key.is_empty()));
        assert_eq!(
            node_keys.iter().collect::<HashSet<_>>().len(),
            node_keys.len()
        );
        assert!(!template["rpc"]["enabled_modules"]
            .as_sequence()
            .expect("template enabled modules")
            .contains(&serde_yaml::Value::String("liquidity".to_string())));

        let chain_spec =
            ChainSpec::load_from(&Resource::file_system(deployer_dir.join("dev.toml")))
                .expect("load checked-in dev chain spec");
        let genesis = chain_spec.build_genesis().expect("build dev genesis block");
        let genesis_tx = genesis
            .transaction(0)
            .expect("genesis block transaction #0");
        let liquidity_lock_data = genesis_tx
            .outputs_data()
            .get(10)
            .expect("liquidity-lock genesis output #10")
            .raw_data();
        let artifact = fs::read(manifest_dir.join("../contracts/liquidity-lock"))
            .expect("read liquidity-lock artifact");

        assert_eq!(liquidity_lock_data.as_ref(), artifact);
        assert_eq!(
            genesis_tx.outputs().get(10).expect("output #10").type_(),
            None.into()
        );
    }

    #[test]
    fn generated_bruno_environments_include_liquidity_chain_identity() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let temp = TempDir::new("bruno-environments");
        let nodes_dir = temp.0.join("nodes");
        let environments_dir = temp.0.join("bruno/environments");
        let deployer_dir = nodes_dir.join("deployer");
        let contracts_dir = temp.0.join("deploy/contracts");
        fs::create_dir_all(deployer_dir.join("ckb")).expect("create deployer dir");
        fs::create_dir_all(&contracts_dir).expect("create contracts dir");
        fs::create_dir_all(&environments_dir).expect("create environments dir");

        fs::copy(
            manifest_dir.join("../../nodes/deployer/dev.toml"),
            deployer_dir.join("dev.toml"),
        )
        .expect("copy dev chain spec");
        fs::copy(
            manifest_dir.join("../../nodes/deployer/ckb/wallet"),
            deployer_dir.join("ckb/wallet"),
        )
        .expect("copy deployer wallet");
        for contract in [
            "auth",
            "funding-lock",
            "commitment-lock",
            "simple_udt",
            "xudt_rce",
            "liquidity-lock",
        ] {
            fs::copy(
                manifest_dir.join("../contracts").join(contract),
                contracts_dir.join(contract),
            )
            .expect("copy genesis contract");
        }

        for environment in ["test.bru", "xudt-test.bru"] {
            fs::copy(
                manifest_dir
                    .join("../../bruno/environments")
                    .join(environment),
                environments_dir.join(environment),
            )
            .expect("copy Bruno environment");
        }

        let ports = [(8344, 31001), (21714, 32001), (8345, 31002), (21715, 32002)];
        update_bruno_configs(&nodes_dir, &ports).expect("generate Bruno environments");

        for environment in ["test.bru", "xudt-test.bru"] {
            let vars = parse_bruno_vars(
                &fs::read_to_string(environments_dir.join(environment))
                    .expect("read generated Bruno environment"),
            );

            assert_eq!(vars["NODE1_RPC_URL"], "http://127.0.0.1:32001");
            assert_eq!(vars["NODE2_RPC_URL"], "http://127.0.0.1:32002");
            assert_eq!(vars["LIQUIDITY_PROVIDER_RPC_URL"], vars["NODE1_RPC_URL"]);
            assert_eq!(vars["LIQUIDITY_CLIENT_RPC_URL"], vars["NODE2_RPC_URL"]);
            assert_eq!(vars["LIQUIDITY_PROVIDER_PUBKEY"], vars["NODE1_PUBKEY"]);
            assert_eq!(vars["LIQUIDITY_CLIENT_PUBKEY"], vars["NODE2_PUBKEY"]);
            assert_eq!(
                vars["NODE1_PUBKEY"],
                "02a64b8993f33b2ebd37a4de1c9441f491291a4e779da8e519bcfb7c1f3f56c9c0"
            );
            assert_eq!(
                vars["NODE2_PUBKEY"],
                "02bcbd0e0d811d13363af1e5998f56e74e6aab8a7aa44005e1ce7d696a4d3f10f6"
            );

            assert_eq!(vars["LIQUIDITY_CKB_ASSET_ID"], "ckb");
            assert_eq!(vars["LIQUIDITY_SIMPLE_UDT_ASSET_ID"], "simple-udt");
            assert_eq!(
                vars["SIMPLE_UDT_CODE_HASH"],
                "0xe1e354d6d643ad42724d40967e334984534e0367405c5ae42a9d7d63d77df419"
            );
            assert_eq!(vars["SIMPLE_UDT_HASH_TYPE"], "data2");
            assert_eq!(
                vars["SIMPLE_UDT_SCRIPT_ARGS"],
                "0x32e555f3ff8e135cece1351a6a2971518392c1e30375c1e006ad0ce8eac07947"
            );
            assert_eq!(
                vars["SIMPLE_UDT_SCRIPT_HASH"],
                "0xd94e7e2b14e2dcec245b22372b19b9d07651f3932f2c153edaf79f14a6b3c9f8"
            );
            assert_eq!(
                vars["XUDT_CODE_HASH"],
                "0x50bd8d6680b8b9cf98b73f3c08faf8b2a21914311954118ad6609be6e78a1b95"
            );
            assert_eq!(vars["XUDT_HASH_TYPE"], "data2");
            assert_eq!(
                vars["XUDT_SCRIPT_HASH"],
                "0x099b74b3fe92414dc0e598bec1c3cdd1b93b0dd72e1cdadb51f5bf95a6715dc3"
            );
            assert_eq!(
                vars["LIQUIDITY_LOCK_CODE_HASH"],
                "0x70734e0c3b5109538b9801682cc8ef3effc5b5c8214900e91f19799719d7620f"
            );
            assert_eq!(vars["LIQUIDITY_LOCK_INDEX"], "0xa");
            assert_eq!(vars["LIQUIDITY_LOCK_HASH_TYPE"], "data2");
            assert_eq!(vars["LIQUIDITY_LOCK_DEP_TYPE"], "code");
            assert_eq!(
                vars["LIQUIDITY_LOCK_TX_HASH"],
                "0x2243dabbe122098f1eb069b45eb91f6b127abc398f5a3853c2d09360d64f5e88"
            );
            assert_eq!(vars["SIMPLE_UDT_SCRIPT_ARGS"], vars["XUDT_SCRIPT_ARGS"]);
            assert!(vars["SIMPLE_UDT_TYPE_SCRIPT"].contains(&vars["SIMPLE_UDT_CODE_HASH"]));
            assert!(vars["XUDT_TYPE_SCRIPT"].contains(&vars["XUDT_CODE_HASH"]));
            assert!(vars["LIQUIDITY_LOCK_OUTPOINT"].contains(&vars["LIQUIDITY_LOCK_TX_HASH"]));
            let expected_legacy_udt_hash = if environment == "test.bru" {
                &vars["SIMPLE_UDT_CODE_HASH"]
            } else {
                &vars["XUDT_CODE_HASH"]
            };
            assert_eq!(&vars["UDT_CODE_HASH"], expected_legacy_udt_hash);
            assert_eq!(&vars["UDT_SCRIPT_ARGS"], &vars["SIMPLE_UDT_SCRIPT_ARGS"]);
        }
    }

    #[test]
    fn generated_simple_udt_whitelist_matches_bruno_script() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let deployer_dir = manifest_dir.join("../../nodes/deployer");
        let chain_spec =
            ChainSpec::load_from(&Resource::file_system(deployer_dir.join("dev.toml")))
                .expect("load checked-in dev chain spec");
        let genesis = chain_spec.build_genesis().expect("build dev genesis block");
        let issuer =
            fs::read_to_string(deployer_dir.join("ckb/wallet")).expect("read deployer wallet");
        let udt_infos = generate_udt_infos(&genesis, issuer.trim());
        let simple_script = generate_udt_type_script_from_genesis(&genesis, 8, issuer.trim());
        let expected_args = format!("0x{:x}", simple_script.args().raw_data());

        assert_eq!(udt_infos[0].name, "SIMPLE_UDT");
        assert_eq!(udt_infos[0].script.args, expected_args);
        assert_eq!(udt_infos[1].name, "XUDT");
        assert_eq!(udt_infos[1].script.args, "0x.*");

        let template: serde_yaml::Value = serde_yaml::from_str(
            &fs::read_to_string(deployer_dir.join("config.yml")).expect("read config template"),
        )
        .expect("parse config template");
        for node_index in 0..=3 {
            let config = create_node_config(
                &template,
                &udt_infos,
                node_index,
                8343 + node_index as u16,
                21713 + node_index as u16,
                None,
            );
            assert_eq!(
                config["ckb"]["udt_whitelist"][0]["script"]["args"],
                expected_args
            );
            assert_eq!(config["ckb"]["udt_whitelist"][1]["script"]["args"], "0x.*");
        }

        let values = BrunoEnvironmentValues::from_nodes_dir(&manifest_dir.join("../../nodes"))
            .expect("generate Bruno values");
        assert_eq!(values.simple_udt_script_args, expected_args);
        assert_eq!(values.xudt_script_args, expected_args);
    }
}
