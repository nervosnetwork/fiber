use ckb_types::packed::Script;
use ckb_types::prelude::Entity;
use fiber_json_types::{LiquidityAssetInfo, LiquidityQuoteEnvelope};
use fiber_types::{Hash256, LiquidityAsset, LiquidityAssetKind, LiquiditySwapKind, Pubkey};

use crate::invoice::CkbInvoice;
use crate::liquidity::types::{
    loop_out_gross_payment_amount, LiquidityLoopOutError, LoopOutQuoteTerms,
};

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

/// Compute the gross on-chain amount required for a Loop In swap.
pub fn loop_in_gross_onchain_amount(
    quote: &LoopOutQuoteTerms,
) -> Result<u128, LiquidityLoopOutError> {
    quote
        .amount
        .checked_add(quote.provider_fee)
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}

fn parse_client_invoice(client_invoice: &str) -> Result<CkbInvoice, LiquidityLoopOutError> {
    client_invoice.parse::<CkbInvoice>().map_err(|error| {
        LiquidityLoopOutError::PaymentFailed(format!("invalid client invoice: {error}"))
    })
}

fn validate_loop_in_invoice(
    invoice: &CkbInvoice,
    amount: u128,
    expected_udt_type_script: Option<&ckb_jsonrpc_types::Script>,
) -> Result<(), LiquidityLoopOutError> {
    if invoice.amount() != Some(amount) {
        return Err(LiquidityLoopOutError::PaymentFailed(format!(
            "invoice amount {:?} does not match requested amount {amount}",
            invoice.amount()
        )));
    }

    let invoice_udt_type_script: Option<ckb_jsonrpc_types::Script> =
        invoice.udt_type_script().cloned().map(Into::into);
    if invoice_udt_type_script.as_ref() != expected_udt_type_script {
        return Err(LiquidityLoopOutError::UdtTypeMismatch);
    }

    Ok(())
}

pub(crate) fn parse_script_hex(value: &str, field: &str) -> Result<Script, LiquidityLoopOutError> {
    let Some(hex_value) = value.strip_prefix("0x") else {
        return Err(LiquidityLoopOutError::Store(format!(
            "invalid {field}: script hex must start with 0x"
        )));
    };
    let bytes = hex::decode(hex_value).map_err(|error| {
        LiquidityLoopOutError::Store(format!(
            "invalid {field}: script hex decode failed: {error}"
        ))
    })?;
    Script::from_slice(&bytes).map_err(|error| {
        LiquidityLoopOutError::Store(format!("invalid {field}: script decode failed: {error}"))
    })
}

pub(crate) fn script_hex(script: &Script) -> String {
    format!("0x{}", hex::encode(script.as_slice()))
}

pub(crate) fn json_asset_to_liquidity_asset(
    info: &LiquidityAssetInfo,
) -> Result<LiquidityAsset, LiquidityLoopOutError> {
    if info.asset_id.trim().is_empty() {
        return Err(LiquidityLoopOutError::Store(
            "asset_id must not be empty".to_string(),
        ));
    }
    let asset = LiquidityAsset {
        asset_id: info.asset_id.clone(),
        kind: match info.kind {
            fiber_json_types::LiquidityAssetKind::Ckb => LiquidityAssetKind::Ckb,
            fiber_json_types::LiquidityAssetKind::Udt => LiquidityAssetKind::Udt,
        },
        udt_type_script: info.udt_type_script.clone(),
        min_amount: info.min_amount,
        max_amount: info.max_amount,
        available_capacity: info.available_capacity,
        base_fee: info.base_fee,
        proportional_fee_ppm: info.proportional_fee_ppm,
        enabled: info.enabled,
    };
    asset
        .validate()
        .map_err(|error| LiquidityLoopOutError::Store(error.to_string()))?;
    Ok(asset)
}

