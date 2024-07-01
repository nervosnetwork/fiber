use crate::debug;
use ckb_types::{
    core::{DepType, ScriptHashType},
    packed::{CellDep, CellDepVec, CellDepVecBuilder, OutPoint, Script},
    prelude::{Builder, Entity, Pack, PackVec},
};
use regex::Regex;
use std::{collections::HashMap, env, str::FromStr, sync::Arc};

use crate::ckb::{config::CkbNetwork, types::Hash256};

#[cfg(not(test))]
use ckb_types::bytes::Bytes;

#[cfg(test)]
use ckb_testtool::{ckb_types::bytes::Bytes, context::Context};
#[cfg(test)]
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{
    config::{UdtArgInfo, UdtCfgInfos},
    CkbChainConfig,
};

#[cfg(test)]
#[derive(Clone, Debug)]
pub struct MockContext {
    context: Arc<RwLock<Context>>,
    contracts_context: Arc<ContractsInfo>,
}

#[cfg(test)]
impl Default for MockContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl MockContext {
    fn get_contract_binaries() -> Vec<(Contract, Bytes)> {
        [
            (
                Contract::FundingLock,
                Bytes::from_static(include_bytes!("../../tests/deploy/contracts/funding-lock")),
            ),
            (
                Contract::CommitmentLock,
                Bytes::from_static(include_bytes!(
                    "../../tests/deploy/contracts/commitment-lock"
                )),
            ),
            (
                Contract::AlwaysSuccess,
                Bytes::from_static(include_bytes!(
                    "../../tests/deploy/contracts/always_success"
                )),
            ),
            (
                Contract::Secp256k1Lock,
                Bytes::from_static(include_bytes!(
                    "../../tests/deploy/contracts/always_success"
                )),
            ),
            (
                Contract::CkbAuth,
                Bytes::from_static(include_bytes!("../../tests/deploy/contracts/auth")),
            ),
            (
                Contract::SimpleUDT,
                Bytes::from_static(include_bytes!("../../tests/deploy/contracts/simple_udt")),
            ),
        ]
        .into()
    }

    pub fn new() -> Self {
        let mut context = Context::default();

        let (map, script_cell_deps) = Self::get_contract_binaries().into_iter().enumerate().fold(
            (HashMap::new(), HashMap::new()),
            |(mut map, mut script_cell_deps), (i, (contract, binary))| {
                use ckb_hash::blake2b_256;
                use rand::{rngs::StdRng, SeedableRng};
                let i = i + 123_456_789;
                let seed = blake2b_256(i.to_le_bytes());
                let mut rng = StdRng::from_seed(seed);
                // Use a deterministic RNG to ensure that the outpoints are the same for all nodes.
                // Otherwise, cell deps may differ for different nodes, which would make
                // different nodes sign different messages (as transaction hashes differ).
                let out_point = context.deploy_cell_with_rng(binary, &mut rng);
                let script = context
                    .build_script(&out_point, Default::default())
                    .expect("valid script");
                map.insert(contract, script);
                let cell_dep = CellDep::new_builder()
                    .out_point(out_point)
                    .dep_type(DepType::Code.into())
                    .build();

                script_cell_deps.insert(contract, CellDepVec::new_builder().push(cell_dep).build());
                (map, script_cell_deps)
            },
        );
        debug!("Loaded contracts into the mock environement: {:?}", &map);

        let context = MockContext {
            context: Arc::new(RwLock::new(context)),
            contracts_context: Arc::new(ContractsInfo {
                contract_default_scripts: map,
                script_cell_deps,
                udt_whitelist: UdtCfgInfos::default(),
            }),
        };
        debug!("Created mock context to test transactions.");
        context
    }

    pub fn write(&self) -> RwLockWriteGuard<Context> {
        self.context.write().unwrap()
    }

    pub fn read(&self) -> RwLockReadGuard<Context> {
        self.context.read().unwrap()
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Contract {
    FundingLock,
    CommitmentLock,
    Secp256k1Lock,
    AlwaysSuccess,
    CkbAuth,
    SimpleUDT,
}

#[derive(Clone, Debug)]
struct ContractsInfo {
    contract_default_scripts: HashMap<Contract, Script>,
    script_cell_deps: HashMap<Contract, CellDepVec>,
    udt_whitelist: UdtCfgInfos,
}

#[derive(Clone)]
pub enum ContractsContext {
    #[cfg(test)]
    Mock(MockContext),
    #[allow(private_interfaces)]
    Real(Arc<ContractsInfo>),
}

impl std::fmt::Debug for ContractsContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(test)]
            Self::Mock(_mock) => write!(f, "Mock"),
            Self::Real(_real) => write!(f, "Real"),
        }
    }
}

