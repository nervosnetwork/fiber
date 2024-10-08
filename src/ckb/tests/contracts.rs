use ckb_testtool::{ckb_types::bytes::Bytes, context::Context};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use ckb_types::{core::TransactionView, packed::CellOutput, prelude::Pack};
use molecule::prelude::{Builder, Entity};

use crate::ckb::contracts::{get_script_by_contract, Contract, ContractsContext, MockContext};


#[derive(Clone, Debug)]
pub struct MockContext {
    context: Arc<RwLock<Context>>,
    contracts_context: Arc<ContractsInfo>,
}

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

impl Default for MockContext {
    fn default() -> Self {
        Self::new()
    }
}

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
