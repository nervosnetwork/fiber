use ckb_types::packed::{OutPoint, Script};
use fiber_types::{Hash256, LiquidityAsset, LiquiditySwapState, Pubkey};
use thiserror::Error;

#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum LiquidityLoopOutError {
    #[error("liquidity asset not found: {0}")]
    AssetNotFound(String),
    #[error("liquidity asset disabled: {0}")]
    AssetDisabled(String),
    #[error("loop out amount {amount} outside asset range {min}..={max}")]
    AmountOutOfRange { amount: u128, min: u128, max: u128 },
    #[error("loop out gross payment amount overflow")]
    GrossAmountOverflow,
    #[error("provider fee exceeds client cap")]
    ProviderFeeTooHigh,
    #[error("routing fee exceeds client cap")]
    RoutingFeeTooHigh,
    #[error("liquidity asset capacity too low: available {available}, required {required}")]
    CapacityTooLow { available: u128, required: u128 },
    #[error("UDT type script does not match provider asset registry")]
    UdtTypeMismatch,
    #[error("quote expired")]
    QuoteExpired,
    #[error("invalid loop out state transition from {from:?} to {to:?}")]
    InvalidStateTransition {
        from: LiquiditySwapState,
        to: LiquiditySwapState,
    },
    #[error("payment failed: {0}")]
    PaymentFailed(String),
    #[error("chain operation failed: {0}")]
    Chain(String),
    #[error("store operation failed: {0}")]
    Store(String),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum LoopOutRoleState {
    Client,
    Provider,
}

impl LoopOutRoleState {
    pub fn next_allowed(self, from: LiquiditySwapState, to: LiquiditySwapState) -> bool {
        use LiquiditySwapState::*;
        match self {
            Self::Client => matches!(
                (from, to),
                (Created, Quoted)
                    | (Quoted, PayoutPending)
                    | (PayoutPending, PayoutLocked)
                    | (PayoutLocked, PaymentInFlight)
                    | (PaymentInFlight, PaymentSettled)
                    | (PaymentInFlight, Failed)
                    | (PaymentSettled, ClaimPending)
                    | (ClaimPending, Success)
            ),
            Self::Provider => matches!(
                (from, to),
                (Created, Quoted)
                    | (Quoted, PayoutPending)
                    | (PayoutPending, PayoutLocked)
                    | (PayoutLocked, PaymentInFlight)
                    | (PaymentInFlight, PaymentSettled)
                    | (PaymentSettled, Success)
                    | (PayoutLocked, RefundPending)
                    | (RefundPending, Refunded)
            ),
        }
    }
}

pub fn loop_out_gross_payment_amount(
    amount: u128,
    provider_fee: u128,
    routing_fee: u128,
) -> Result<u128, LiquidityLoopOutError> {
    amount
        .checked_add(provider_fee)
        .and_then(|v| v.checked_add(routing_fee))
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutQuoteTerms {
    pub quote_id: Hash256,
    pub provider: Pubkey,
    pub asset: LiquidityAsset,
    pub amount: u128,
    pub provider_fee: u128,
    pub routing_fee_limit: u128,
    pub onchain_fee_estimate_ckb: u64,
    pub capacity_requirement_ckb: u64,
    pub payment_hash: Hash256,
    pub expires_at: u64,
    pub payout_deadline: u64,
    pub refund_after_lock_time: u64,
    pub claimant_lock: Script,
    pub refund_lock: Script,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutPayoutLock {
    pub outpoint: OutPoint,
    pub confirmed_at: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_types::LiquiditySwapState;

    #[test]
    fn loop_out_client_states_require_payout_before_payment() {
        assert!(LoopOutRoleState::Client.next_allowed(
            LiquiditySwapState::PayoutLocked,
            LiquiditySwapState::PaymentInFlight,
        ));
        assert!(!LoopOutRoleState::Client.next_allowed(
            LiquiditySwapState::Quoted,
            LiquiditySwapState::PaymentInFlight,
        ));
    }

    #[test]
    fn loop_out_gross_payment_amount_checks_overflow() {
        assert_eq!(loop_out_gross_payment_amount(100, 2, 3).unwrap(), 105);
        assert!(loop_out_gross_payment_amount(u128::MAX, 1, 0).is_err());
    }
}
