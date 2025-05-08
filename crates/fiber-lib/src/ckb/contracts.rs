use ckb_sdk::{
    rpc::ckb_indexer::{Order, ScriptType, SearchKey, SearchMode},
    CkbRpcClient,
};
use ckb_types::{
    core::{BlockView, DepType, ScriptHashType},
    packed::{CellDep, CellDepVec, CellDepVecBuilder, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Pack, PackVec},
};
use once_cell::sync::OnceCell;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, vec};
use thiserror::Error;
use tracing::info;

use crate::fiber::{
    config::FiberScript,
    gen::fiber::{UdtDep, UdtDepUnion},
};

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
pub enum ScriptCellDep {
    CellDep(CellDep),
    TypeID(Script),
}

impl From<crate::fiber::config::ScriptCellDep> for ScriptCellDep {
    fn from(script_cell_dep: crate::fiber::config::ScriptCellDep) -> Self {
        match script_cell_dep {
            crate::fiber::config::ScriptCellDep {
                cell_dep: Some(cell_dep),
                type_id: None,
            } => ScriptCellDep::CellDep(cell_dep.into()),
            crate::fiber::config::ScriptCellDep {
                cell_dep: None,
                type_id: Some(type_id),
            } => ScriptCellDep::TypeID(type_id.into()),
            _ => panic!("Invalid ScriptCellDep"),
        }
    }
}

impl From<CellDep> for ScriptCellDep {
    fn from(cell_dep: CellDep) -> Self {
        ScriptCellDep::CellDep(cell_dep)
    }
}

impl From<Script> for ScriptCellDep {
    fn from(type_id: Script) -> Self {
        ScriptCellDep::TypeID(type_id)
    }
}

#[derive(Clone, Debug)]
pub struct ContractsInfo {
    pub contract_default_scripts: HashMap<Contract, Script>,
    pub script_cell_deps: HashMap<Contract, Vec<ScriptCellDep>>,
    pub udt_whitelist: UdtCfgInfos,
}

#[derive(Clone, Debug)]
pub struct TypeIDResolver {
    ckb_url: String,
}

impl TypeIDResolver {
    pub fn new(ckb_url: String) -> Self {
        Self { ckb_url }
    }

    pub fn resolve(&self, type_id: Script) -> Option<CellDep> {
        let ckb_client = CkbRpcClient::new(&self.ckb_url);
        let search_key = SearchKey {
            script: type_id.into(),
            script_type: ScriptType::Type,
            script_search_mode: Some(SearchMode::Exact),
            filter: None,
            with_data: Some(false),
            group_by_transaction: Some(false),
        };
        let cells = ckb_client
            .get_cells(search_key, Order::Desc, 1u32.into(), None)
            .ok()?;
        let cell = cells.objects.first()?;
        let cell_dep = CellDep::new_builder()
            .out_point(cell.out_point.clone().into())
            .dep_type(DepType::Code.into())
            .build();
        Some(cell_dep)
    }
}

#[derive(Clone, Debug)]
pub struct ContractsContext {
    pub contracts: ContractsInfo,
    pub type_id_resolver: Option<TypeIDResolver>,
}

#[derive(Debug, Error)]
pub enum ContractsContextError {
    #[error("Context already initialized")]
    ContextAlreadyInitialized,

    #[error("Genesis block transaction #{0} should exist")]
    GenesisBlockTransactionNotFound(usize),

    #[error("Genesis block transaction #0 output #{0} should exist")]
    GenesisBlockTransaction0OutputNotFound(usize),

    #[error("Genesis block secp256k1 binary cell type script should exist")]
    GenesisBlockSecp256k1BinaryCellTypeScriptNotFound,

    #[error("Cannot resolve cell dep for type id {0}")]
    CannotResolveCellDep(Script),

    #[error("Cannot resolve udt info for {0}")]
    CannotResolveUdtInfo(Script),
}

impl ContractsContext {
    pub fn try_new(
        genesis_block: BlockView,
        fiber_scripts: Vec<FiberScript>,
        udt_whitelist: UdtCfgInfos,
        type_id_resolver: Option<TypeIDResolver>,
    ) -> Result<Self, ContractsContextError> {
        let mut contract_default_scripts: HashMap<Contract, Script> = HashMap::new();
        let mut script_cell_deps: HashMap<Contract, Vec<ScriptCellDep>> = HashMap::new();

        let genesis_tx = genesis_block
            .transaction(0)
            .ok_or(ContractsContextError::GenesisBlockTransactionNotFound(0))?;

        // setup secp256k1
        let secp256k1_binary_cell = genesis_tx
            .output(1)
            .ok_or(ContractsContextError::GenesisBlockTransaction0OutputNotFound(1))?;
        let secp256k1_binary_cell_type_script = secp256k1_binary_cell
            .type_()
            .to_opt()
            .ok_or(ContractsContextError::GenesisBlockSecp256k1BinaryCellTypeScriptNotFound)?;
        contract_default_scripts.insert(
            Contract::Secp256k1Lock,
            Script::new_builder()
                .code_hash(secp256k1_binary_cell_type_script.calc_script_hash())
                .hash_type(ScriptHashType::Type.into())
                .build(),
        );

        let secp256k1_dep_group_tx_hash = genesis_block
            .transaction(1)
            .ok_or(ContractsContextError::GenesisBlockTransactionNotFound(1))?
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
                .build()
                .into()],
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
                info!("Creating ContractsContext for dev");
                // index from 5 ~ 8 are the default contracts: CkbAuth, FundingLock, CommitmentLock, SimpleUDT
                let ckb_auth_cell_dep = CellDep::new_builder()
                    .out_point(
                        OutPoint::new_builder()
                            .tx_hash(genesis_tx.hash())
                            .index(5u32.pack())
                            .build(),
                    )
                    .dep_type(DepType::Code.into())
                    .build();
                script_cell_deps.insert(Contract::CkbAuth, vec![ckb_auth_cell_dep.clone().into()]);

