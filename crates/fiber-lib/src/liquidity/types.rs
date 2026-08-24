use ckb_types::packed::{OutPoint, Script};
use fiber_types::{Hash256, LiquidityAsset, LiquiditySwapKind, LiquiditySwapState, Pubkey};
use thiserror::Error;

/// Errors returned while validating or executing Loop Out liquidity swaps.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum LiquidityLoopOutError {
    /// The requested liquidity asset is not registered locally.
    #[error("liquidity asset not found: {0}")]
    AssetNotFound(String),
    /// The requested liquidity asset exists but is not available for new swaps.
    #[error("liquidity asset disabled: {0}")]
    AssetDisabled(String),
    /// The requested swap amount is outside the asset's configured limits.
    #[error("loop out amount {amount} outside asset range {min}..={max}")]
    AmountOutOfRange {
        /// Requested swap amount.
        amount: u128,
        /// Minimum amount allowed for the asset.
        min: u128,
        /// Maximum amount allowed for the asset.
        max: u128,
    },
    /// The gross payment amount cannot be represented as a `u128`.
    #[error("loop out gross payment amount overflow")]
    GrossAmountOverflow,
    /// The provider fee exceeds the client's accepted maximum.
    #[error("provider fee exceeds client cap")]
    ProviderFeeTooHigh,
    /// The routing fee exceeds the client's accepted maximum.
    #[error("routing fee exceeds client cap")]
    RoutingFeeTooHigh,
    /// The asset does not have enough available capacity for the swap.
    #[error("liquidity asset capacity too low: available {available}, required {required}")]
    CapacityTooLow {
        /// Capacity currently available for the asset.
        available: u128,
        /// Capacity required by the requested swap.
        required: u128,
    },
    /// The requested UDT type script differs from the provider's asset registry.
    #[error("UDT type script does not match provider asset registry")]
    UdtTypeMismatch,
    /// The quote expired before it could be accepted or executed.
    #[error("quote expired")]
    QuoteExpired,
    /// The absolute refund lock time cannot be encoded as a CKB since timestamp.
    #[error("refund lock time overflows CKB since timestamp")]
    RefundLockTimeOverflow,
    /// The requested state transition is not valid for the Loop Out role.
    #[error("invalid loop out state transition from {from:?} to {to:?}")]
    InvalidStateTransition {
        /// Current liquidity swap state.
        from: LiquiditySwapState,
        /// Requested next liquidity swap state.
        to: LiquiditySwapState,
    },
    /// The Fiber payment failed before producing a settled preimage.
    #[error("payment failed: {0}")]
    PaymentFailed(String),
    /// A chain operation failed while building, broadcasting, or watching a transaction.
    #[error("chain operation failed: {0}")]
    Chain(String),
    /// A persistence operation failed while reading or updating swap state.
    #[error("store operation failed: {0}")]
    Store(String),
}

/// Role-specific Loop Out state transition rules.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum LoopOutRoleState {
    /// Client-side view of a Loop Out swap.
    Client,
    /// Provider-side view of a Loop Out swap.
    Provider,
}

impl LoopOutRoleState {
    /// Return whether this role may transition a Loop Out swap from `from` to `to`.
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
                    | (PaymentSettled, ClaimPending)
                    | (ClaimPending, Success)
                    | (PayoutLocked, RefundPending)
                    | (RefundPending, Refunded)
            ),
        }
    }
}

/// Compute the gross Fiber payment amount for a Loop Out swap.
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

/// Terms returned by a provider quote for a Loop Out swap.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutQuoteTerms {
    /// Unique quote identifier used as the initial swap identifier.
    pub quote_id: Hash256,
    /// Quoted swap direction.
    pub swap_kind: LiquiditySwapKind,
    /// Provider node public key.
    pub provider: Pubkey,
    /// Asset being swapped out.
    pub asset: LiquidityAsset,
    /// Net amount the client wants to receive on-chain.
    pub amount: u128,
    /// Provider fee charged in the quoted asset unit.
    pub provider_fee: u128,
    /// Maximum Fiber routing fee the client accepts.
    pub routing_fee_limit: u128,
    /// Estimated CKB transaction fee for the on-chain operation.
    pub onchain_fee_estimate_ckb: u64,
    /// CKB capacity required for the payout lock output.
    pub capacity_requirement_ckb: u64,
    /// Payment hash used for the Fiber payment and on-chain lock.
    pub payment_hash: Hash256,
    /// Provider-generated preimage revealed only through payment settlement.
    /// Always `None` on the client side.
    pub payment_preimage: Option<Hash256>,
    /// Quote expiration timestamp in milliseconds.
    pub expires_at: u64,
    /// Deadline timestamp by which the payout lock must be confirmed.
    pub payout_deadline: u64,
    /// Lock time after which the provider may refund the payout lock.
    pub refund_after_lock_time: u64,
    /// Client claimant lock used for the claim transaction.
    pub claimant_lock: Script,
    /// Provider refund lock used if the swap is not paid and claimed.
    pub refund_lock: Script,
    /// Client invoice paid by the provider in Loop In swaps.
    pub client_invoice: Option<String>,
}

/// On-chain payout lock location and confirmation metadata.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutPayoutLock {
    /// Outpoint of the payout lock output.
    pub outpoint: OutPoint,
    /// Block number or timestamp where the payout lock was confirmed, if known.
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
        assert!(!LoopOutRoleState::Client.next_allowed(
            LiquiditySwapState::PaymentInFlight,
            LiquiditySwapState::Failed,
        ));
    }

    #[test]
    fn loop_out_provider_states_require_claim_pending_before_success() {
        assert!(!LoopOutRoleState::Provider.next_allowed(
            LiquiditySwapState::PaymentSettled,
            LiquiditySwapState::Success,
        ));
        assert!(LoopOutRoleState::Provider.next_allowed(
            LiquiditySwapState::PaymentSettled,
            LiquiditySwapState::ClaimPending,
        ));
        assert!(LoopOutRoleState::Provider.next_allowed(
            LiquiditySwapState::ClaimPending,
            LiquiditySwapState::Success,
        ));
        assert!(LoopOutRoleState::Provider.next_allowed(
            LiquiditySwapState::PayoutLocked,
            LiquiditySwapState::RefundPending,
        ));
        assert!(LoopOutRoleState::Provider.next_allowed(
            LiquiditySwapState::RefundPending,
            LiquiditySwapState::Refunded,
        ));
    }

    #[test]
    fn loop_out_gross_payment_amount_checks_overflow() {
        assert_eq!(loop_out_gross_payment_amount(100, 2, 3).unwrap(), 105);
        assert!(loop_out_gross_payment_amount(u128::MAX, 1, 0).is_err());
    }
}
