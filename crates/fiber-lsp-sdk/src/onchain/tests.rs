use ckb_types::core::EpochNumberWithFraction;

use super::*;

fn relative_epoch_since(epoch: EpochNumberWithFraction) -> u64 {
    0xa000_0000_0000_0000 | epoch.full_value()
}

#[test]
fn epoch_at_least_matches_watchtower_rounding_for_1_3_and_2_3_and_exact_3_3() {
    let delay = EpochNumberWithFraction::new(1, 1, 1800);
    let required = relative_epoch_since(delay);

    // 1/3 TLC preimage claim: watchtower normalizes (1, 1, 1800) * 1/3 -> (0, 450, 1350).
    let expected_1_3 = mul_watchtower_delay(delay, 1, 3).unwrap();
    assert_eq!(expected_1_3, EpochNumberWithFraction::new(0, 450, 1350));
    assert!(epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 450, 1350)),
        required,
        1,
        3,
    ));
    assert!(epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 600, 1800)),
        required,
        1,
        3,
    ));
    assert!(!epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 449, 1350)),
        required,
        1,
        3,
    ));
    assert!(!epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 599, 1800)),
        required,
        1,
        3,
    ));

    // 2/3 TLC timeout claim: watchtower normalizes (1, 1, 1800) * 2/3 -> (0, 900, 1350).
    let expected_2_3 = mul_watchtower_delay(delay, 2, 3).unwrap();
    assert_eq!(expected_2_3, EpochNumberWithFraction::new(0, 900, 1350));
    assert!(epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 900, 1350)),
        required,
        2,
        3,
    ));
    assert!(epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 1200, 1800)),
        required,
        2,
        3,
    ));
    assert!(!epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 899, 1350)),
        required,
        2,
        3,
    ));
    assert!(!epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 1199, 1800)),
        required,
        2,
        3,
    ));

    // 3/3 final party unlock: watchtower uses delay_epoch directly without scaling.
    assert!(epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(1, 1, 1800)),
        required,
        3,
        3,
    ));
    assert!(!epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(1, 0, 1350)),
        required,
        3,
        3,
    ));
    assert!(!epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(1, 0, 1800)),
        required,
        3,
        3,
    ));
}

#[test]
fn epoch_at_least_handles_unscaled_fractions_and_invalid_inputs() {
    let delay = EpochNumberWithFraction::new(1, 3, 600);
    let required = relative_epoch_since(delay);
    // 3 * 600 = 1800 (no scale_factor needed): (1, 3, 600) * 1/3 = (0, 603, 1800).
    assert_eq!(
        mul_watchtower_delay(delay, 1, 3).unwrap(),
        EpochNumberWithFraction::new(0, 603, 1800)
    );
    assert!(epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 603, 1800)),
        required,
        1,
        3,
    ));
    assert!(!epoch_at_least(
        relative_epoch_since(EpochNumberWithFraction::new(0, 602, 1800)),
        required,
        1,
        3,
    ));

    // Invalid flags, zero length, index >= length, or zero denominator are rejected.
    let valid = relative_epoch_since(EpochNumberWithFraction::new(1, 0, 1));
    assert!(!epoch_at_least(
        valid & 0x00ff_ffff_ffff_ffff,
        required,
        1,
        3
    ));
    assert!(!epoch_at_least(
        valid,
        required & 0x00ff_ffff_ffff_ffff,
        1,
        3
    ));
    assert!(!epoch_at_least(
        0xa000_0000_0000_0000 | EpochNumberWithFraction::new_unchecked(1, 0, 0).full_value(),
        required,
        1,
        3,
    ));
    assert!(!epoch_at_least(
        valid,
        0xa000_0000_0000_0000 | EpochNumberWithFraction::new_unchecked(1, 0, 0).full_value(),
        1,
        3,
    ));
    assert!(!epoch_at_least(
        0xa000_0000_0000_0000 | EpochNumberWithFraction::new_unchecked(0, 10, 10).full_value(),
        required,
        1,
        3,
    ));
    assert!(!epoch_at_least(
        valid,
        0xa000_0000_0000_0000 | EpochNumberWithFraction::new_unchecked(0, 10, 10).full_value(),
        1,
        3,
    ));
    assert!(!epoch_at_least(valid, required, 1, 0));
}
