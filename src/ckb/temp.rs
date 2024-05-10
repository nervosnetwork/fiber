use ckb_hash::blake2b_256;
use ckb_sdk::Since;
use ckb_testtool::context::Context;
use ckb_types::{
    core::{TransactionBuilder, TransactionView},
    packed::{CellDep, CellDepVec, CellInput, CellOutput, OutPoint, Script},
    prelude::{Builder, Entity, Pack, PackVec},
};
struct AugmentedTransaction {
    tx: TransactionView,
    lock_script: Script,
    witness_script: Vec<u8>,
}
use ckb_testtool::{
    ckb_error::Error,
    ckb_types::{bytes::Bytes, core::Cycle},
};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;

use once_cell::sync::OnceCell;

use super::{channel::TLC, types::Pubkey};

// The exact same Loader code from capsule's template, except that
// now we use MODE as the environment variable
const TEST_ENV_VAR: &str = "MODE";

pub enum TestEnv {
    Debug,
    Release,
}

impl FromStr for TestEnv {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "debug" => Ok(TestEnv::Debug),
            "release" => Ok(TestEnv::Release),
            _ => Err("no match"),
        }
    }
}

pub struct Loader(PathBuf);

impl Default for Loader {
    fn default() -> Self {
        let test_env = match env::var(TEST_ENV_VAR) {
            Ok(val) => val.parse().expect("test env"),
            Err(_) => TestEnv::Release,
        };
        Self::with_test_env(test_env)
    }
}

impl Loader {
    fn with_test_env(env: TestEnv) -> Self {
        let load_prefix = match env {
            TestEnv::Debug => "debug",
            TestEnv::Release => "release",
        };
        let mut base_path = match env::var("TOP") {
            Ok(val) => {
                let mut base_path: PathBuf = val.into();
                base_path.push("build");
                base_path
            }
            Err(_) => {
                let mut base_path = PathBuf::new();
                // cargo may use a different cwd when running tests, for example:
                // when running debug in vscode, it will use workspace root as cwd by default,
                // when running test by `cargo test`, it will use tests directory as cwd,
                // so we need a fallback path
                base_path.push("build");
                if !base_path.exists() {
                    base_path.pop();
                    base_path.push("..");
                    base_path.push("build");
                }
                base_path
            }
        };

        base_path.push(load_prefix);
        Loader(base_path)
    }

    pub fn load_binary(&self, name: &str) -> Bytes {
        let mut path = self.0.clone();
        path.push(name);
        let result = fs::read(&path);
        if result.is_err() {
            panic!("Binary {:?} is missing!", path);
        }
        result.unwrap().into()
    }
}

// This helper method runs Context::verify_tx, but in case error happens,
// it also dumps current transaction to failed_txs folder.
pub fn verify_and_dump_failed_tx(
    context: &Context,
    tx: &TransactionView,
    max_cycles: u64,
) -> Result<Cycle, Error> {
    let result = context.verify_tx(tx, max_cycles);
    if result.is_err() {
        let mut path = env::current_dir().expect("current dir");
        path.push("failed_txs");
        std::fs::create_dir_all(&path).expect("create failed_txs dir");
        let mock_tx = context.dump_tx(tx).expect("dump failed tx");
        let json = serde_json::to_string_pretty(&mock_tx).expect("json");
        path.push(format!("0x{:x}.json", tx.hash()));
        println!("Failed tx written to {:?}", path);
        std::fs::write(path, json).expect("write");
    }
    result
}

pub(crate) struct CommitmentLockContext {
    pub(crate) context: Context,
    pub(crate) funding_lock_out_point: OutPoint,
    pub(crate) commitment_lock_out_point: OutPoint,
    pub(crate) cell_deps: CellDepVec,
}