enum EnvironmentVariableType {
    CodeHash,
    #[allow(dead_code)]
    TypeHash,
    TxIndex,
    TxHash,
}

fn get_hash_from_environment_variable(
    contract: Contract,
    env_type: EnvironmentVariableType,
    dep_type: DepType,
) -> Hash256 {
    let string = get_environment_variable(contract, env_type, dep_type);
    if string.len() < 2 || &string[..2].to_lowercase() != "0x" {
        panic!("hex string should start with 0x");
    };
    <[u8; 32]>::try_from(hex::decode(&string[2..]).expect("valid hex").as_slice())
        .expect("valid hash")
        .into()
}

const ENV_PREFIX: &str = "NEXT_PUBLIC";
const DEFUALT_SECP256K1_TYPE_HASH: &str =
    "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8";

fn get_environment_variable(
    contract: Contract,
    env_type: EnvironmentVariableType,
    dep_type: DepType,
) -> String {
    let contract_name = match contract {
        Contract::FundingLock => "FUNDING_LOCK",
        Contract::CommitmentLock => "COMMITMENT_LOCK",
        Contract::AlwaysSuccess => "ALWAYS_SUCCESS",
        Contract::SimpleUDT => "SIMPLE_UDT",
        _ => panic!("Unsupported contract type {:?}", contract),
    };
    let type_desc = match env_type {
        EnvironmentVariableType::CodeHash => "CODE_HASH",
        EnvironmentVariableType::TypeHash => "TYPE_HASH",
        EnvironmentVariableType::TxIndex => "TX_INDEX",
        EnvironmentVariableType::TxHash => "TX_HASH",
    };
    let maybe_dep_group = if dep_type == DepType::Code {
        ""
    } else {
        "_DEP_GROUP"
    };
    let env = format!("{ENV_PREFIX}_{contract_name}{maybe_dep_group}_{type_desc}");
    std::env::var(&env).unwrap_or_else(|_| {
        panic!(
            "Environment variable {} for contract {:?}",
            env.as_str(),
            contract
        )
    })
}

#[cfg(test)]
impl From<MockContext> for ContractsContext {
    fn from(mock: MockContext) -> Self {
        Self::Mock(mock)
    }
}

