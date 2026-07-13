use fiber_types::{LiquidityAsset, LiquidityAssetKind};

use crate::liquidity::types::{loop_out_gross_payment_amount, LiquidityLoopOutError};

/// Quote values accepted for a Loop Out request after provider-side validation.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ValidatedLoopOutQuote {
    /// Fee charged by the liquidity provider in the quoted asset unit.
    pub provider_fee: u128,
    /// Maximum Fiber routing fee accepted by the requester.
    pub routing_fee_limit: u128,
    /// Quote expiration timestamp in milliseconds.
    pub expires_at: u64,
}

/// Compute the provider fee for a Loop Out amount using checked arithmetic.
pub fn compute_provider_fee(
    asset: &LiquidityAsset,
    amount: u128,
) -> Result<u128, LiquidityLoopOutError> {
    let proportional = amount
        .checked_mul(u128::from(asset.proportional_fee_ppm))
        .and_then(|value| value.checked_div(1_000_000))
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)?;

    asset
        .base_fee
        .checked_add(proportional)
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}

/// Validate a Loop Out quote request against provider asset policy and client fee caps.
pub fn validate_loop_out_quote_request(
    asset: &LiquidityAsset,
    amount: u128,
    max_provider_fee: u128,
    max_routing_fee: u128,
    requested_udt_type_script: Option<&ckb_jsonrpc_types::Script>,
    now_ms: u64,
    expires_at: u64,
) -> Result<ValidatedLoopOutQuote, LiquidityLoopOutError> {
    if !asset.enabled {
        return Err(LiquidityLoopOutError::AssetDisabled(asset.asset_id.clone()));
    }

    if amount < asset.min_amount || amount > asset.max_amount {
        return Err(LiquidityLoopOutError::AmountOutOfRange {
            amount,
            min: asset.min_amount,
            max: asset.max_amount,
        });
    }

    if amount > asset.available_capacity {
        return Err(LiquidityLoopOutError::CapacityTooLow {
            available: asset.available_capacity,
            required: amount,
        });
    }

    let expected_udt_type_script = match asset.kind {
        LiquidityAssetKind::Ckb => None,
        LiquidityAssetKind::Udt => asset.udt_type_script.as_ref(),
    };
    if expected_udt_type_script != requested_udt_type_script {
        return Err(LiquidityLoopOutError::UdtTypeMismatch);
    }

    if expires_at <= now_ms {
        return Err(LiquidityLoopOutError::QuoteExpired);
    }

    let provider_fee = compute_provider_fee(asset, amount)?;
    if provider_fee > max_provider_fee {
        return Err(LiquidityLoopOutError::ProviderFeeTooHigh);
    }

    loop_out_gross_payment_amount(amount, provider_fee, max_routing_fee)?;

    Ok(ValidatedLoopOutQuote {
        provider_fee,
        routing_fee_limit: max_routing_fee,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use ckb_jsonrpc_types::Script;
    use fiber_types::{LiquidityAsset, LiquidityAssetKind};

    use super::*;

    fn ckb_asset(enabled: bool) -> LiquidityAsset {
        LiquidityAsset {
            asset_id: "ckb".to_string(),
            kind: LiquidityAssetKind::Ckb,
            udt_type_script: None,
            min_amount: 10,
            max_amount: 1_000,
            available_capacity: 10_000,
            base_fee: 2,
            proportional_fee_ppm: 1_000,
            enabled,
        }
    }

    fn udt_script(args: &'static str) -> Script {
        serde_json::from_value(serde_json::json!({
            "code_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "hash_type": "type",
            "args": args,
        }))
        .expect("script")
    }

    fn udt_asset() -> LiquidityAsset {
        LiquidityAsset {
            asset_id: "udt-test".to_string(),
            kind: LiquidityAssetKind::Udt,
            udt_type_script: Some(udt_script("0x01")),
            min_amount: 10,
            max_amount: 1_000,
            available_capacity: 500,
            base_fee: 2,
            proportional_fee_ppm: 1_000,
            enabled: true,
        }
    }

    #[test]
    fn validates_loop_out_quote_for_enabled_asset() {
        let asset = ckb_asset(true);
        let quote =
            validate_loop_out_quote_request(&asset, 100, 10, 10, None, 1_000, 2_000).unwrap();

        assert_eq!(quote.provider_fee, 2);
        assert_eq!(quote.routing_fee_limit, 10);
        assert_eq!(quote.expires_at, 2_000);
    }

    #[test]
    fn loop_out_quote_rejects_disabled_or_out_of_range_assets() {
        assert!(matches!(
            validate_loop_out_quote_request(&ckb_asset(false), 100, 10, 10, None, 1_000, 2_000),
            Err(crate::liquidity::types::LiquidityLoopOutError::AssetDisabled(_))
        ));
        assert!(matches!(
            validate_loop_out_quote_request(&ckb_asset(true), 1, 10, 10, None, 1_000, 2_000),
            Err(crate::liquidity::types::LiquidityLoopOutError::AmountOutOfRange { .. })
        ));
    }

    #[test]
    fn loop_out_quote_rejects_capacity_shortfall_and_udt_type_mismatch() {
        let asset = udt_asset();

        assert!(matches!(
            validate_loop_out_quote_request(
                &asset,
                600,
                10,
                10,
                asset.udt_type_script.as_ref(),
                1_000,
                2_000,
            ),
            Err(crate::liquidity::types::LiquidityLoopOutError::CapacityTooLow { .. })
        ));

        assert!(matches!(
            validate_loop_out_quote_request(
                &asset,
                100,
                10,
                10,
                Some(&udt_script("0x02")),
                1_000,
                2_000,
            ),
            Err(crate::liquidity::types::LiquidityLoopOutError::UdtTypeMismatch)
        ));
    }

    #[test]
    fn loop_out_quote_rejects_ckb_request_with_udt_type_script() {
        assert!(matches!(
            validate_loop_out_quote_request(
                &ckb_asset(true),
                100,
                10,
                10,
                Some(&udt_script("0x01")),
                1_000,
                2_000,
            ),
            Err(crate::liquidity::types::LiquidityLoopOutError::UdtTypeMismatch)
        ));
    }

    #[test]
    fn loop_out_quote_rejects_expired_provider_fee_cap_and_gross_overflow() {
        assert!(matches!(
            validate_loop_out_quote_request(&ckb_asset(true), 100, 10, 10, None, 2_000, 2_000),
            Err(crate::liquidity::types::LiquidityLoopOutError::QuoteExpired)
        ));
        assert!(matches!(
            validate_loop_out_quote_request(&ckb_asset(true), 100, 1, 10, None, 1_000, 2_000),
            Err(crate::liquidity::types::LiquidityLoopOutError::ProviderFeeTooHigh)
        ));

        let mut asset = ckb_asset(true);
        asset.max_amount = u128::MAX;
        asset.available_capacity = u128::MAX;
        assert!(matches!(
            validate_loop_out_quote_request(&asset, u128::MAX, 10, 10, None, 1_000, 2_000),
            Err(crate::liquidity::types::LiquidityLoopOutError::GrossAmountOverflow)
        ));
    }

    #[test]
    fn loop_out_quote_provider_fee_math_checks_overflow() {
        let mut asset = ckb_asset(true);
        asset.base_fee = 7;
        assert_eq!(compute_provider_fee(&asset, 2_000).unwrap(), 9);

        asset.proportional_fee_ppm = 1_000_000;
        assert!(matches!(
            compute_provider_fee(&asset, u128::MAX),
            Err(crate::liquidity::types::LiquidityLoopOutError::GrossAmountOverflow)
        ));
    }
}
