//! Liquidity-lock transaction output builders.

use ckb_types::{bytes::Bytes, packed, prelude::*};
use thiserror::Error;

use super::build_liquidity_lock_args;

/// Error returned when liquidity-lock output parameters are inconsistent.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum LiquidityLockBuildError {
    /// CKB lock output capacity must be at least the quoted CKB amount.
    #[error("CKB liquidity-lock capacity {capacity} is below quoted amount {amount}")]
    CkbCapacityBelowAmount {
        /// Requested cell capacity.
        capacity: u64,
        /// Quoted CKB amount.
        amount: u128,
    },
}

/// Script artifact needed to instantiate the liquidity-lock contract.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LiquidityLockScriptArtifact {
    /// Code hash of the deployed liquidity-lock script.
    pub code_hash: packed::Byte32,
    /// Script hash type.
    pub hash_type: packed::Byte,
}

/// Parameters for building a liquidity-lock output.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LiquidityLockOutputParams {
    /// CKB-hash of the 32-byte payment preimage.
    pub payment_hash: [u8; 32],
    /// Lock hash allowed to receive funds on claim.
    pub claimant_lock: packed::Script,
    /// Lock hash allowed to receive funds on refund.
    pub refund_lock: packed::Script,
    /// Absolute `since` value required by refund transactions.
    pub refund_after_lock_time: u64,
    /// Raw CKB/UDT amount protected by the script.
    pub amount: u128,
    /// UDT type script for UDT swaps, absent for CKB swaps.
    pub asset_type_script: Option<packed::Script>,
    /// Cell capacity in shannons.
    pub capacity: u64,
}

/// Build the liquidity-lock script from deployed script metadata and swap terms.
pub fn build_liquidity_lock_script(
    artifact: &LiquidityLockScriptArtifact,
    params: &LiquidityLockOutputParams,
) -> packed::Script {
    packed::Script::new_builder()
        .code_hash(artifact.code_hash.clone())
        .hash_type(artifact.hash_type.clone())
        .args(
            Bytes::from(build_liquidity_lock_args(
                params.payment_hash,
                &params.claimant_lock,
                &params.refund_lock,
                params.refund_after_lock_time,
                params.amount,
                params.asset_type_script.as_ref(),
            ))
            .pack(),
        )
        .build()
}

/// Build the lock output and data pair expected by the liquidity-lock contract.
pub fn build_liquidity_lock_output(
    artifact: &LiquidityLockScriptArtifact,
    params: &LiquidityLockOutputParams,
) -> Result<(packed::CellOutput, packed::Bytes), LiquidityLockBuildError> {
    if params.asset_type_script.is_none() && u128::from(params.capacity) < params.amount {
        return Err(LiquidityLockBuildError::CkbCapacityBelowAmount {
            capacity: params.capacity,
            amount: params.amount,
        });
    }

    let mut output = packed::CellOutput::new_builder()
        .capacity(params.capacity)
        .lock(build_liquidity_lock_script(artifact, params));
    let data = if let Some(asset_type_script) = &params.asset_type_script {
        output = output.type_(Some(asset_type_script.clone()).pack());
        Bytes::from(params.amount.to_le_bytes().to_vec()).pack()
    } else {
        Bytes::new().pack()
    };

    Ok((output.build(), data))
}
