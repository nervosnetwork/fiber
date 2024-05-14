use ckb_testtool::{ckb_types::bytes::Bytes, context::Context};
use ckb_types::{
    core::{DepType, ScriptHashType},
    packed::{CellDep, CellDepVec, OutPoint, Script},
    prelude::{Builder, Entity, PackVec},
};
use log::{debug, warn};
use once_cell::sync::OnceCell;
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};
use std::{env, fs, path::PathBuf};

use ckb_types::prelude::Pack;

use super::{config::CkbNetwork, types::Hash256};

fn load_contract_binary(base_dir: &str, contract: Contract) -> Bytes {
    let mut path = PathBuf::from(base_dir);
    path.push(PathBuf::from(contract.binary_name()));

    let result = fs::read(&path);
    if result.is_err() {
        panic!("Loading binary {:?} failed: {:?}", path, result.err());
    }
    result.unwrap().into()
}

#[derive(Debug)]
pub struct MockContext {
    context: RwLock<Context>,
    contracts_context: Arc<ContractsContext>,
}

impl MockContext {
    // This is used temporarily to test the functionality of the contract.
    pub fn get() -> &'static Self {
        static INSTANCE: OnceCell<MockContext> = OnceCell::new();
        INSTANCE.get_or_init(|| {
            let mut context = Context::default();

            let (map, cell_deps) = match env::var("TESTING_CONTRACTS_DIR") {
                Ok(base_dir) => {
                    [
                        Contract::FundingLock,
                        Contract::CommitmentLock,
                        Contract::AlwaysSuccess,
                        Contract::Secp256k1Lock,
                        // These are contracts that we will call from other contracts, e.g. funding-lock.
                        Contract::CkbAuth,
                        Contract::SimpleUDT,
                    ]
                    .iter()
                    .map(|contract| {
                        let binary = load_contract_binary(base_dir.as_str(), *contract);
                        let out_point = context.deploy_cell(binary);
                        let script = context
                            .build_script(&out_point, Bytes::new())
                            .expect("Build script");
                        let cell_dep = CellDep::new_builder().out_point(out_point.clone()).build();
                        ((*contract, (out_point, script)), cell_dep)
                    })
                    .unzip()
                }
                Err(e) => {
                    warn!(
                        "TESTING_CONTRACTS_DIR is not set, using default contracts: {:?}",
                        e
                    );
                    (HashMap::new(), vec![])
                }
            };
            let cell_dep_vec = cell_deps.pack();
            debug!("Loaded contracts into the mock environement: {:?}", &map);
            debug!("Use this contracts with CellDepVec: {:?}", &cell_dep_vec);

            let context = MockContext {
                context: RwLock::new(context),
                contracts_context: Arc::new(ContractsContext {
                    contracts: map,
                    cell_deps: cell_dep_vec,
                }),
            };
            context
        });
        INSTANCE.get().unwrap()
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
enum Contract {
    FundingLock,
    CommitmentLock,
    Secp256k1Lock,
    AlwaysSuccess,
    CkbAuth,
    SimpleUDT,
}

impl Contract {
    fn binary_name(&self) -> &'static str {
        match self {
            Contract::FundingLock => "funding-lock",
            Contract::CommitmentLock => "commitment-lock",
            // TODO fix me.
            // We create an secp256k1 binary here because we need to find out the tx of secp256k1 in dev chain.
            Contract::Secp256k1Lock => "always_success",
            Contract::AlwaysSuccess => "always_success",
            Contract::CkbAuth => "auth",
            Contract::SimpleUDT => "simple_udt",
        }
    }
}

#[derive(Debug)]
pub struct ContractsContext {
    contracts: HashMap<Contract, (OutPoint, Script)>,
    // TODO: We bundle all the cell deps together, but some of they are not always needed.
    cell_deps: CellDepVec,
}