impl ContractsContext {
    pub fn new(network: CkbNetwork, udt_whitelist: UdtCfgInfos) -> Self {
        match network {
            #[cfg(test)]
            CkbNetwork::Mocknet => {
                crate::warn!("Initializing mock context for testing.");
                MockContext::new().into()
            }
            CkbNetwork::Dev => {
                let mut map = HashMap::new();
                let mut script_cell_deps = HashMap::new();
                for (program_dep_type, group_dep_type, contracts) in [
                    (DepType::Code, DepType::Code, vec![Contract::AlwaysSuccess]),
                    (
                        DepType::Code,
                        DepType::DepGroup,
                        vec![Contract::FundingLock, Contract::CommitmentLock],
                    ),
                    (DepType::Code, DepType::Code, vec![Contract::SimpleUDT]),
                ] {
                    for contract in contracts {
                        let program_code_hash = get_hash_from_environment_variable(
                            contract,
                            EnvironmentVariableType::CodeHash,
                            program_dep_type,
                        );
                        let group_tx = get_hash_from_environment_variable(
                            contract,
                            EnvironmentVariableType::TxHash,
                            group_dep_type,
                        );
                        let group_index: usize = get_environment_variable(
                            contract,
                            EnvironmentVariableType::TxIndex,
                            group_dep_type,
                        )
                        .parse()
                        .expect("Valid index");
                        let dep_group_out_point = OutPoint::new_builder()
                            .tx_hash(group_tx.into())
                            .index(group_index.pack())
                            .build();
                        let script = Script::new_builder()
                            .code_hash(program_code_hash.into())
                            .hash_type(ScriptHashType::Data1.into())
                            .args(Bytes::new().pack())
                            .build();
                        let cell_dep = CellDep::new_builder()
                            .out_point(dep_group_out_point.clone())
                            .dep_type(group_dep_type.into())
                            .build();
                        map.insert(contract, script);
                        script_cell_deps.insert(
                            contract,
                            CellDepVec::new_builder().push(cell_dep).build().pack(),
                        );
                    }
                }
                let tx1_env_name = format!("{ENV_PREFIX}_CKB_GENESIS_TX_1");
                let tx1 = Hash256::from_str(
                    &env::var(&tx1_env_name)
                        .unwrap_or_else(|_| panic!("environment variable {tx1_env_name}")),
                )
                .expect("valid hash");

                let secp256k1_script = Script::new_builder()
                    .code_hash(
                        Hash256::from_str(DEFUALT_SECP256K1_TYPE_HASH)
                            .expect("valid hash")
                            .into(),
                    )
                    .hash_type(ScriptHashType::Type.into())
                    .args(Bytes::new().pack())
                    .build();
                map.insert(Contract::Secp256k1Lock, secp256k1_script);
                let cell_dep = CellDep::new_builder()
                    .out_point(
                        OutPoint::new_builder()
                            .tx_hash(tx1.into())
                            .index(0u32.pack())
                            .build(),
                    )
                    .dep_type(DepType::DepGroup.into())
                    .build();
                script_cell_deps.insert(
                    Contract::Secp256k1Lock,
                    CellDepVec::new_builder().push(cell_dep).build().pack(),
                );

                debug!("Loaded contracts into the real environement: {:?}", &map);
                Self::Real(Arc::new(ContractsInfo {
                    contract_default_scripts: map,
                    script_cell_deps,
                    udt_whitelist,
                }))
            }
            CkbNetwork::Testnet => {
                let map = [
                    (
                        Contract::FundingLock,
                        "0x6c67887fe201ee0c7853f1682c0b77c0e6214044c156c7558269390a8afa6d7c",
                    ),
                    (
                        Contract::CommitmentLock,
                        "0x740dee83f87c6f309824d8fd3fbdd3c8380ee6fc9acc90b1a748438afcdf81d8",
                    ),
                    (Contract::Secp256k1Lock, DEFUALT_SECP256K1_TYPE_HASH),
                ]
                .into_iter()
                .map(|(contract, type_hash)| {
                    (
                        contract,
                        Script::new_builder()
                            .code_hash(Hash256::from_str(type_hash).expect("valid hash").into())
                            .hash_type(ScriptHashType::Type.into())
                            .args(Bytes::new().pack())
                            .build(),
                    )
                })
                .collect();
                let script_cell_deps = [
                    (
                        Contract::Secp256k1Lock,
                        "0xf8de3bb47d055cdf460d93a2a6e1b05f7432f9777c8c474abf4eec1d4aee5d37",
                        0,
                        DepType::DepGroup,
                    ),
                    (
                        Contract::CkbAuth,
                        "0xbfd6d68b328a02606f1f65ee0f79f8ed5f76dfe86998c7aaa9ee4720d53f4c49",
                        0u32,
                        DepType::Code,
                    ),
                    (
                        Contract::FundingLock,
                        "0xbfd6d68b328a02606f1f65ee0f79f8ed5f76dfe86998c7aaa9ee4720d53f4c49",
                        1,
                        DepType::Code,
                    ),
                    (
                        Contract::CommitmentLock,
                        "0xd33bd0d7d6e3b087ffdafa0dd769b7a4caa91e28ce87cd04ee1aa1814c5e460e",
                        0,
                        DepType::Code,
                    ),
                ]
                .into_iter()
                .map(|(contract, tx, index, dep_type)| {
                    let cell_dep = CellDep::new_builder()
                        .out_point(
                            OutPoint::new_builder()
                                .tx_hash(Hash256::from_str(tx).unwrap().into())
                                .index(index.pack())
                                .build(),
                        )
                        .dep_type(dep_type.into())
                        .build();
                    (
                        contract,
                        CellDepVec::new_builder().push(cell_dep).build().pack(),
                    )
                })
                .collect();

                Self::Real(Arc::new(ContractsInfo {
                    contract_default_scripts: map,
                    script_cell_deps,
                    udt_whitelist,
                }))
            }
            _ => panic!("Unsupported network type {:?}", network),
        }
    }

    fn get_contracts_map(&self) -> &HashMap<Contract, Script> {
        match self {
            #[cfg(test)]
            Self::Mock(mock) => &mock.contracts_context.contract_default_scripts,
            Self::Real(real) => &real.contract_default_scripts,
        }
    }