                let contract_map = [
                    (Contract::FundingLock, 6u32),
                    (Contract::CommitmentLock, 7u32),
                    (Contract::SimpleUDT, 8u32),
                ];
                for (contract, index) in contract_map.into_iter() {
                    let cell_dep = CellDep::new_builder()
                        .out_point(
                            OutPoint::new_builder()
                                .tx_hash(genesis_tx.hash())
                                .index(index.pack())
                                .build(),
                        )
                        .dep_type(DepType::Code.into())
                        .build();
                    let output_data = genesis_tx
                        .outputs_data()
                        .get(index as usize)
                        .ok_or(
                            ContractsContextError::GenesisBlockTransaction0OutputNotFound(
                                index as usize,
                            ),
                        )?
                        .raw_data();
                    let cell_deps =
                        if matches!(contract, Contract::FundingLock | Contract::CommitmentLock) {
                            vec![cell_dep.into(), ckb_auth_cell_dep.clone().into()]
                        } else {
                            vec![cell_dep.into()]
                        };
                    script_cell_deps.insert(contract, cell_deps);
                    contract_default_scripts.insert(
                        contract,
                        Script::new_builder()
                            .code_hash(CellOutput::calc_data_hash(&output_data))
                            .hash_type(ScriptHashType::Data1.into())
                            .build(),
                    );
                }
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
            script_cell_deps.insert(
                name,
                cell_deps.into_iter().map(ScriptCellDep::from).collect(),
            );
        }

        // ensure all type id cell deps are resolvable
        for cell_dep in script_cell_deps.values().flatten() {
            if let ScriptCellDep::TypeID(type_id) = cell_dep {
                let err = || ContractsContextError::CannotResolveCellDep(type_id.clone());
                let resolver = type_id_resolver.as_ref().ok_or_else(err)?;
                resolver.resolve(type_id.clone()).ok_or_else(err)?;
            }
        }

        for cell_dep in udt_whitelist
            .0
            .iter()
            .flat_map(|info| info.cell_deps.clone())
        {
            if let Some(type_id) = cell_dep.type_id {
                let err = || ContractsContextError::CannotResolveCellDep(type_id.clone().into());
                let resolver = type_id_resolver.as_ref().ok_or_else(err)?;
                resolver.resolve(type_id.clone().into()).ok_or_else(err)?;
            }
        }

        Ok(Self {
            contracts: ContractsInfo {
                contract_default_scripts,
                script_cell_deps,
                udt_whitelist,
            },
            type_id_resolver,
        })
    }

    fn get_contracts_map(&self) -> &HashMap<Contract, Script> {
        &self.contracts.contract_default_scripts
    }

    pub(crate) fn get_cell_deps(
        &self,
        contracts: Vec<Contract>,
    ) -> Result<CellDepVec, ContractsContextError> {
        let mut builder: CellDepVecBuilder = CellDepVec::new_builder();
        for contract in contracts {
            if let Some(cell_deps) = self.contracts.script_cell_deps.get(&contract) {
                for cell_dep in cell_deps {
                    match cell_dep {
                        ScriptCellDep::CellDep(cell_dep) => {
                            builder = builder.push(cell_dep.clone());
                        }
                        ScriptCellDep::TypeID(type_id) => {
                            let err =
                                || ContractsContextError::CannotResolveCellDep(type_id.clone());
                            let resolver = self.type_id_resolver.as_ref().ok_or_else(err)?;
                            let cell_dep = resolver.resolve(type_id.clone()).ok_or_else(err)?;
                            builder = builder.push(cell_dep);
                        }
                    }
                }
            }
        }
        Ok(builder.build())
    }

    /// Used to calculate the transaction size and fee.
    pub(crate) fn get_cell_deps_count(&self, contracts: Vec<Contract>) -> usize {
        let mut count = 0;
        for contract in contracts {
            if let Some(cell_deps) = self.contracts.script_cell_deps.get(&contract) {
                count += cell_deps.len();
            }
        }
        count
    }

    pub(crate) fn get_udt_cell_deps(
        &self,
        udt_deps: Vec<UdtDep>,
    ) -> Result<CellDepVec, ContractsContextError> {
        let mut builder: CellDepVecBuilder = CellDepVec::new_builder();
        for udt_dep in &udt_deps {
            match udt_dep.to_enum() {
                UdtDepUnion::UdtCellDep(cell_dep) => {
                    let cell_dep = CellDep::new_builder()
                        .out_point(cell_dep.out_point())
                        .dep_type(cell_dep.dep_type())
                        .build();
                    builder = builder.push(cell_dep);
                }
                UdtDepUnion::Script(type_id) => {
                    let err = || ContractsContextError::CannotResolveCellDep(type_id.clone());
                    let resolver = self.type_id_resolver.as_ref().ok_or_else(err)?;
                    let type_id = Script::new_builder()
                        .code_hash(type_id.code_hash())
                        .hash_type(type_id.hash_type())
                        .args(type_id.args())
                        .build();
                    let cell_dep = resolver.resolve(type_id).ok_or_else(err)?;
                    builder = builder.push(cell_dep);
                }
            }
        }
        Ok(builder.build())
    }

    pub fn get_udt_whitelist(&self) -> &UdtCfgInfos {
        &self.contracts.udt_whitelist
    }

    pub(crate) fn get_script(&self, contract: Contract, args: &[u8]) -> Script {
        self.get_contracts_map()
            .get(&contract)
            .unwrap_or_else(|| panic!("Contract {:?} should exist", contract))
            .clone()
            .as_builder()
            .args(args.pack())
            .build()
    }

    pub(crate) fn get_udt_info(&self, udt_script: &Script) -> Option<&UdtArgInfo> {
        for udt in &self.get_udt_whitelist().0 {
            if let Ok(_type) = udt_script.hash_type().try_into() {
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
        }
        None
    }
}

