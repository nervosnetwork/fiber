use ckb_types::{
    core::{DepType, ScriptHashType},
    packed::{CellDep, CellDepVec, OutPoint, Script},
    prelude::{Builder, Entity, Pack, PackVec},
};
use log::debug;
use once_cell::sync::OnceCell;
use std::{collections::HashMap, env, str::FromStr, sync::Arc};

use super::{config::CkbNetwork, types::Hash256};

#[cfg(not(test))]
use ckb_types::bytes::Bytes;

#[cfg(test)]
use ckb_testtool::{ckb_types::bytes::Bytes, context::Context};
#[cfg(test)]
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(test)]
#[derive(Clone, Debug)]
pub struct MockContext {
    context: Arc<RwLock<Context>>,
    contracts_context: Arc<ContractsContext>,
}

#[cfg(test)]
impl MockContext {
    // If we are using cfg(test), then directly including contracts binaries into the
    // resulting executable is not a problem. Otherwise, we'd better read the binaries from
    // the filesystem.
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

        let (map, cell_deps) = Self::get_contract_binaries().into_iter().fold(
            (HashMap::new(), vec![]),
            |(mut map, mut cell_deps), (contract, binary)| {
                let out_point = context.deploy_cell(binary);
                let script = context
                    .build_script(&out_point, Default::default())
                    .expect("valid script");
                map.insert(contract, script);
                cell_deps.push(
                    CellDep::new_builder()
                        .out_point(out_point)
                        .dep_type(DepType::Code.into())
                        .build(),
                );
                (map, cell_deps)
            },
        );
        let cell_dep_vec = cell_deps.pack();
        debug!("Loaded contracts into the mock environement: {:?}", &map);
        debug!(
            "Use these contracts by specifying cell deps to {:?}",
            &cell_dep_vec
        );

        let context = MockContext {
            context: Arc::new(RwLock::new(context)),
            contracts_context: Arc::new(ContractsContext {
                contract_default_scripts: map,
                cell_deps: cell_dep_vec,
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
enum Contract {
    FundingLock,
    CommitmentLock,
    Secp256k1Lock,
    AlwaysSuccess,
    CkbAuth,
    #[allow(dead_code)]
    SimpleUDT,
}

#[derive(Debug)]
pub struct ContractsContext {
    contract_default_scripts: HashMap<Contract, Script>,
    // TODO: We bundle all the cell deps together, but some of they are not always needed.
    cell_deps: CellDepVec,
}

#[derive(Clone, Debug)]
pub enum CommitmentLockContext {
    #[cfg(test)]
    Mock(MockContext),
    Real(Arc<ContractsContext>),
}

enum EnvironmentVariableType {
    CodeHash,
    // FIXME(yukang): warning suppression
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

const ENV_PREFIX: &'static str = "NEXT_PUBLIC";
const DEFUALT_SECP256K1_TYPE_HASH: &'static str =
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
    std::env::var(&env).expect(
        format!(
            "Environment variable {} for contract {:?}",
            env.as_str(),
            contract
        )
        .as_str(),
    )
}

static COMMITMENT_LOCK_CTX_INSTANCE: OnceCell<CommitmentLockContext> = OnceCell::new();

impl CommitmentLockContext {
    // TODO: better way to organize this? Currently CommitmentLockContext is a singleton
    // because it is used in so many places.
    pub fn initialize(network: CkbNetwork) -> &'static Self {
        COMMITMENT_LOCK_CTX_INSTANCE.get_or_init(|| match network {
            #[cfg(test)]
            CkbNetwork::Mocknet => {
                log::warn!("Initializing mock context for testing.");
                Self::Mock(MockContext::new())
            }
            CkbNetwork::Dev => {
                let mut map = HashMap::new();
                let mut cell_deps = vec![];
                for (program_dep_type, group_dep_type, contracts) in [
                    (DepType::Code, DepType::Code, vec![Contract::AlwaysSuccess]),
                    (
                        DepType::Code,
                        DepType::DepGroup,
                        vec![Contract::FundingLock, Contract::CommitmentLock],
                    ),
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
                        map.insert(contract, script);
                        cell_deps.push(
                            CellDep::new_builder()
                                .out_point(dep_group_out_point)
                                .dep_type(group_dep_type.into())
                                .build(),
                        );
                    }
                }
                let tx1_env_name = format!("{ENV_PREFIX}_CKB_GENESIS_TX_1");
                let tx1 = Hash256::from_str(
                    &env::var(&tx1_env_name)
                        .expect(&format!("environment variable {tx1_env_name}")),
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
                cell_deps.push(
                    CellDep::new_builder()
                        .out_point(
                            OutPoint::new_builder()
                                .tx_hash(tx1.into())
                                .index(0u32.pack())
                                .build(),
                        )
                        .dep_type(DepType::DepGroup.into())
                        .build(),
                );

                let cell_dep_vec = cell_deps.pack();
                debug!("Loaded contracts into the real environement: {:?}", &map);
                debug!(
                    "Use these contracts by specifying cell deps to {:?}",
                    &cell_dep_vec
                );
                Self::Real(Arc::new(ContractsContext {
                    contract_default_scripts: map,
                    cell_deps: cell_dep_vec,
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
                let cell_dep_vec = [
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
                        "0xbfd6d68b328a02606f1f65ee0f79f8ed5f76dfe86998c7aaa9ee4720d53f4c49",
                        2,
                        DepType::Code,
                    ),
                ]
                .into_iter()
                .map(|(_contract, tx, index, dep_type)| {
                    CellDep::new_builder()
                        .out_point(
                            OutPoint::new_builder()
                                .tx_hash(Hash256::from_str(tx).unwrap().into())
                                .index(index.pack())
                                .build(),
                        )
                        .dep_type(dep_type.into())
                        .build()
                })
                .pack();
                Self::Real(Arc::new(ContractsContext {
                    contract_default_scripts: map,
                    cell_deps: cell_dep_vec.pack(),
                }))
            }
            _ => panic!("Unsupported network type {:?}", network),
        });
        COMMITMENT_LOCK_CTX_INSTANCE.get().unwrap()
    }

    pub fn get() -> &'static Self {
        COMMITMENT_LOCK_CTX_INSTANCE.get().unwrap()
    }

    fn get_contracts_map(&self) -> &HashMap<Contract, Script> {
        match self {
            #[cfg(test)]
            Self::Mock(mock) => &mock.contracts_context.contract_default_scripts,
            Self::Real(real) => &real.contract_default_scripts,
        }
    }
    fn get_cell_deps(&self) -> &CellDepVec {
        match self {
            #[cfg(test)]
            Self::Mock(mock) => &mock.contracts_context.cell_deps,
            Self::Real(real) => &real.cell_deps,
        }
    }

    fn get_script(&self, contract: Contract, args: &[u8]) -> Script {
        self.get_contracts_map()
            .get(&contract)
            .expect(format!("Contract {:?} exists", contract).as_str())
            .clone()
            .as_builder()
            .args(args.pack())
            .build()
    }

    pub fn get_secp256k1_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::Secp256k1Lock, args)
    }

    pub fn get_funding_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::FundingLock, args)
    }

    pub fn get_commitment_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::CommitmentLock, args)
    }

    pub fn get_commitment_transaction_cell_deps(&self) -> CellDepVec {
        self.get_cell_deps().clone()
    }

    pub fn get_always_success_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::AlwaysSuccess, args)
    }
}
