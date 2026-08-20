use ckb_hash::blake2b_256;
use ckb_types::{
    bytes::Bytes,
    core::TransactionView,
    packed::{CellInput, CellOutput, Script},
    prelude::{Builder, Entity, Pack},
};

use crate::{
    ckb::{
        contracts::{
            get_cell_deps_by_contracts, get_script_by_contract, try_get_script_by_contract,
            Contract,
        },
        tests::test_utils::MOCK_CONTEXT,
    },
    liquidity::{
        build_liquidity_lock_args,
        tx::{build_liquidity_lock_claim_witness, build_liquidity_lock_refund_witness},
    },
};

const MAX_CYCLES: u64 = 100_000_000;
const CAPACITY: u64 = 10_000_000_000;

enum Unlock {
    Claim([u8; 32]),
    Refund { since: u64 },
}

async fn liquidity_lock_vm_verifies(payment_preimage: [u8; 32], unlock: Unlock) -> bool {
    let claimant_lock = Script::new_builder()
        .args(Bytes::from_static(b"claimant").pack())
        .build();
    let refund_lock = Script::new_builder()
        .args(Bytes::from_static(b"refund").pack())
        .build();
    let refund_after_lock_time = 42;
    let args = build_liquidity_lock_args(
        blake2b_256(payment_preimage),
        &claimant_lock,
        &refund_lock,
        refund_after_lock_time,
        CAPACITY.into(),
        None,
    );
    let lock = get_script_by_contract(Contract::LiquidityLock, &args);
    let cell_deps = get_cell_deps_by_contracts(vec![Contract::LiquidityLock])
        .await
        .expect("get liquidity lock cell deps");
    let (output_lock, since, witness) = match unlock {
        Unlock::Claim(preimage) => (
            claimant_lock,
            0,
            build_liquidity_lock_claim_witness(preimage),
        ),
        Unlock::Refund { since } => (refund_lock, since, build_liquidity_lock_refund_witness()),
    };

    let mut mock_context = MOCK_CONTEXT.write().unwrap();
    let input_out_point = mock_context.context.create_cell(
        CellOutput::new_builder()
            .capacity(CAPACITY)
            .lock(lock)
            .build(),
        Bytes::new(),
    );
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(cell_deps)
        .input(
            CellInput::new_builder()
                .previous_output(input_out_point)
                .since(since)
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(CAPACITY)
                .lock(output_lock)
                .build(),
        )
        .output_data(Bytes::new().pack())
        .witness(witness)
        .build();

    mock_context.context.verify_tx(&tx, MAX_CYCLES).is_ok()
}

#[tokio::test]
async fn liquidity_lock_mock_context_resolves_script_and_deps() {
    assert!(try_get_script_by_contract(Contract::LiquidityLock, &[]).is_some());
    assert!(!get_cell_deps_by_contracts(vec![Contract::LiquidityLock])
        .await
        .expect("get liquidity lock cell deps")
        .is_empty());
}

#[tokio::test]
async fn liquidity_lock_vm_accepts_valid_claim() {
    let preimage = [7; 32];
    assert!(liquidity_lock_vm_verifies(preimage, Unlock::Claim(preimage)).await);
}

#[tokio::test]
async fn liquidity_lock_vm_rejects_wrong_preimage() {
    assert!(!liquidity_lock_vm_verifies([7; 32], Unlock::Claim([8; 32])).await);
}

#[tokio::test]
async fn liquidity_lock_vm_rejects_early_refund() {
    assert!(!liquidity_lock_vm_verifies([7; 32], Unlock::Refund { since: 41 }).await);
}