pub(crate) fn liquidity_asset_to_json_info(asset: &LiquidityAsset) -> LiquidityAssetInfo {
    LiquidityAssetInfo {
        asset_id: asset.asset_id.clone(),
        kind: match asset.kind {
            LiquidityAssetKind::Ckb => fiber_json_types::LiquidityAssetKind::Ckb,
            LiquidityAssetKind::Udt => fiber_json_types::LiquidityAssetKind::Udt,
        },
        udt_type_script: asset.udt_type_script.clone(),
        min_amount: asset.min_amount,
        max_amount: asset.max_amount,
        available_capacity: asset.available_capacity,
        base_fee: asset.base_fee,
        proportional_fee_ppm: asset.proportional_fee_ppm,
        enabled: asset.enabled,
    }
}

/// Convert persisted internal quote terms to their lossless transport envelope.
pub fn liquidity_quote_envelope_from_terms(terms: &LoopOutQuoteTerms) -> LiquidityQuoteEnvelope {
    LiquidityQuoteEnvelope {
        quote_id: terms.quote_id.into(),
        swap_kind: match terms.swap_kind {
            LiquiditySwapKind::LoopOut => fiber_json_types::LiquiditySwapKind::LoopOut,
            LiquiditySwapKind::LoopIn => fiber_json_types::LiquiditySwapKind::LoopIn,
        },
        provider_pubkey: fiber_json_types::Pubkey(terms.provider.0),
        asset: liquidity_asset_to_json_info(&terms.asset),
        amount: terms.amount,
        provider_fee: terms.provider_fee,
        routing_fee_limit: terms.routing_fee_limit,
        onchain_fee_estimate_ckb: terms.onchain_fee_estimate_ckb,
        capacity_requirement_ckb: terms.capacity_requirement_ckb,
        payment_hash: terms.payment_hash.into(),
        expires_at: terms.expires_at,
        payout_deadline: terms.payout_deadline,
        refund_after_lock_time: terms.refund_after_lock_time,
        claimant_lock: script_hex(&terms.claimant_lock),
        refund_lock: script_hex(&terms.refund_lock),
        client_invoice: terms.client_invoice.clone(),
    }
}

