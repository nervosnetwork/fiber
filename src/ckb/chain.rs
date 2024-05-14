use ckb_testtool::ckb_types::bytes::Bytes;
use ckb_testtool::context::Context;
use ckb_types::{
    packed::{CellDep, CellDepVec, OutPoint, Script},
    prelude::{Builder, Entity, PackVec},
};
use once_cell::sync::OnceCell;
use std::sync::RwLock;
use std::{env, fs, path::PathBuf};

pub struct Loader();
impl Loader {
    pub fn load_binary(&self, name: PathBuf) -> Bytes {
        let result = fs::read(&name);
        if result.is_err() {
            panic!("Loading binary {:?} failed: {:?}", name, result.err());
        }
        result.unwrap().into()
    }
}

pub(crate) struct InnerCommitmentLockContext {
    pub(crate) context: Context,
    pub(crate) funding_lock_out_point: OutPoint,
    pub(crate) commitment_lock_out_point: OutPoint,
    pub(crate) always_success_out_point: OutPoint,
    pub(crate) cell_deps: CellDepVec,
}

impl InnerCommitmentLockContext {
    fn new() -> Self {
        // deploy contract
        let base_dir = env::var("BINARY_PATH")
            .ok()
            .map(|x| PathBuf::from(x))
            .unwrap_or({
                let mut cwd = env::current_dir().unwrap();
                cwd.push("build");
                cwd.push("release");
                cwd
            });
        let get_binary_path = |binary: &str| {
            let mut path = base_dir.clone();
            path.push(PathBuf::from(binary));
            path
        };
        let mut context = Context::default();
        let loader = Loader {};

        let funding_lock_bin = loader.load_binary(get_binary_path("funding-lock"));
        let commitment_lock_bin = loader.load_binary(get_binary_path("commitment-lock"));
        let auth_bin = loader.load_binary(get_binary_path("auth"));
        let always_success_bin = loader.load_binary(get_binary_path("always_success"));
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
        let auth_dep = CellDep::new_builder().out_point(auth_out_point).build();
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
        Self {
            context,
            funding_lock_out_point,
            commitment_lock_out_point,
            always_success_out_point,
            cell_deps,
        }
    }
}

pub(crate) fn get_commitment_lock_context() -> &'static RwLock<InnerCommitmentLockContext> {
    static INSTANCE: OnceCell<RwLock<InnerCommitmentLockContext>> = OnceCell::new();
    INSTANCE.get_or_init(|| {
        let c = InnerCommitmentLockContext::new();
        RwLock::new(c) // run
    })
}

pub struct CommitmentLockContext {
    pub(crate) inner: &'static RwLock<InnerCommitmentLockContext>,
}

impl CommitmentLockContext {
    pub fn get() -> Self {
        CommitmentLockContext {
            inner: get_commitment_lock_context(),
        }
    }

    pub fn is_testing(&self) -> bool {
        true
    }

    pub fn get_commitment_lock_outpoint(&self) -> OutPoint {
        let context = self.inner.read().unwrap();
        context.commitment_lock_out_point.clone()
    }

    pub fn get_secp256k1_lock_script(&self, args: &[u8]) -> Script {
        self.get_always_success_script(args)
    }

    pub fn get_commitment_lock_script(&self, args: &[u8]) -> Script {
        let context = self.inner.read().unwrap();
        let commitment_lock_out_point = context.commitment_lock_out_point.clone();
        context
            .context
            .build_script(&commitment_lock_out_point, args.to_owned().into())
            .expect("Build script")
    }

    pub fn get_commitment_transaction_cell_deps(&self) -> CellDepVec {
        self.inner.read().unwrap().cell_deps.clone()
    }

    pub fn get_always_success_outpoint(&self) -> OutPoint {
        let context = self.inner.read().unwrap();
        context.always_success_out_point.clone()
    }

    pub fn get_always_success_script(&self, args: &[u8]) -> Script {
        let context = self.inner.read().unwrap();
        let always_success_out_point = context.always_success_out_point.clone();
        context
            .context
            .build_script(&always_success_out_point, args.to_owned().into())
            .expect("Build script")
    }
}
