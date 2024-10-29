use ckb_types::{
    core::{BlockView, DepType, ScriptHashType},
    packed::{CellDep, CellDepVec, CellDepVecBuilder, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Pack, PackVec},
};
use once_cell::sync::OnceCell;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, vec};
use tracing::info;

use crate::fiber::config::FiberScript;

use super::config::{UdtArgInfo, UdtCfgInfos};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Contract {
    CkbAuth,
    FundingLock,
    CommitmentLock,
    Secp256k1Lock,
    SimpleUDT,
}

#[derive(Clone, Debug)]
pub struct ContractsInfo {
    pub contract_default_scripts: HashMap<Contract, Script>,
    pub script_cell_deps: HashMap<Contract, Vec<CellDep>>,
    pub udt_whitelist: UdtCfgInfos,
}

#[derive(Clone, Debug)]
pub struct ContractsContext {
    pub contracts: ContractsInfo,
}

impl ContractsContext {
    pub fn new(
        genesis_block: BlockView,
        fiber_scripts: Vec<FiberScript>,
        udt_whitelist: UdtCfgInfos,
    ) -> Self {
        let mut contract_default_scripts: HashMap<Contract, Script> = HashMap::new();
        let mut script_cell_deps: HashMap<Contract, Vec<CellDep>> = HashMap::new();

        let genesis_tx = genesis_block
            .transaction(0)
            .expect("genesis block transaction #0 should exist");

        // setup secp256k1
        let secp256k1_binary_cell = genesis_tx
            .output(1)
            .expect("genesis block transaction #0 output #1 should exist");
        let secp256k1_binary_cell_type_script = secp256k1_binary_cell
            .type_()
            .to_opt()
            .expect("secp256k1 binary type script should exist");
        contract_default_scripts.insert(
            Contract::Secp256k1Lock,
            Script::new_builder()
                .code_hash(secp256k1_binary_cell_type_script.calc_script_hash())
                .hash_type(ScriptHashType::Type.into())
                .build(),
        );

        let secp256k1_dep_group_tx_hash = genesis_block
            .transaction(1)
            .expect("genesis block transaction #1 should exist")
            .hash();
        let secp256k1_dep_group_out_point = OutPoint::new_builder()
            .tx_hash(secp256k1_dep_group_tx_hash)
            .index(0u32.pack())
            .build();
        script_cell_deps.insert(
            Contract::Secp256k1Lock,
            vec![CellDep::new_builder()
                .out_point(secp256k1_dep_group_out_point)
                .dep_type(DepType::DepGroup.into())
                .build()],
        );

        let genesis_hash = genesis_block.hash();
        match format!("{genesis_hash:#x}").as_str() {
            "0x92b197aa1fba0f63633922c61c92375c9c074a93e85963554f5499fe1450d0e5" => {
                info!("Creating ContractsContext for mainnet");
            }
            "0x10639e0895502b5688a6be8cf69460d76541bfa4821629d86d62ba0aae3f9606" => {
                info!("Creating ContractsContext for testnet");
            }
            _ => {
                info!("Creating ContractsContext for devnet");
                let len = genesis_tx.outputs().len();
                // the last 4 cells are the default contracts: CkbAuth, FundingLock, CommitmentLock, SimpleUDT
                let ckb_auth_cell_dep = CellDep::new_builder()
                    .out_point(
                        OutPoint::new_builder()
                            .tx_hash(genesis_tx.hash())
                            .index((len - 4).pack())
                            .build(),
                    )
                    .dep_type(DepType::Code.into())
                    .build();
                script_cell_deps.insert(Contract::CkbAuth, vec![ckb_auth_cell_dep.clone()]);

                let funding_lock_cell_dep = CellDep::new_builder()
                    .out_point(
                        OutPoint::new_builder()
                            .tx_hash(genesis_tx.hash())
                            .index((len - 3).pack())
                            .build(),
                    )
                    .dep_type(DepType::Code.into())
                    .build();
                let output_data = genesis_tx.outputs_data().get(len - 3).unwrap().raw_data();
                script_cell_deps.insert(
                    Contract::FundingLock,
                    vec![funding_lock_cell_dep, ckb_auth_cell_dep.clone()],
                );
                contract_default_scripts.insert(
                    Contract::FundingLock,
                    Script::new_builder()
                        .code_hash(CellOutput::calc_data_hash(&output_data))
                        .hash_type(ScriptHashType::Data1.into())
                        .build(),
                );

                let commitment_lock_cell_dep = CellDep::new_builder()
                    .out_point(
                        OutPoint::new_builder()
                            .tx_hash(genesis_tx.hash())
                            .index((len - 2).pack())
                            .build(),
                    )
                    .dep_type(DepType::Code.into())
                    .build();
                let output_data = genesis_tx.outputs_data().get(len - 2).unwrap().raw_data();
                script_cell_deps.insert(
                    Contract::CommitmentLock,
                    vec![commitment_lock_cell_dep, ckb_auth_cell_dep],
                );
                contract_default_scripts.insert(
                    Contract::CommitmentLock,
                    Script::new_builder()
                        .code_hash(CellOutput::calc_data_hash(&output_data))
                        .hash_type(ScriptHashType::Data1.into())
                        .build(),
                );

                let simple_udt_cell_dep = CellDep::new_builder()
                    .out_point(
                        OutPoint::new_builder()
                            .tx_hash(genesis_tx.hash())
                            .index((len - 1).pack())
                            .build(),
                    )
                    .dep_type(DepType::Code.into())
                    .build();
                let output_data = genesis_tx.outputs_data().get(len - 1).unwrap().raw_data();
                script_cell_deps.insert(Contract::SimpleUDT, vec![simple_udt_cell_dep]);
                contract_default_scripts.insert(
                    Contract::SimpleUDT,
                    Script::new_builder()
                        .code_hash(CellOutput::calc_data_hash(&output_data))
                        .hash_type(ScriptHashType::Data1.into())
                        .build(),
                );
            }
        }

        // allow for overriding the default scripts and cell deps
        for fiber_script in fiber_scripts {
            let FiberScript {
                name,
                script,
                cell_deps,
            } = fiber_script;
            contract_default_scripts.insert(name, script.into());
            script_cell_deps.insert(name, cell_deps.into_iter().map(CellDep::from).collect());
        }

        Self {
            contracts: ContractsInfo {
                contract_default_scripts,
                script_cell_deps,
                udt_whitelist,
            },
        }
    }

