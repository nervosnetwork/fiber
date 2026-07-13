//! Chain adapter boundary for loop-out liquidity operations.

use ckb_types::packed;

use crate::liquidity::tx::{
    build_liquidity_lock_output, LiquidityLockBuildError, LiquidityLockOutputParams,
    LiquidityLockScriptArtifact,
};

/// Request to build the on-chain payout output for a loop-out swap.
#[derive(Debug, Clone)]
pub struct LoopOutPayoutRequest {
    /// CKB-hash of the 32-byte payment preimage.
    pub payment_hash: [u8; 32],
    /// Lock allowed to claim the payout with the payment preimage.
    pub claimant_lock: packed::Script,
    /// Lock allowed to refund the payout after the timeout.
    pub refund_lock: packed::Script,
    /// Absolute `since` value required before refund is valid.
    pub refund_after_lock_time: u64,
    /// Raw CKB/UDT amount protected by the liquidity-lock script.
    pub amount: u128,
    /// UDT type script for UDT payouts, absent for CKB payouts.
    pub asset_type_script: Option<packed::Script>,
    /// Cell capacity in shannons.
    pub capacity: u64,
}

/// Build the lock output and data pair for a loop-out payout cell.
pub fn build_loop_out_payout_output(
    artifact: &LiquidityLockScriptArtifact,
    request: &LoopOutPayoutRequest,
) -> Result<(packed::CellOutput, packed::Bytes), LiquidityLockBuildError> {
    build_liquidity_lock_output(
        artifact,
        &LiquidityLockOutputParams {
            payment_hash: request.payment_hash,
            claimant_lock: request.claimant_lock.clone(),
            refund_lock: request.refund_lock.clone(),
            refund_after_lock_time: request.refund_after_lock_time,
            amount: request.amount,
            asset_type_script: request.asset_type_script.clone(),
            capacity: request.capacity,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ckb_types::{bytes::Bytes, packed, prelude::*};

    fn script(args: &'static str) -> packed::Script {
        packed::Script::new_builder()
            .args(Bytes::from(args).pack())
            .build()
    }

    #[test]
    fn payout_builder_reuses_liquidity_lock_output_builder() {
        let artifact = crate::liquidity::tx::LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        };
        let request = LoopOutPayoutRequest {
            payment_hash: [1u8; 32],
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            refund_after_lock_time: 42,
            amount: 1000,
            asset_type_script: None,
            capacity: 2000,
        };

        let (output, data) = build_loop_out_payout_output(&artifact, &request).unwrap();

        assert_eq!(u64::from(output.capacity()), 2000);
        assert!(data.raw_data().is_empty());
    }

    #[test]
    fn payout_builder_forwards_udt_type_and_amount_data() {
        let artifact = crate::liquidity::tx::LiquidityLockScriptArtifact {
            code_hash: packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            hash_type: packed::Byte::new(0),
        };
        let udt_type_script = script("udt");
        let request = LoopOutPayoutRequest {
            payment_hash: [1u8; 32],
            claimant_lock: script("claimant"),
            refund_lock: script("refund"),
            refund_after_lock_time: 42,
            amount: 1000,
            asset_type_script: Some(udt_type_script.clone()),
            capacity: 2000,
        };

        let (output, data) = build_loop_out_payout_output(&artifact, &request).unwrap();

        assert_eq!(output.type_().to_opt(), Some(udt_type_script));
        assert_eq!(data.raw_data().as_ref(), 1000u128.to_le_bytes());
    }
}
