use crate::fiber::fee::checked_calculate_commitment_tx_fee;

#[test]
fn checked_commitment_tx_fee_allows_intermediate_u64_overflow_when_final_fee_fits() {
    let tx_size = checked_calculate_commitment_tx_fee(1_000, &None)
        .expect("fee rate 1000 should return commitment tx size");
    assert!(tx_size > 0);

    let fee_rate = u64::MAX / tx_size + 1;
    let raw_product = u128::from(fee_rate) * u128::from(tx_size);
    assert!(raw_product > u128::from(u64::MAX));

    let expected_fee = u64::try_from(raw_product / 1_000).expect("final fee should fit");
    let actual_fee = checked_calculate_commitment_tx_fee(fee_rate, &None)
        .expect("intermediate u64 overflow should be allowed when final fee fits");

    assert_eq!(actual_fee, expected_fee);
}