    fn get_contracts_map(&self) -> &HashMap<Contract, Script> {
        &self.contracts.contract_default_scripts
    }

    pub(crate) fn get_cell_deps(&self, contracts: Vec<Contract>) -> CellDepVec {
        let mut builder: CellDepVecBuilder = CellDepVec::new_builder();
        for contract in contracts {
            if let Some(cell_deps) = self.contracts.script_cell_deps.get(&contract) {
                builder = builder.extend(cell_deps.clone());
            }
        }
        builder.build()
    }

    pub fn get_udt_whitelist(&self) -> &UdtCfgInfos {
        &self.contracts.udt_whitelist
    }

    pub(crate) fn get_script(&self, contract: Contract, args: &[u8]) -> Script {
        self.get_contracts_map()
            .get(&contract)
            .unwrap_or_else(|| panic!("Contract {:?} exists", contract))
            .clone()
            .as_builder()
            .args(args.pack())
            .build()
    }

    pub(crate) fn get_udt_info(&self, udt_script: &Script) -> Option<&UdtArgInfo> {
        for udt in &self.get_udt_whitelist().0 {
            let _type: ScriptHashType = udt_script.hash_type().try_into().expect("valid hash type");
            if udt.script.code_hash.pack() == udt_script.code_hash()
                && udt.script.hash_type == _type
            {
                let args = format!("0x{:x}", udt_script.args().raw_data());
                let pattern = Regex::new(&udt.script.args).expect("invalid expression");
                if pattern.is_match(&args) {
                    return Some(udt);
                }
            }
        }
        None
    }
}

pub static CONTRACTS_CONTEXT_INSTANCE: OnceCell<ContractsContext> = OnceCell::new();

pub fn init_contracts_context(
    genesis_block: BlockView,
    fiber_scripts: Vec<FiberScript>,
    udt_whitelist: UdtCfgInfos,
) {
    CONTRACTS_CONTEXT_INSTANCE
        .set(ContractsContext::new(
            genesis_block,
            fiber_scripts,
            udt_whitelist,
        ))
        .expect("init_contracts_context should only be called once");
}

fn get_contracts_context() -> &'static ContractsContext {
    CONTRACTS_CONTEXT_INSTANCE
        .get()
        .expect("init_contracts_context should be called first")
}

pub fn get_script_by_contract(contract: Contract, args: &[u8]) -> Script {
    get_contracts_context().get_script(contract, args)
}

pub fn get_cell_deps_by_contracts(contracts: Vec<Contract>) -> CellDepVec {
    get_contracts_context().get_cell_deps(contracts)
}

fn get_udt_info(script: &Script) -> Option<&UdtArgInfo> {
    get_contracts_context().get_udt_info(script)
}

pub fn check_udt_script(script: &Script) -> bool {
    get_udt_info(script).is_some()
}

pub fn get_udt_cell_deps(script: &Script) -> Option<CellDepVec> {
    get_udt_info(script).map(|udt| {
        udt.cell_deps
            .iter()
            .map(CellDep::from)
            .collect::<Vec<_>>()
            .pack()
    })
}

pub fn get_udt_whitelist() -> UdtCfgInfos {
    get_contracts_context().get_udt_whitelist().clone()
}

pub fn is_udt_type_auto_accept(script: &Script, amount: u128) -> bool {
    if let Some(udt_info) = get_udt_info(script) {
        if let Some(auto_accept_amount) = udt_info.auto_accept_amount {
            return amount >= auto_accept_amount;
        }
    }
    false
}

pub fn get_cell_deps(contracts: Vec<Contract>, udt_script: &Option<Script>) -> CellDepVec {
    let cell_deps = get_cell_deps_by_contracts(contracts);
    if let Some(udt_script) = udt_script {
        if let Some(udt_cell_deps) = get_udt_cell_deps(udt_script) {
            let res = cell_deps
                .into_iter()
                .chain(udt_cell_deps)
                .collect::<Vec<CellDep>>();
            return res.pack();
        }
    }
    cell_deps
}
