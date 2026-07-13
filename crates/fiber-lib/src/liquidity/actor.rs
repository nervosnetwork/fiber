//! Liquidity actor boundary messages and Loop Out client guard helpers.

use fiber_types::{Hash256, LiquiditySwapState};

use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};

/// Messages accepted by the liquidity actor boundary.
#[derive(Debug)]
pub enum LiquidityActorMessage {
    /// Request a provider quote for a Loop Out swap.
    QuoteLoopOut(QuoteLoopOutCommand),
    /// Accept a previously issued Loop Out quote.
    AcceptLoopOut(AcceptLoopOutCommand),
    /// Resume orchestration for a persisted Loop Out swap.
    ResumeLoopOut(Hash256),
}

/// Command payload for requesting a Loop Out quote.
#[derive(Debug, Clone)]
pub struct QuoteLoopOutCommand {
    /// Asset identifier to quote.
    pub asset_id: String,
    /// Net amount the client wants to receive on-chain.
    pub amount: u128,
    /// Maximum provider fee the client accepts.
    pub max_provider_fee: u128,
    /// Maximum Fiber routing fee the client accepts.
    pub max_routing_fee: u128,
    /// Relative quote expiration requested by the client.
    pub expires_after_seconds: u64,
}

/// Command payload for accepting a Loop Out quote.
#[derive(Debug, Clone)]
pub struct AcceptLoopOutCommand {
    /// Provider quote terms being accepted.
    pub quote: LoopOutQuoteTerms,
}

/// Return whether the client may start the Fiber payment from `state`.
pub fn client_can_start_payment(state: LiquiditySwapState) -> bool {
    state == LiquiditySwapState::PayoutLocked
}

/// Return whether the client may claim the payout lock from `state`.
pub fn client_can_claim(state: LiquiditySwapState) -> bool {
    state == LiquiditySwapState::PaymentSettled
}

/// Ensure the client may start the Fiber payment from `state`.
pub fn ensure_client_can_start_payment(
    state: LiquiditySwapState,
) -> Result<(), LiquidityLoopOutError> {
    if client_can_start_payment(state) {
        Ok(())
    } else {
        Err(LiquidityLoopOutError::InvalidStateTransition {
            from: state,
            to: LiquiditySwapState::PaymentInFlight,
        })
    }
}

#[cfg(test)]
mod tests {
    use fiber_types::LiquiditySwapState;

    use super::*;

    #[test]
    fn client_payment_requires_payout_locked_state() {
        assert!(client_can_start_payment(LiquiditySwapState::PayoutLocked));
        assert!(!client_can_start_payment(LiquiditySwapState::Quoted));
    }

    #[test]
    fn client_claim_requires_payment_settled_state() {
        assert!(client_can_claim(LiquiditySwapState::PaymentSettled));
        assert!(!client_can_claim(LiquiditySwapState::PaymentInFlight));
    }

    #[test]
    fn ensure_client_payment_reports_invalid_state_transition() {
        assert!(ensure_client_can_start_payment(LiquiditySwapState::PayoutLocked).is_ok());
        assert_eq!(
            ensure_client_can_start_payment(LiquiditySwapState::Quoted),
            Err(LiquidityLoopOutError::InvalidStateTransition {
                from: LiquiditySwapState::Quoted,
                to: LiquiditySwapState::PaymentInFlight,
            })
        );
    }
}
