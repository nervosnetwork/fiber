//! Payment request models for client-side Loop Out execution.

use fiber_types::Hash256;

use crate::liquidity::types::{loop_out_gross_payment_amount, LiquidityLoopOutError};

/// Fiber payment request derived from accepted Loop Out terms.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutPaymentRequest {
    /// Payment hash identifying the HTLC/conditional payment.
    pub payment_hash: Hash256,
    /// Gross Fiber payment amount including provider and routing fee budgets.
    pub amount: u128,
    /// Maximum Fiber routing fee the client accepts for this payment.
    pub max_fee_amount: u128,
}

impl LoopOutPaymentRequest {
    /// Build a Loop Out payment request from the net on-chain amount and fee limits.
    pub fn new(
        payment_hash: Hash256,
        amount: u128,
        provider_fee: u128,
        routing_fee_limit: u128,
    ) -> Result<Self, LiquidityLoopOutError> {
        Ok(Self {
            payment_hash,
            amount: loop_out_gross_payment_amount(amount, provider_fee, routing_fee_limit)?,
            max_fee_amount: routing_fee_limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_request_uses_gross_amount_and_fee_cap() {
        let request = LoopOutPaymentRequest::new([1u8; 32].into(), 100, 2, 3).unwrap();

        assert_eq!(request.amount, 105);
        assert_eq!(request.max_fee_amount, 3);
    }
}