/// Validate an independently received quote envelope and convert it to internal terms.
///
/// The quote expiration must be strictly greater than `now_ms`. Provider and routing fees may
/// equal their respective caps. Malformed provider keys, assets, scripts, and Loop In invoices are
/// rejected. This function only validates and converts the quote; it does not persist any state.
pub fn validate_imported_quote(
    envelope: LiquidityQuoteEnvelope,
    max_provider_fee: u128,
    max_routing_fee: u128,
    now_ms: u64,
) -> Result<LoopOutQuoteTerms, LiquidityLoopOutError> {
    if envelope.expires_at <= now_ms {
        return Err(LiquidityLoopOutError::QuoteExpired);
    }
    if envelope.provider_fee > max_provider_fee {
        return Err(LiquidityLoopOutError::ProviderFeeTooHigh);
    }
    if envelope.routing_fee_limit > max_routing_fee {
        return Err(LiquidityLoopOutError::RoutingFeeTooHigh);
    }

    let asset = json_asset_to_liquidity_asset(&envelope.asset)?;
    let claimant_lock = parse_script_hex(&envelope.claimant_lock, "claimant_lock")?;
    let refund_lock = parse_script_hex(&envelope.refund_lock, "refund_lock")?;
    let swap_kind = match envelope.swap_kind {
        fiber_json_types::LiquiditySwapKind::LoopOut => LiquiditySwapKind::LoopOut,
        fiber_json_types::LiquiditySwapKind::LoopIn => LiquiditySwapKind::LoopIn,
    };
    let payment_hash: Hash256 = envelope.payment_hash.into();
    let provider = Pubkey::try_from(envelope.provider_pubkey).map_err(|error| {
        LiquidityLoopOutError::Store(format!("invalid provider pubkey: {error}"))
    })?;

    match swap_kind {
        LiquiditySwapKind::LoopOut => {
            if claimant_lock == Script::default() {
                return Err(LiquidityLoopOutError::Store(
                    "invalid claimant_lock: Loop Out requires a non-default script".to_string(),
                ));
            }
            if refund_lock == Script::default() {
                return Err(LiquidityLoopOutError::Store(
                    "invalid refund_lock: Loop Out requires a non-default script".to_string(),
                ));
            }
            if envelope.client_invoice.is_some() {
                return Err(LiquidityLoopOutError::PaymentFailed(
                    "Loop Out quote must not include client invoice".to_string(),
                ));
            }
        }
        LiquiditySwapKind::LoopIn => {
            let invoice_text = envelope.client_invoice.as_deref().ok_or_else(|| {
                LiquidityLoopOutError::PaymentFailed(
                    "Loop In quote requires client invoice".to_string(),
                )
            })?;
            let invoice = parse_client_invoice(invoice_text)?;
            if invoice.payment_hash() != &payment_hash {
                return Err(LiquidityLoopOutError::PaymentFailed(
                    "invoice payment hash does not match quote payment hash".to_string(),
                ));
            }
            validate_loop_in_invoice(&invoice, envelope.amount, asset.udt_type_script.as_ref())?;
        }
    }

    let terms = LoopOutQuoteTerms {
        quote_id: envelope.quote_id.into(),
        swap_kind,
        provider,
        asset,
        amount: envelope.amount,
        provider_fee: envelope.provider_fee,
        routing_fee_limit: envelope.routing_fee_limit,
        onchain_fee_estimate_ckb: envelope.onchain_fee_estimate_ckb,
        capacity_requirement_ckb: envelope.capacity_requirement_ckb,
        payment_hash,
        expires_at: envelope.expires_at,
        payout_deadline: envelope.payout_deadline,
        refund_after_lock_time: envelope.refund_after_lock_time,
        claimant_lock,
        refund_lock,
        client_invoice: envelope.client_invoice,
    };
    match terms.swap_kind {
        LiquiditySwapKind::LoopOut => {
            loop_out_gross_payment_amount(
                terms.amount,
                terms.provider_fee,
                terms.routing_fee_limit,
            )?;
        }
        LiquiditySwapKind::LoopIn => {
            loop_in_gross_onchain_amount(&terms)?;
        }
    }
    Ok(terms)
}

/// Build quote terms for a Loop In request after provider-side validation.
pub fn build_loop_in_quote_terms(
    quote_id: Hash256,
    provider: Pubkey,
    asset: &LiquidityAsset,
    amount: u128,
    requested_udt_type_script: Option<&ckb_jsonrpc_types::Script>,
    client_invoice: String,
    routing_fee_limit: u128,
    expires_at: u64,
    onchain_fee_estimate_ckb: u64,
) -> Result<LoopOutQuoteTerms, LiquidityLoopOutError> {
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

    let expected_udt_type_script = match asset.kind {
        LiquidityAssetKind::Ckb => None,
        LiquidityAssetKind::Udt => asset.udt_type_script.as_ref(),
    };
    if expected_udt_type_script != requested_udt_type_script {
        return Err(LiquidityLoopOutError::UdtTypeMismatch);
    }

    let invoice = parse_client_invoice(&client_invoice)?;
    validate_loop_in_invoice(&invoice, amount, expected_udt_type_script)?;

    let provider_fee = compute_provider_fee(asset, amount)?;
    let gross_amount = amount
        .checked_add(provider_fee)
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)?;
    let capacity_requirement_ckb = match asset.kind {
        LiquidityAssetKind::Ckb => u64::try_from(gross_amount)
            .map_err(|_| LiquidityLoopOutError::GrossAmountOverflow)?
            .max(1),
        LiquidityAssetKind::Udt => onchain_fee_estimate_ckb.max(1),
    };
    let quote = LoopOutQuoteTerms {
        quote_id,
        swap_kind: LiquiditySwapKind::LoopIn,
        provider,
        asset: asset.clone(),
        amount,
        provider_fee,
        routing_fee_limit,
        onchain_fee_estimate_ckb,
        capacity_requirement_ckb,
        payment_hash: *invoice.payment_hash(),
        expires_at,
        payout_deadline: expires_at,
        refund_after_lock_time: expires_at,
        claimant_lock: Default::default(),
        refund_lock: Default::default(),
        client_invoice: Some(client_invoice),
    };

    if gross_amount > asset.available_capacity {
        return Err(LiquidityLoopOutError::CapacityTooLow {
            available: asset.available_capacity,
            required: gross_amount,
        });
    }

    Ok(quote)
}

