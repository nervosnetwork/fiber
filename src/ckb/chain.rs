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
use std::{fs, path::PathBuf};

use std::{env, sync::RwLock};

use once_cell::sync::OnceCell;

use super::{channel::TLC, types::Pubkey};

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
    pub(crate) always_success_out_point: OutPoint,
    pub(crate) cell_deps: CellDepVec,
}

impl CommitmentLockContext {
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

pub fn get_commitment_lock_context() -> &'static RwLock<CommitmentLockContext> {
    static INSTANCE: OnceCell<RwLock<CommitmentLockContext>> = OnceCell::new();
    INSTANCE.get_or_init(|| {
        let c = CommitmentLockContext::new();
        RwLock::new(c) // run
    })
}

pub fn get_commitment_lock_outpoint() -> OutPoint {
    let context = get_commitment_lock_context().read().unwrap();
    context.commitment_lock_out_point.clone()
}

pub fn get_commitment_lock_script(args: &[u8]) -> Script {
    let context = get_commitment_lock_context().read().unwrap();
    let commitment_lock_out_point = context.commitment_lock_out_point.clone();
    context
        .context
        .build_script(&commitment_lock_out_point, args.to_owned().into())
        .expect("Build script")
}

pub fn get_always_success_outpoint() -> OutPoint {
    let context = get_commitment_lock_context().read().unwrap();
    context.always_success_out_point.clone()
}

pub fn get_always_success_script(args: &[u8]) -> Script {
    let context = get_commitment_lock_context().read().unwrap();
    let always_success_out_point = context.always_success_out_point.clone();
    context
        .context
        .build_script(&always_success_out_point, args.to_owned().into())
        .expect("Build script")
}