pub static CONTRACTS_CONTEXT_INSTANCE: OnceCell<ContractsContext> = OnceCell::new();

pub fn try_init_contracts_context(
    genesis_block: BlockView,
    fiber_scripts: Vec<FiberScript>,
    udt_whitelist: UdtCfgInfos,
    type_id_resolver: Option<TypeIDResolver>,
) -> Result<(), ContractsContextError> {
    CONTRACTS_CONTEXT_INSTANCE
        .set(ContractsContext::try_new(
            genesis_block,
            fiber_scripts,
            udt_whitelist,
            type_id_resolver,
        )?)
        .map_err(|_| ContractsContextError::ContextAlreadyInitialized)
}

#[cfg(not(test))]
fn get_contracts_context() -> &'static ContractsContext {
    CONTRACTS_CONTEXT_INSTANCE
        .get()
        .expect("init_contracts_context should be called first")
}

#[cfg(test)]
fn get_contracts_context() -> ContractsContext {
    super::tests::test_utils::MOCK_CONTEXT
        .read()
        .expect("read mock context")
        .contracts_context
        .clone()
}

pub fn get_script_by_contract(contract: Contract, args: &[u8]) -> Script {
    get_contracts_context().get_script(contract, args)
}

pub fn get_cell_deps_by_contracts(
    contracts: Vec<Contract>,
) -> Result<CellDepVec, ContractsContextError> {
    get_contracts_context().get_cell_deps(contracts)
}

pub fn get_cell_deps_count_by_contracts(contracts: Vec<Contract>) -> usize {
    get_contracts_context().get_cell_deps_count(contracts)
}

fn get_udt_info(script: &Script) -> Option<UdtArgInfo> {
    get_contracts_context().get_udt_info(script).cloned()
}

pub fn check_udt_script(script: &Script) -> bool {
    get_udt_info(script).is_some()
}

pub fn resolve_udt_cell_deps(udt: &UdtArgInfo) -> Result<CellDepVec, ContractsContextError> {
    get_contracts_context()
        .get_udt_cell_deps(udt.cell_deps.iter().cloned().map(Into::into).collect())
}

pub fn get_udt_cell_deps(script: &Script) -> Result<CellDepVec, ContractsContextError> {
    let udt = get_udt_info(script)
        .ok_or_else(|| ContractsContextError::CannotResolveUdtInfo(script.clone()))?;
    resolve_udt_cell_deps(&udt)
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

pub fn get_cell_deps(
    contracts: Vec<Contract>,
    udt_script: &Option<Script>,
) -> Result<CellDepVec, ContractsContextError> {
    let cell_deps = get_cell_deps_by_contracts(contracts)?;
    if let Some(udt_script) = udt_script {
        if let Ok(udt_cell_deps) = get_udt_cell_deps(udt_script) {
            let res = cell_deps
                .into_iter()
                .chain(udt_cell_deps)
                .collect::<Vec<CellDep>>();
            return Ok(res.pack());
        }
    }
    Ok(cell_deps)
}

pub fn get_cell_deps_count(contracts: Vec<Contract>, udt_script: &Option<Script>) -> usize {
    let mut count = get_cell_deps_count_by_contracts(contracts);
    if let Some(udt_script) = udt_script {
        if let Some(udt) = get_udt_info(udt_script) {
            count += udt.cell_deps.len();
        }
    }
    count
}