#[cfg(test)]
mod tests {
    use ckb_jsonrpc_types::Script;
    use ckb_types::packed;
    use ckb_types::prelude::Entity;
    use fiber_json_types::LiquidityQuoteEnvelope;
    use fiber_types::{Hash256, LiquidityAsset, LiquidityAssetKind, Pubkey};
    use secp256k1::Secp256k1;

    use crate::gen_deterministic_secp256k1_keypair_tuple;
    use crate::invoice::{Currency, InvoiceBuilder};

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

    fn client_invoice(
        payment_hash: Hash256,
        amount: Option<u128>,
        udt_type_script: Option<ckb_types::packed::Script>,
    ) -> crate::invoice::CkbInvoice {
        let (private_key, public_key) = gen_deterministic_secp256k1_keypair_tuple();
        let mut builder = InvoiceBuilder::new(Currency::Fibb)
            .amount(amount)
            .payment_hash(payment_hash)
            .payee_pub_key(public_key);
        if let Some(script) = udt_type_script {
            builder = builder.udt_type_script(script);
        }
        builder
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .expect("invoice")
    }

    fn ckb_client_invoice(payment_hash: Hash256) -> crate::invoice::CkbInvoice {
        client_invoice(payment_hash, Some(1_000), None)
    }

    fn imported_quote_terms(
        swap_kind: fiber_types::LiquiditySwapKind,
        asset: LiquidityAsset,
    ) -> LoopOutQuoteTerms {
        let payment_hash = Hash256::from([3; 32]);
        let (_, provider_public_key) = gen_deterministic_secp256k1_keypair_tuple();
        let client_invoice = (swap_kind == fiber_types::LiquiditySwapKind::LoopIn).then(|| {
            client_invoice(
                payment_hash,
                Some(100),
                asset.udt_type_script.clone().map(Into::into),
            )
            .to_string()
        });
        LoopOutQuoteTerms {
            quote_id: Hash256::from([1; 32]),
            swap_kind,
            provider: provider_public_key.into(),
            asset,
            amount: 100,
            provider_fee: 2,
            routing_fee_limit: 3,
            onchain_fee_estimate_ckb: 4,
            capacity_requirement_ckb: 5,
            payment_hash,
            expires_at: 20_000,
            payout_deadline: 17_777,
            refund_after_lock_time: 30_000,
            claimant_lock: udt_script("0x11").into(),
            refund_lock: udt_script("0x22").into(),
            client_invoice,
        }
    }

    fn imported_quote_envelope(
        swap_kind: fiber_types::LiquiditySwapKind,
        asset: LiquidityAsset,
    ) -> LiquidityQuoteEnvelope {
        liquidity_quote_envelope_from_terms(&imported_quote_terms(swap_kind, asset))
    }

    fn assert_imported_quote_rejected(envelope: LiquidityQuoteEnvelope, message: &str) {
        let error = validate_imported_quote(envelope, 2, 3, 10_000).unwrap_err();
        assert!(
            error.to_string().contains(message),
            "expected error containing {message:?}, got {error}"
        );
    }

