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

pub fn load_binary(base_dir: &str, name: &str) -> Bytes {
    let mut path = PathBuf::from(base_dir);
    path.push(PathBuf::from(name));

    let result = fs::read(&path);
    if result.is_err() {
        panic!("Loading binary {:?} failed: {:?}", path, result.err());
    }
    result.unwrap().into()
}

struct MockContext {
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

            let funding_lock_bin = load_binary(base_dir.as_str(), "funding-lock");
            let commitment_lock_bin = load_binary(base_dir.as_str(), "commitment-lock");
            let auth_bin = load_binary(base_dir.as_str(), "auth");
            let always_success_bin = load_binary(base_dir.as_str(), "always_success");
            let funding_lock_out_point: OutPoint = context.deploy_cell(funding_lock_bin);
            let commitment_lock_out_point = context.deploy_cell(commitment_lock_bin);
            let auth_out_point: OutPoint = context.deploy_cell(auth_bin);
            let always_success_out_point = context.deploy_cell(always_success_bin);

            dbg!(
                &funding_lock_out_point,
                &commitment_lock_out_point,
                &auth_out_point,
                &always_success_out_point
            );
            // prepare cell deps
            let funding_lock_dep = CellDep::new_builder()
                .out_point(funding_lock_out_point.clone())
                .build();
            let commitment_lock_dep = CellDep::new_builder()
                .out_point(commitment_lock_out_point.clone())
                .build();
            dbg!(&commitment_lock_out_point);
            let auth_dep = CellDep::new_builder()
                .out_point(auth_out_point.clone())
                .build();
            let always_success_dep = CellDep::new_builder()
                .out_point(always_success_out_point.clone())
                .build();
            dbg!(
                &funding_lock_dep,
                &commitment_lock_dep,
                &auth_dep,
                &always_success_dep
            );
            let cell_deps = vec![
                funding_lock_dep,
                commitment_lock_dep,
                auth_dep,
                always_success_dep,
            ]
            .pack();
            context.set_capture_debug(true);
            let mut map = HashMap::new();
            map.insert(
                Contracts::FundingLock,
                (
                    funding_lock_out_point.clone(),
                    context
                        .build_script(&funding_lock_out_point, Bytes::new())
                        .expect("Build script"),
                ),
            );
            map.insert(
                Contracts::CommitmentLock,
                (
                    commitment_lock_out_point.clone(),
                    context
                        .build_script(&commitment_lock_out_point, Bytes::new())
                        .expect("Build script"),
                ),
            );
            map.insert(
                Contracts::Secp256k1Lock,
                (
                    auth_out_point.clone(),
                    context
                        .build_script(&auth_out_point, Bytes::new())
                        .expect("Build script"),
                ),
            );
            map.insert(
                Contracts::AlwaysSuccess,
                (
                    always_success_out_point.clone(),
                    context
                        .build_script(&always_success_out_point, Bytes::new())
                        .expect("Build script"),
                ),
            );

            let cc = ContractsContext {
                contracts: map,
                cell_deps: cell_deps,
            };
            let lock = RwLock::new(context);
            let context = MockContext {
                context: lock,
                contracts_context: cc,
            };
            context
        });
        INSTANCE.get().unwrap()
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
enum Contracts {
    FundingLock,
    CommitmentLock,
    Secp256k1Lock,
    AlwaysSuccess,
}

struct ContractsContext {
    contracts: HashMap<Contracts, (OutPoint, Script)>,
    cell_deps: CellDepVec,
}

pub(crate) enum CommitmentLockContext {
    Mock(&'static MockContext),
    Real(ContractsContext),
}

impl CommitmentLockContext {
    pub fn get() -> Self {
        Self::Mock(MockContext::get())
    }

    pub fn is_testing(&self) -> bool {
        true
    }

    fn get_contracts_map(&self) -> &HashMap<Contracts, (OutPoint, Script)> {
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

    fn get_out_point(&self, contract: Contracts) -> OutPoint {
        self.get_contracts_map().get(&contract).unwrap().0.clone()
    }

    fn get_script(&self, contract: Contracts, args: &[u8]) -> Script {
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
        self.get_out_point(Contracts::CommitmentLock)
    }

    pub fn get_secp256k1_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contracts::Secp256k1Lock, args)
    }

    pub fn get_commitment_lock_script(&self, args: &[u8]) -> Script {
        self.get_script(Contracts::CommitmentLock, args)
    }

    pub fn get_commitment_transaction_cell_deps(&self) -> CellDepVec {
        self.get_cell_deps().clone()
    }

    pub fn get_always_success_outpoint(&self) -> OutPoint {
        self.get_out_point(Contracts::AlwaysSuccess)
    }

    pub fn get_always_success_script(&self, args: &[u8]) -> Script {
        self.get_script(Contracts::AlwaysSuccess, args)
    }
}
