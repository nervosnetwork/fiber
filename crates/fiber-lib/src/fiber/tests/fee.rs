use crate::fiber::channel::create_witness_for_commitment_cell_with_pending_tlcs;
use crate::fiber::fee::checked_calculate_commitment_tx_fee;
use fiber_types::CommitmentContractVersion;

#[test]
fn checked_commitment_tx_fee_allows_intermediate_u64_overflow_when_final_fee_fits() {
    let tx_size =
        checked_calculate_commitment_tx_fee(1_000, &None, CommitmentContractVersion::Legacy)
            .expect("fee rate 1000 should return commitment tx size");
    assert!(tx_size > 0);

    let fee_rate = u64::MAX / tx_size + 1;
    let raw_product = u128::from(fee_rate) * u128::from(tx_size);
    assert!(raw_product > u128::from(u64::MAX));

    let expected_fee = u64::try_from(raw_product / 1_000).expect("final fee should fit");
    let actual_fee =
        checked_calculate_commitment_tx_fee(fee_rate, &None, CommitmentContractVersion::Legacy)
            .expect("intermediate u64 overflow should be allowed when final fee fits");

    assert_eq!(actual_fee, expected_fee);
}

#[test]
fn commitment_cell_witness_uses_version_specific_htlc_size() {
    let legacy_pending_tlcs = vec![0; 85];
    let v1_pending_tlcs = vec![0; 97];

    let legacy_witness = create_witness_for_commitment_cell_with_pending_tlcs(
        1,
        &legacy_pending_tlcs,
        CommitmentContractVersion::Legacy,
    );
    let v1_witness = create_witness_for_commitment_cell_with_pending_tlcs(
        1,
        &v1_pending_tlcs,
        CommitmentContractVersion::V1,
    );

    assert_eq!(legacy_witness[17], 1);
    assert_eq!(v1_witness[17], 1);
    assert_eq!(legacy_witness.len() + 12, v1_witness.len());
}