    #[test]
    fn imported_quote_round_trips_loop_out_ckb_and_preserves_payout_deadline() {
        let terms = imported_quote_terms(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        let envelope = liquidity_quote_envelope_from_terms(&terms);
        assert_eq!(envelope.payout_deadline, 17_777);
        assert_eq!(
            validate_imported_quote(envelope, 2, 3, 10_000).unwrap(),
            terms
        );
    }

    #[test]
    fn imported_quote_round_trips_loop_out_udt_with_exact_script() {
        let terms = imported_quote_terms(fiber_types::LiquiditySwapKind::LoopOut, udt_asset());
        let envelope = liquidity_quote_envelope_from_terms(&terms);
        assert_eq!(envelope.asset.udt_type_script, terms.asset.udt_type_script);
        assert_eq!(
            validate_imported_quote(envelope, 2, 3, 10_000).unwrap(),
            terms
        );
    }

    #[test]
    fn imported_quote_round_trips_loop_in_ckb_invoice_and_payout_deadline() {
        let terms = imported_quote_terms(fiber_types::LiquiditySwapKind::LoopIn, ckb_asset(true));
        let envelope = liquidity_quote_envelope_from_terms(&terms);
        assert_eq!(envelope.client_invoice, terms.client_invoice);
        assert_eq!(envelope.payout_deadline, 17_777);
        assert_eq!(
            validate_imported_quote(envelope, 2, 3, 10_000).unwrap(),
            terms
        );
    }

    #[test]
    fn imported_quote_round_trips_loop_in_udt_with_matching_invoice_udt() {
        let terms = imported_quote_terms(fiber_types::LiquiditySwapKind::LoopIn, udt_asset());
        let envelope = liquidity_quote_envelope_from_terms(&terms);
        assert_eq!(
            validate_imported_quote(envelope, 2, 3, 10_000).unwrap(),
            terms
        );
    }

    #[test]
    fn imported_quote_rejects_expiration_and_fee_caps() {
        let mut expired =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        expired.expires_at = 10_000;
        assert_imported_quote_rejected(expired, "expired");

        let mut provider_fee =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        provider_fee.provider_fee = 3;
        assert_imported_quote_rejected(provider_fee, "provider fee");

        let mut routing_fee =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        routing_fee.routing_fee_limit = 4;
        assert_imported_quote_rejected(routing_fee, "routing fee");
    }

    #[test]
    fn imported_quote_rejects_invalid_provider_pubkey() {
        let mut envelope =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        let mut invalid_compressed_key = [0xff; 33];
        invalid_compressed_key[0] = 0x02;
        envelope.provider_pubkey = fiber_json_types::Pubkey(invalid_compressed_key);

        assert_imported_quote_rejected(envelope, "invalid provider pubkey");
    }

    #[test]
    fn imported_quote_rejects_malformed_claimant_and_refund_scripts() {
        let mut claimant =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        claimant.claimant_lock = "0xzz".to_string();
        assert_imported_quote_rejected(claimant, "claimant_lock");

        let mut refund =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        refund.refund_lock = "0x00".to_string();
        assert_imported_quote_rejected(refund, "refund_lock");
    }

    #[test]
    fn imported_quote_rejects_invalid_asset_script_combinations() {
        let script = udt_script("0x01");
        let mut ckb =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        ckb.asset.udt_type_script = Some(script);
        assert_imported_quote_rejected(ckb, "CKB liquidity asset");

        let mut udt = imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, udt_asset());
        udt.asset.udt_type_script = None;
        assert_imported_quote_rejected(udt, "UDT liquidity asset");
    }

