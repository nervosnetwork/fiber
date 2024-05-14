use ckb_testtool::{ckb_types::bytes::Bytes, context::Context};
use ckb_types::{
    packed::{CellDep, CellDepVec, OutPoint, Script},
    prelude::{Builder, Entity, PackVec},
};
use once_cell::sync::OnceCell;
use std::{
    collections::HashMap,
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};
use std::{env, fs, path::PathBuf};

use ckb_types::prelude::Pack;

fn load_contract_binary(base_dir: &str, contract: Contract) -> Bytes {
    let mut path = PathBuf::from(base_dir);
    path.push(PathBuf::from(contract.binary_name()));

    let result = fs::read(&path);
    if result.is_err() {
        panic!("Loading binary {:?} failed: {:?}", path, result.err());
    }
    result.unwrap().into()
}

pub(crate) struct MockContext {
    context: RwLock<Context>,
    contracts_context: ContractsContext,
}

impl MockContext {
    // This is used temporarily to test the functionality of the contract.
    pub fn get() -> &'static Self {
        static INSTANCE: OnceCell<MockContext> = OnceCell::new();
        INSTANCE.get_or_init(|| {
            let base_dir = env::var("TESTING_CONTRACTS_DIR")
                .expect("TESTING_CONTRACTS_DIR must be given to mock ckb context");

            let mut context = Context::default();

            let (map, cell_deps): (HashMap<Contract, _>, Vec<CellDep>) = [
                Contract::FundingLock,
                Contract::CommitmentLock,
                Contract::AlwaysSuccess,
                Contract::Secp256k1Lock,
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
            .unzip();

            let context = MockContext {
                context: RwLock::new(context),
                contracts_context: ContractsContext {
                    contracts: map,
                    cell_deps: cell_deps.pack(),
                },
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
}

impl Contract {
    fn binary_name(&self) -> &'static str {
        match self {
            Contract::FundingLock => "funding-lock",
            Contract::CommitmentLock => "commitment-lock",
            Contract::Secp256k1Lock => "auth",
            Contract::AlwaysSuccess => "always_success",
        }
    }
}

pub(crate) struct ContractsContext {
    contracts: HashMap<Contract, (OutPoint, Script)>,
    // TODO: We bundle all the cell deps together, but some of they are not always needed.
    cell_deps: CellDepVec,
}

pub(crate) enum CommitmentLockContext {
    Mock(&'static MockContext),
    Real(ContractsContext),
}

impl CommitmentLockContext {
    pub(crate) fn get() -> Self {
        Self::Mock(MockContext::get())
    }

    pub fn is_testing(&self) -> bool {
        true
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

    fn get_out_point(&self, contract: Contract) -> OutPoint {
        self.get_contracts_map().get(&contract).unwrap().0.clone()
    }

    fn get_script(&self, contract: Contract, args: &[u8]) -> Script {
        self.get_contracts_map()
            .get(&contract)
            .unwrap()
            .1
            .clone()
            .as_builder()
            .args(args.to_owned().pack())
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