    pub(crate) fn get_cell_deps(&self, contracts: Vec<Contract>) -> CellDepVec {
        let (script_cell_deps, contracts) = match self {
            #[cfg(test)]
            Self::Mock(mock) => {
                // ckb-testtool need to include CkbAuth
                let mut contracts = contracts;
                contracts.push(Contract::CkbAuth);
                (&mock.contracts_context.script_cell_deps, contracts)
            }
            Self::Real(real) => (&real.script_cell_deps, contracts),
        };

        let cell_deps_vec = contracts
            .into_iter()
            .map(|contract| {
                script_cell_deps
                    .get(&contract)
                    .unwrap_or_else(|| {
                        panic!("Cell dep for contract {:?} does not exists", contract)
                    })
                    .clone()
            })
            .collect::<Vec<CellDepVec>>();
        let mut res: CellDepVecBuilder = CellDepVec::new_builder();
        for cell_dep in cell_deps_vec.into_iter().flatten() {
            res = res.push(cell_dep);
        }
        res.build()
    }

    fn get_udt_whitelist(&self) -> &UdtCfgInfos {
        match self {
            #[cfg(test)]
            Self::Mock(mock) => &mock.contracts_context.udt_whitelist,
            Self::Real(real) => &real.udt_whitelist,
        }
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
                let pattern = Regex::new(&udt.script.args).expect("invalid expressio");
                if pattern.is_match(&args) {
                    return Some(udt);
                }
            }
        }
        None
    }
}

pub fn init_contracts_context(
    network: Option<CkbNetwork>,
    ckb_chain_config: Option<&CkbChainConfig>,
) -> &'static ContractsContext {
    static INSTANCE: once_cell::sync::OnceCell<ContractsContext> = once_cell::sync::OnceCell::new();
    let udt_whitelist = ckb_chain_config
        .map(|config| config.udt_whitelist.clone())
        .unwrap_or_default();
    INSTANCE.get_or_init(|| {
        ContractsContext::new(
            network.unwrap_or(DEFAULT_CONTRACT_NETWORK),
            udt_whitelist.unwrap_or_default(),
        )
    });
    INSTANCE.get().unwrap()
}

#[cfg(test)]
const DEFAULT_CONTRACT_NETWORK: CkbNetwork = CkbNetwork::Mocknet;
#[cfg(not(test))]
const DEFAULT_CONTRACT_NETWORK: CkbNetwork = CkbNetwork::Dev;

pub fn get_script_by_contract(contract: Contract, args: &[u8]) -> Script {
    init_contracts_context(None, None).get_script(contract, args)
}

pub fn get_cell_deps_by_contracts(contracts: Vec<Contract>) -> CellDepVec {
    init_contracts_context(None, None).get_cell_deps(contracts)
}

fn get_udt_info(script: &Script) -> Option<&UdtArgInfo> {
    init_contracts_context(None, None).get_udt_info(script)
}

pub fn check_udt_script(script: &Script) -> bool {
    get_udt_info(script).is_some()
}

pub fn get_udt_cell_deps(script: &Script) -> Option<CellDepVec> {
    get_udt_info(script).map(|udt| udt.cell_deps.clone())
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

#[cfg(test)]
mod test {
    use ckb_types::{core::TransactionView, packed::CellOutput, prelude::Pack};
    use molecule::prelude::{Builder, Entity};

    use crate::ckb_chain::contracts::{
        get_script_by_contract, Contract, ContractsContext, MockContext,
    };

    // This test is to ensure that the same transaction is generated for different mock contexts.
    // If different transactions are generated, then the mock context is not deterministic.
    // We can't use the mock context to verify the validity of the transactions.
    #[test]
    fn test_same_tx_for_different_mock_context() {
        let mock_ctx1 = MockContext::new();
        let mock_ctx2 = MockContext::new();
        let capacity = 100u64;
        let output = CellOutput::new_builder()
            .capacity(capacity.pack())
            .lock(get_script_by_contract(
                Contract::FundingLock,
                &b"whatever"[..],
            ))
            .build();
        assert_eq!(
            TransactionView::new_advanced_builder()
                .cell_deps(
                    ContractsContext::from(mock_ctx1).get_cell_deps(vec![Contract::FundingLock])
                )
                .output(output.clone())
                .output_data(Default::default())
                .build(),
            TransactionView::new_advanced_builder()
                .cell_deps(
                    ContractsContext::from(mock_ctx2).get_cell_deps(vec![Contract::FundingLock])
                )
                .output(output)
                .output_data(Default::default())
                .build()
        );
    }
}