impl CommitmentLockContext {
    fn new() -> Self {
        // deploy contract
        let mut context = Context::default();
        let loader = Loader::default();
        let funding_lock_bin =
            loader.load_binary("/home/e/Workspace/ckb-pcn-scripts/build/release/funding-lock");
        let commitment_lock_bin =
            loader.load_binary("/home/e/Workspace/ckb-pcn-scripts/build/release/commitment-lock");
        let auth_bin = loader.load_binary("/home/e/Workspace/ckb-pcn-scripts/deps/auth");
        let funding_lock_out_point = context.deploy_cell(funding_lock_bin);
        let commitment_lock_out_point = context.deploy_cell(commitment_lock_bin);
        let auth_out_point = context.deploy_cell(auth_bin);

        dbg!(
            &funding_lock_out_point,
            &commitment_lock_out_point,
            &auth_out_point
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
        let cell_deps = vec![funding_lock_dep, commitment_lock_dep, auth_dep].pack();
        Self {
            context,
            funding_lock_out_point,
            commitment_lock_out_point,
            cell_deps,
        }
    }

    fn get_witnesses(
        &self,
        local_delay_epoch: Since,
        local_delay_epoch_key: Pubkey,
        revocation_key: Pubkey,
        tlcs: Vec<TLC>,
    ) -> Vec<u8> {
        let witness_script = [
            local_delay_epoch.value().to_le_bytes().to_vec(),
            blake2b_256(local_delay_epoch_key.serialize())[0..20].to_vec(),
            blake2b_256(revocation_key.serialize())[0..20].to_vec(),
            tlcs.iter()
                .map(|tlc| tlc.serialize_to_lock_args())
                .flatten()
                .collect(),
        ]
        .concat();
        witness_script
    }

    fn create_commitment_cell_with_aux_data(
        &mut self,
        capacity: u64,
        local_delay_epoch: Since,
        local_delay_epoch_key: Pubkey,
        revocation_key: Pubkey,
        tlcs: Vec<TLC>,
    ) -> (OutPoint, Vec<u8>, Script) {
        let witness_script = self.get_witnesses(
            local_delay_epoch,
            local_delay_epoch_key,
            revocation_key,
            tlcs,
        );

        let args = blake2b_256(&witness_script)[0..20].to_vec();

        let lock_script = self
            .context
            .build_script(&self.commitment_lock_out_point, args.into())
            .expect("script");

        (
            self.context.create_cell(
                CellOutput::new_builder()
                    .capacity(capacity.pack())
                    .lock(lock_script.clone())
                    .build(),
                Bytes::new(),
            ),
            witness_script,
            lock_script,
        )
    }

    fn create_augmented_tx(
        &mut self,
        capacity: u64,
        local_delay_epoch: Since,
        local_delay_epoch_key: Pubkey,
        revocation_key: Pubkey,
        tlcs: Vec<TLC>,
        outputs: Vec<CellOutput>,
        outputs_data: Vec<Bytes>,
    ) -> AugmentedTransaction {
        let (input_out_point, witness_script, lock_script) = self
            .create_commitment_cell_with_aux_data(
                capacity,
                local_delay_epoch,
                local_delay_epoch_key,
                revocation_key,
                tlcs,
            );

        let input = CellInput::new_builder()
            .previous_output(input_out_point.clone())
            .build();

        // build transaction with revocation unlock logic
        let tx = TransactionBuilder::default()
            .cell_deps(self.cell_deps.clone())
            .input(input)
            .outputs(outputs)
            .outputs_data(outputs_data.pack())
            .build();

        AugmentedTransaction {
            tx,
            lock_script,
            witness_script,
        }
    }
}

pub fn get_commitment_lock_context() -> &'static Mutex<CommitmentLockContext> {
    static INSTANCE: OnceCell<Mutex<CommitmentLockContext>> = OnceCell::new();
    INSTANCE.get_or_init(|| {
        let c = CommitmentLockContext::new();
        Mutex::new(c) // run
    })
}

pub fn get_commitment_lock_outpoint() -> OutPoint {
    let context = get_commitment_lock_context().lock().unwrap();
    context.commitment_lock_out_point.clone()
}