#[derive(Clone, Debug)]
pub enum CommitmentLockContext {
    Mock(&'static MockContext),
    Real(Arc<ContractsContext>),
}

enum EnvironmentVariableType {
    CodeHash,
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

impl CommitmentLockContext {
    pub fn new(network: CkbNetwork) -> Self {
        match network {
            CkbNetwork::Mocknet => Self::Mock(MockContext::get()),
            CkbNetwork::Dev => {
                let mut map = HashMap::new();
                let mut cell_deps = vec![];
                for (dep_type, contracts) in [
                    (DepType::Code, vec![Contract::AlwaysSuccess]),
                    (
                        DepType::DepGroup,
                        vec![Contract::FundingLock, Contract::CommitmentLock],
                    ),
                ] {
                    for contract in contracts {
                        let type_hash = get_hash_from_environment_variable(
                            contract,
                            EnvironmentVariableType::CodeHash,
                            dep_type,
                        );
                        let tx = get_hash_from_environment_variable(
                            contract,
                            EnvironmentVariableType::TxHash,
                            dep_type,
                        );
                        let index: usize = get_environment_variable(
                            contract,
                            EnvironmentVariableType::TxIndex,
                            dep_type,
                        )
                        .parse()
                        .expect("Valid index");
                        let out_point = OutPoint::new_builder()
                            .tx_hash(tx.into())
                            .index(index.pack())
                            .build();
                        let script = Script::new_builder()
                            .code_hash(type_hash.into())
                            .hash_type(ScriptHashType::Data2.into())
                            .args(Bytes::new().pack())
                            .build();
                        map.insert(contract, (out_point.clone(), script));
                        cell_deps.push(
                            CellDep::new_builder()
                                .out_point(out_point)
                                .dep_type(dep_type.into())
                                .build(),
                        );
                    }
                }
                let tx0_env_name = format!("{ENV_PREFIX}_CKB_GENESIS_TX_0");
                let tx0 = Hash256::from_str(
                    &env::var(&tx0_env_name)
                        .expect(&format!("environment variable {tx0_env_name}")),
                )
                .expect("valid hash");
                let tx1_env_name = format!("{ENV_PREFIX}_CKB_GENESIS_TX_1");
                let tx1 = Hash256::from_str(
                    &env::var(&tx1_env_name)
                        .expect(&format!("environment variable {tx1_env_name}")),
                )
                .expect("valid hash");

                let secp256k1_outpoint = OutPoint::new_builder()
                    .tx_hash(tx0.into())
                    .index(1u32.pack())
                    .build();
                let secp256k1_script = Script::new_builder()
                    .code_hash(
                        Hash256::from_str(
                            "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
                        )
                        .expect("valid hash")
                        .into(),
                    )
                    .hash_type(ScriptHashType::Type.into())
                    .args(Bytes::new().pack())
                    .build();
                map.insert(
                    Contract::Secp256k1Lock,
                    (secp256k1_outpoint, secp256k1_script),
                );
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

                debug!("Loaded contracts into the real environement: {:?}", &map);
                debug!("Use this contracts with CellDepVec: {:?}", &cell_deps);
                Self::Real(Arc::new(ContractsContext {
                    contracts: map,
                    cell_deps: cell_deps.pack(),
                }))
            }
            _ => panic!("Unsupported network type {:?}", network),
        }
    }

    pub fn get_mock() -> Self {
        Self::Mock(MockContext::get())
    }

    pub fn is_testing(&self) -> bool {
        match self {
            Self::Mock(_) => true,
            Self::Real(_) => false,
        }
    }

    fn get_contracts_map(&self) -> &HashMap<Contract, (OutPoint, Script)> {
        match self {
            Self::Mock(mock) => &mock.contracts_context.contracts,
            Self::Real(real) => &real.contracts,
        }
    }
    fn get_cell_deps(&self) -> &CellDepVec {
        match self {
            Self::Mock(mock) => &mock.contracts_context.cell_deps,
            Self::Real(real) => &real.cell_deps,
        }
    }

    fn get_contract_info(&self, contract: Contract) -> (OutPoint, Script) {
        self.get_contracts_map()
            .get(&contract)
            .expect(format!("Contract {:?} exists", contract).as_str())
            .clone()
    }

    fn get_out_point(&self, contract: Contract) -> OutPoint {
        self.get_contract_info(contract).0
    }

    fn get_script(&self, contract: Contract, args: &[u8]) -> Script {
        self.get_contract_info(contract)
            .1
            .as_builder()
            .args(args.pack())
            .build()
    }

    pub fn read_mock_context(&self) -> RwLockReadGuard<Context> {
        match &self {
            Self::Mock(mock) => mock.context.read().unwrap(),
            Self::Real(_real) => panic!("Real context is not readable"),
        }
    }

    pub fn write_mock_context(&self) -> RwLockWriteGuard<Context> {
        match &self {
            Self::Mock(mock) => mock.context.write().unwrap(),
            Self::Real(_real) => panic!("Real context is not writable"),
        }
    }

    pub fn get_commitment_lock_outpoint(&self) -> OutPoint {
        self.get_out_point(Contract::CommitmentLock)
    }

    pub fn get_secp256k1_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::Secp256k1Lock, args)
    }

    pub fn get_funding_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::CommitmentLock, args)
    }

    pub fn get_commitment_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::CommitmentLock, args)
    }

    pub fn get_commitment_transaction_cell_deps(&self) -> CellDepVec {
        self.get_cell_deps().clone()
    }

    pub fn get_always_success_outpoint(&self) -> OutPoint {
        self.get_out_point(Contract::AlwaysSuccess)
    }

    pub fn get_always_success_script(&self, args: &[u8]) -> Script {
        self.get_script(Contract::AlwaysSuccess, args)
    }
}