    #[test]
    fn imported_quote_rejects_checked_gross_amount_overflow() {
        let mut loop_out =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        loop_out.amount = u128::MAX;
        assert_imported_quote_rejected(loop_out, "overflow");

        let mut loop_in =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopIn, ckb_asset(true));
        loop_in.amount = u128::MAX;
        loop_in.client_invoice =
            Some(client_invoice(loop_in.payment_hash.into(), Some(u128::MAX), None).to_string());
        assert_imported_quote_rejected(loop_in, "overflow");
    }

    #[test]
    fn imported_quote_rejects_invalid_loop_out_direction_fields() {
        let mut missing_claimant =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        missing_claimant.claimant_lock =
            format!("0x{}", hex::encode(packed::Script::default().as_slice()));
        assert_imported_quote_rejected(missing_claimant, "claimant_lock");

        let mut missing_refund =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        missing_refund.refund_lock =
            format!("0x{}", hex::encode(packed::Script::default().as_slice()));
        assert_imported_quote_rejected(missing_refund, "refund_lock");

        let mut invoice =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopOut, ckb_asset(true));
        invoice.client_invoice = Some("unexpected".to_string());
        assert_imported_quote_rejected(invoice, "must not include client invoice");
    }

    #[test]
    fn imported_quote_rejects_missing_or_mismatched_loop_in_invoice() {
        let mut missing =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopIn, ckb_asset(true));
        missing.client_invoice = None;
        assert_imported_quote_rejected(missing, "requires client invoice");

        let mut payment_hash =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopIn, ckb_asset(true));
        payment_hash.client_invoice = Some(ckb_client_invoice(Hash256::from([9; 32])).to_string());
        assert_imported_quote_rejected(payment_hash, "payment hash");

        let mut amount =
            imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopIn, ckb_asset(true));
        amount.client_invoice =
            Some(client_invoice(amount.payment_hash.into(), Some(99), None).to_string());
        assert_imported_quote_rejected(amount, "invoice amount");

        let mut udt = imported_quote_envelope(fiber_types::LiquiditySwapKind::LoopIn, udt_asset());
        udt.client_invoice = Some(
            client_invoice(
                udt.payment_hash.into(),
                Some(100),
                Some(udt_script("0x02").into()),
            )
            .to_string(),
        );
        assert_imported_quote_rejected(udt, "UDT type script");
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

    #[test]
    fn loop_in_quote_uses_gross_onchain_amount_for_capacity_checks() {
        let mut asset = ckb_asset(true);
        asset.available_capacity = 1_050;
        asset.proportional_fee_ppm = 0;

        let quote = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &asset,
            1_000,
            None,
            ckb_client_invoice(Hash256::from([3; 32])).to_string(),
            0,
            60_000,
            1,
        )
        .expect("loop in quote should fit gross amount");

        assert_eq!(quote.amount, 1_000);
        assert_eq!(quote.provider_fee, asset.base_fee);
        assert_eq!(
            loop_in_gross_onchain_amount(&quote).unwrap(),
            1_000 + asset.base_fee
        );
        assert_eq!(quote.payout_deadline, quote.expires_at);
    }

    #[test]
    fn loop_in_quote_sets_nonzero_capacity_requirement() {
        let mut asset = ckb_asset(true);
        asset.proportional_fee_ppm = 0;

        let quote = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &asset,
            1_000,
            None,
            ckb_client_invoice(Hash256::from([3; 32])).to_string(),
            0,
            60_000,
            1,
        )
        .expect("loop in quote");

        assert_ne!(quote.capacity_requirement_ckb, 0);
        assert_eq!(
            quote.capacity_requirement_ckb,
            loop_in_gross_onchain_amount(&quote).unwrap() as u64
        );
    }

    #[test]
    fn loop_in_quote_keeps_ckb_capacity_nonzero_for_zero_gross_amount() {
        let mut asset = ckb_asset(true);
        asset.min_amount = 0;
        asset.base_fee = 0;
        asset.proportional_fee_ppm = 0;

        let quote = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &asset,
            0,
            None,
            client_invoice(Hash256::from([3; 32]), Some(0), None).to_string(),
            0,
            60_000,
            1,
        )
        .expect("loop in quote");

        assert_eq!(loop_in_gross_onchain_amount(&quote).unwrap(), 0);
        assert!(quote.capacity_requirement_ckb > 0);
    }

    #[test]
    fn loop_in_quote_uses_payment_hash_from_client_invoice() {
        let payment_hash = Hash256::from([3; 32]);
        let client_invoice = ckb_client_invoice(payment_hash);

        let quote = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &ckb_asset(true),
            1_000,
            None,
            client_invoice.to_string(),
            0,
            60_000,
            1,
        )
        .expect("loop in quote");

        assert_eq!(quote.payment_hash, *client_invoice.payment_hash());
    }

    #[test]
    fn loop_in_quote_persists_client_invoice_for_provider_payment_recovery() {
        let payment_hash = Hash256::from([8; 32]);
        let client_invoice = ckb_client_invoice(payment_hash).to_string();

        let quote = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &ckb_asset(true),
            1_000,
            None,
            client_invoice.clone(),
            0,
            60_000,
            1,
        )
        .expect("loop in quote");

        assert_eq!(quote.client_invoice, Some(client_invoice));
    }

    #[test]
    fn loop_in_quote_preserves_requested_routing_fee_limit() {
        let quote = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &ckb_asset(true),
            1_000,
            None,
            ckb_client_invoice(Hash256::from([3; 32])).to_string(),
            23,
            60_000,
            1,
        )
        .expect("loop in quote");

        assert_eq!(quote.routing_fee_limit, 23);
    }

    #[test]
    fn loop_in_quote_rejects_invoice_amount_mismatch() {
        let err = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &ckb_asset(true),
            1_000,
            None,
            client_invoice(Hash256::from([3; 32]), Some(999), None).to_string(),
            0,
            60_000,
            1,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invoice amount"));

        let err = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &ckb_asset(true),
            1_000,
            None,
            client_invoice(Hash256::from([3; 32]), None, None).to_string(),
            0,
            60_000,
            1,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invoice amount"));
    }

    #[test]
    fn loop_in_quote_rejects_invoice_udt_type_mismatch() {
        let ckb_err = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &ckb_asset(true),
            1_000,
            None,
            client_invoice(
                Hash256::from([3; 32]),
                Some(1_000),
                Some(udt_script("0x01").into()),
            )
            .to_string(),
            0,
            60_000,
            1,
        )
        .unwrap_err();
        assert!(matches!(
            ckb_err,
            crate::liquidity::types::LiquidityLoopOutError::UdtTypeMismatch
        ));

        let asset = udt_asset();
        let udt_err = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &asset,
            1_000,
            asset.udt_type_script.as_ref(),
            client_invoice(
                Hash256::from([3; 32]),
                Some(1_000),
                Some(udt_script("0x02").into()),
            )
            .to_string(),
            0,
            60_000,
            1,
        )
        .unwrap_err();
        assert!(matches!(
            udt_err,
            crate::liquidity::types::LiquidityLoopOutError::UdtTypeMismatch
        ));
    }

    #[test]
    fn loop_in_quote_rejects_gross_amount_overflow_and_capacity_shortfall() {
        let mut asset = ckb_asset(true);
        asset.max_amount = u128::MAX;
        asset.available_capacity = u128::MAX;
        asset.base_fee = 2;
        asset.proportional_fee_ppm = 0;

        let overflow = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &asset,
            u128::MAX,
            None,
            client_invoice(Hash256::from([3; 32]), Some(u128::MAX), None).to_string(),
            0,
            60_000,
            1,
        )
        .unwrap_err();
        assert!(matches!(
            overflow,
            crate::liquidity::types::LiquidityLoopOutError::GrossAmountOverflow
        ));

        asset.max_amount = 1_000;
        asset.available_capacity = 1_001;
        let shortfall = build_loop_in_quote_terms(
            Hash256::from([1; 32]),
            Pubkey([2; 33]),
            &asset,
            1_000,
            None,
            ckb_client_invoice(Hash256::from([3; 32])).to_string(),
            0,
            60_000,
            1,
        )
        .unwrap_err();
        assert!(shortfall.to_string().contains("capacity"));
    }
}
