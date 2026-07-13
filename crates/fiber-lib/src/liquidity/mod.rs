//! Fiber-side liquidity integration helpers.

pub mod actor;
pub mod payment;
pub mod quote;
pub mod store;
pub mod tx;
pub mod types;

use ckb_hash::blake2b_256;
use ckb_types::{packed::Script, prelude::*};

/// Build script args for the `liquidity-lock` contract in `../fiber-scripts`.
pub fn build_liquidity_lock_args(
    payment_hash: [u8; 32],
    claimant_lock: &Script,
    refund_lock: &Script,
    refund_after_lock_time: u64,
    amount: u128,
    asset_type_script: Option<&Script>,
) -> Vec<u8> {
    let asset_type_hash = asset_type_script
        .map(|script| blake2b_256(script.as_slice()))
        .unwrap_or([0u8; 32]);

    [
        payment_hash.to_vec(),
        blake2b_256(claimant_lock.as_slice()).to_vec(),
        blake2b_256(refund_lock.as_slice()).to_vec(),
        refund_after_lock_time.to_le_bytes().to_vec(),
        amount.to_le_bytes().to_vec(),
        asset_type_hash.to_vec(),
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use ckb_hash::blake2b_256;
    use ckb_types::{bytes::Bytes, packed, packed::Script, prelude::*};

    use super::tx::{
        build_liquidity_lock_output, LiquidityLockBuildError, LiquidityLockOutputParams,
        LiquidityLockScriptArtifact,
    };
    use super::*;

    fn script(args: &'static str) -> Script {
        Script::new_builder().args(Bytes::from(args).pack()).build()
    }

    #[test]
    fn liquidity_lock_args_match_script_contract_layout_for_ckb() {
        let payment_hash = [1u8; 32];
        let claimant_lock = script("claimant");
        let refund_lock = script("refund");
        let args =
            build_liquidity_lock_args(payment_hash, &claimant_lock, &refund_lock, 42, 1000, None);

        let expected = [
            payment_hash.to_vec(),
            blake2b_256(claimant_lock.as_slice()).to_vec(),
            blake2b_256(refund_lock.as_slice()).to_vec(),
            42u64.to_le_bytes().to_vec(),
            1000u128.to_le_bytes().to_vec(),
            [0u8; 32].to_vec(),
        ]
        .concat();

        assert_eq!(args, expected);
    }

    #[test]
    fn liquidity_lock_args_include_udt_type_script_hash() {
        let udt_type_script = script("udt");
        let args = build_liquidity_lock_args(
            [1u8; 32],
            &script("claimant"),
            &script("refund"),
            42,
            1000,
            Some(&udt_type_script),
        );

        assert_eq!(&args[120..152], blake2b_256(udt_type_script.as_slice()));
    }

    #[test]
    fn liquidity_lock_output_builder_sets_ckb_output_without_type_or_data() {
        let artifact = LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        };
        let params = LiquidityLockOutputParams {
            payment_hash: [1u8; 32],
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            refund_after_lock_time: 42,
            amount: 1000,
            asset_type_script: None,
            capacity: 2000,
        };

        let (output, data) = build_liquidity_lock_output(&artifact, &params).expect("ckb output");

        assert!(output.type_().to_opt().is_none());
        assert!(data.raw_data().is_empty());
        assert_eq!(u64::from(output.capacity()), 2000);
    }

    #[test]
    fn liquidity_lock_output_builder_sets_udt_type_and_exact_amount_data() {
        let artifact = LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        };
        let udt_type_script = script("udt");
        let params = LiquidityLockOutputParams {
            payment_hash: [1u8; 32],
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            refund_after_lock_time: 42,
            amount: 1000,
            asset_type_script: Some(udt_type_script.clone()),
            capacity: 2000,
        };

        let (output, data) = build_liquidity_lock_output(&artifact, &params).expect("udt output");

        assert_eq!(output.type_().to_opt(), Some(udt_type_script));
        assert_eq!(data.raw_data().as_ref(), 1000u128.to_le_bytes());
    }

    #[test]
    fn liquidity_lock_output_builder_rejects_ckb_capacity_below_amount() {
        let artifact = LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        };
        let params = LiquidityLockOutputParams {
            payment_hash: [1u8; 32],
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            refund_after_lock_time: 42,
            amount: 2000,
            asset_type_script: None,
            capacity: 1000,
        };

        assert_eq!(
            build_liquidity_lock_output(&artifact, &params),
            Err(LiquidityLockBuildError::CkbCapacityBelowAmount {
                capacity: 1000,
                amount: 2000,
            })
        );
    }
}
