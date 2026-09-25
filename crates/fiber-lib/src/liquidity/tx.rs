//! Liquidity-lock transaction output builders.

use ckb_types::{bytes::Bytes, packed, prelude::*};
use thiserror::Error;

use super::build_liquidity_lock_args;
use super::types::LiquidityLoopOutError;

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
        .hash_type(artifact.hash_type)
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

/// Build the witness bytes accepted by the liquidity-lock claim path.
pub fn build_liquidity_lock_claim_witness(payment_preimage: [u8; 32]) -> packed::Bytes {
    let mut witness = packed::WitnessArgs::default().as_bytes().to_vec();
    witness.push(1);
    witness.extend_from_slice(&payment_preimage);
    Bytes::from(witness).pack()
}

/// Parse a liquidity-lock claim witness and return its payment preimage.
pub fn parse_liquidity_lock_claim_witness(
    witness: &packed::Bytes,
) -> Result<[u8; 32], LiquidityLoopOutError> {
    let prefix = packed::WitnessArgs::default().as_bytes();
    let bytes = witness.raw_data();
    let expected_len = prefix.len() + 1 + 32;
    if bytes.len() < prefix.len() + 1 {
        return Err(LiquidityLoopOutError::Chain(format!(
            "invalid liquidity-lock claim witness length: expected {expected_len}, got {}",
            bytes.len()
        )));
    }
    if &bytes[..prefix.len()] != prefix.as_ref() {
        return Err(LiquidityLoopOutError::Chain(
            "invalid liquidity-lock claim witness prefix".to_string(),
        ));
    }
    if bytes[prefix.len()] != 1 {
        return Err(LiquidityLoopOutError::Chain(format!(
            "invalid liquidity-lock claim branch: expected 1, got {}",
            bytes[prefix.len()]
        )));
    }
    if bytes.len() != expected_len {
        return Err(LiquidityLoopOutError::Chain(format!(
            "invalid liquidity-lock claim witness length: expected {expected_len}, got {}",
            bytes.len()
        )));
    }

    Ok(bytes[prefix.len() + 1..]
        .try_into()
        .expect("length checked"))
}

/// Build the witness bytes accepted by the liquidity-lock refund path.
pub fn build_liquidity_lock_refund_witness() -> packed::Bytes {
    let mut witness = packed::WitnessArgs::default().as_bytes().to_vec();
    witness.push(2);
    Bytes::from(witness).pack()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_witness_matches_liquidity_lock_contract_layout() {
        let preimage = [7u8; 32];

        let witness = build_liquidity_lock_claim_witness(preimage);

        let expected_prefix = packed::WitnessArgs::default().as_bytes();
        assert_eq!(
            &witness.raw_data()[..expected_prefix.len()],
            expected_prefix.as_ref()
        );
        assert_eq!(witness.raw_data()[expected_prefix.len()], 1);
        assert_eq!(&witness.raw_data()[expected_prefix.len() + 1..], &preimage);
        assert_eq!(witness.raw_data().len(), expected_prefix.len() + 33);
    }

    #[test]
    fn refund_witness_matches_liquidity_lock_contract_layout() {
        let witness = build_liquidity_lock_refund_witness();

        let expected_prefix = packed::WitnessArgs::default().as_bytes();
        assert_eq!(
            &witness.raw_data()[..expected_prefix.len()],
            expected_prefix.as_ref()
        );
        assert_eq!(witness.raw_data()[expected_prefix.len()], 2);
        assert_eq!(witness.raw_data().len(), expected_prefix.len() + 1);
    }

    #[test]
    fn parse_claim_witness_roundtrips_built_claim() {
        let preimage = [7u8; 32];

        assert_eq!(
            parse_liquidity_lock_claim_witness(&build_liquidity_lock_claim_witness(preimage)),
            Ok(preimage)
        );
    }

    #[test]
    fn parse_claim_witness_rejects_refund_branch() {
        let error =
            parse_liquidity_lock_claim_witness(&build_liquidity_lock_refund_witness()).unwrap_err();

        assert!(error.to_string().contains("claim branch"));
    }

    #[test]
    fn parse_claim_witness_rejects_malformed_and_extra_bytes() {
        let prefix = packed::WitnessArgs::default().as_bytes();
        let malformed = Bytes::from(prefix[..prefix.len() - 1].to_vec()).pack();
        let mut extra = build_liquidity_lock_claim_witness([7u8; 32])
            .raw_data()
            .to_vec();
        extra.push(0);

        assert!(parse_liquidity_lock_claim_witness(&malformed).is_err());
        assert!(parse_liquidity_lock_claim_witness(&Bytes::from(extra).pack()).is_err());
    }
}
