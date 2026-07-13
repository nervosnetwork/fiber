# Fiber Liquidity M3 Loop Out Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement a real manual Loop Out path between a client FNN and provider FNN using RPC coordination, M2 persistence, Fiber payments, and liquidity-lock on-chain payout/claim flow.

**Architecture:** Add a `LiquidityActor` under `crates/fiber-lib/src/liquidity/` as the orchestration boundary. RPC handlers stay thin and call the actor; the actor owns quote validation, state transitions, payment/chain adapter calls, and restart recovery.

**Tech Stack:** Rust, Ractor, jsonrpsee, existing Fiber payment commands, existing CKB actor/client APIs, M2 `LiquidityStore`, M1 liquidity-lock builders, cargo test/nextest.

---

## File Structure

- Create `crates/fiber-lib/src/liquidity/types.rs`: M3-only quote/order command types, provider quote reservation types, adapter traits, and errors.
- Create `crates/fiber-lib/src/liquidity/quote.rs`: Loop Out quote validation, fee math, expiry checks, and preimage/payment-hash generation helpers.
- Create `crates/fiber-lib/src/liquidity/actor.rs`: `LiquidityActor`, actor messages, state machine orchestration, and recovery entry point.
- Create `crates/fiber-lib/src/liquidity/payment.rs`: adapter around existing Fiber payment commands/status rehydration.
- Create `crates/fiber-lib/src/liquidity/chain.rs`: adapter around liquidity-lock payout/claim/refund construction and CKB broadcast/watch interfaces.
- Modify `crates/fiber-lib/src/liquidity/mod.rs`: export new modules.
- Create `crates/fiber-lib/src/rpc/liquidity.rs`: client and provider JSON-RPC methods.
- Modify `crates/fiber-lib/src/rpc/mod.rs`: register liquidity RPC module and include `LiquidityStore` in `RpcServerStore` bounds.
- Modify `crates/fiber-json-types/src/liquidity.rs`: add any missing request/response DTO fields needed by provider quote/accept RPCs.
- Modify `crates/fiber-lib/src/store/tests/store.rs` only if M3 needs extra persisted fields not covered by M2.
- Add unit tests beside each new liquidity module and integration-style tests under `crates/fiber-lib/src/tests/` or `crates/fiber-lib/src/liquidity/actor.rs` test module, following existing actor test patterns.

---

## Decomposition Note

M3 is product-complete and crosses RPC, persistence, payment, chain, and recovery. Execute these tasks sequentially. Do not dispatch implementation subagents in parallel because later tasks depend on types and actor boundaries from earlier tasks.

---

### Task 1: Define Loop Out Domain And Adapter Boundaries

**Files:**
- Create: `crates/fiber-lib/src/liquidity/types.rs`
- Modify: `crates/fiber-lib/src/liquidity/mod.rs`
- Test: `crates/fiber-lib/src/liquidity/types.rs`

- [ ] **Step 1: Write the failing type/transition tests**

Create `crates/fiber-lib/src/liquidity/types.rs` with tests first:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb loop_out_client_states_require_payout_before_payment -- --nocapture`

Expected: FAIL because `LoopOutRoleState` and `loop_out_gross_payment_amount` do not exist.

- [ ] **Step 3: Add minimal domain types and errors**

Add this implementation above the tests in `types.rs`:

```rust
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
    InvalidStateTransition { from: LiquiditySwapState, to: LiquiditySwapState },
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
```

Update `crates/fiber-lib/src/liquidity/mod.rs`:

```rust
pub mod types;
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p fnn --features rocksdb loop_out_ -- --nocapture`

Expected: PASS for the new tests.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/mod.rs crates/fiber-lib/src/liquidity/types.rs
git commit -m "feat: add loop out domain types"
```

---

### Task 2: Implement Quote Validation And Preimage Generation

> Task 2 correction: this slice implements quote validation only. Preimage/payment-hash generation helpers are deferred until an actor or payment task needs a concrete API.

**Files:**
- Create: `crates/fiber-lib/src/liquidity/quote.rs`
- Modify: `crates/fiber-lib/src/liquidity/mod.rs`
- Test: `crates/fiber-lib/src/liquidity/quote.rs`

- [ ] **Step 1: Write failing quote validation tests**

Create `crates/fiber-lib/src/liquidity/quote.rs` with tests first:

```rust
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
        let quote = validate_loop_out_quote_request(&asset, 100, 10, 10, None, 1_000, 2_000).unwrap();

        assert_eq!(quote.provider_fee, 2);
        assert_eq!(quote.routing_fee_limit, 10);
        assert_eq!(quote.expires_at, 2_000);
    }

    #[test]
    fn rejects_disabled_or_out_of_range_assets() {
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
    fn rejects_capacity_shortfall_and_udt_type_mismatch() {
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
    fn rejects_ckb_request_with_udt_type_script() {
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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb validates_loop_out_quote_for_enabled_asset -- --nocapture`

Expected: FAIL because `validate_loop_out_quote_request` does not exist.

- [ ] **Step 3: Implement quote math**

Add implementation above tests:

```rust
use crate::liquidity::types::LiquidityLoopOutError;
use fiber_types::{LiquidityAsset, LiquidityAssetKind};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ValidatedLoopOutQuote {
    pub provider_fee: u128,
    pub routing_fee_limit: u128,
    pub expires_at: u64,
}

pub fn compute_provider_fee(asset: &LiquidityAsset, amount: u128) -> Result<u128, LiquidityLoopOutError> {
    let proportional = amount
        .checked_mul(u128::from(asset.proportional_fee_ppm))
        .and_then(|v| v.checked_div(1_000_000))
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)?;
    asset
        .base_fee
        .checked_add(proportional)
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}

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
    crate::liquidity::types::loop_out_gross_payment_amount(amount, provider_fee, max_routing_fee)?;
    Ok(ValidatedLoopOutQuote {
        provider_fee,
        routing_fee_limit: max_routing_fee,
        expires_at,
    })
}
```

Update `mod.rs`:

```rust
pub mod quote;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p fnn --features rocksdb loop_out_quote -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/mod.rs crates/fiber-lib/src/liquidity/quote.rs
git commit -m "feat: validate loop out quotes"
```

---

### Task 3: Add LiquidityActor Skeleton And State Transition Commands

**Files:**
- Create: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/mod.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing actor state tests**

Create `actor.rs` with a unit test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fiber_types::LiquiditySwapState;

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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb client_payment_requires_payout_locked_state -- --nocapture`

Expected: FAIL because helpers do not exist.

- [ ] **Step 3: Add actor messages and pure state guards**

Add:

```rust
use fiber_types::{Hash256, LiquiditySwapState};

use crate::liquidity::types::{LiquidityLoopOutError, LoopOutQuoteTerms};

#[derive(Debug)]
pub enum LiquidityActorMessage {
    QuoteLoopOut(QuoteLoopOutCommand),
    AcceptLoopOut(AcceptLoopOutCommand),
    ResumeLoopOut(Hash256),
}

#[derive(Debug, Clone)]
pub struct QuoteLoopOutCommand {
    pub asset_id: String,
    pub amount: u128,
    pub max_provider_fee: u128,
    pub max_routing_fee: u128,
    pub expires_after_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct AcceptLoopOutCommand {
    pub quote: LoopOutQuoteTerms,
}

pub fn client_can_start_payment(state: LiquiditySwapState) -> bool {
    state == LiquiditySwapState::PayoutLocked
}

pub fn client_can_claim(state: LiquiditySwapState) -> bool {
    state == LiquiditySwapState::PaymentSettled
}

pub fn ensure_client_can_start_payment(state: LiquiditySwapState) -> Result<(), LiquidityLoopOutError> {
    if client_can_start_payment(state) {
        Ok(())
    } else {
        Err(LiquidityLoopOutError::InvalidStateTransition {
            from: state,
            to: LiquiditySwapState::PaymentInFlight,
        })
    }
}
```

Update `mod.rs`:

```rust
pub mod actor;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p fnn --features rocksdb client_can_ -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/mod.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: add liquidity actor boundary"
```

---

### Task 4: Add Provider Quote And Accept Workflow Behind Actor Boundary

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/types.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing provider workflow test**

Add a test proving the production provider accept method persists before chain calls by using deterministic test adapters:

```rust
#[test]
fn provider_accept_persists_payout_pending_before_chain_broadcast() {
    let mut store = TestLiquidityStore::default();
    let mut chain = TestLiquidityChain::default();
    let now_ms = 1_000;
    let quote = test_loop_out_quote(now_ms + 60_000);

    accept_provider_loop_out(&mut store, &mut chain, quote, now_ms).unwrap();

    assert_eq!(
        &store.events[..3],
        ["insert_swap", "transition_quoted", "transition_payout_pending"]
    );
    assert_eq!(chain.events, vec!["broadcast_payout"]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb provider_accept_persists_payout_pending_before_chain_broadcast -- --nocapture`

Expected: FAIL because `TestLiquidityStore`, `TestLiquidityChain`, `test_loop_out_quote`, and `accept_provider_loop_out` do not exist.

- [ ] **Step 3: Implement provider accept orchestration and test adapters**

Add production adapter traits and accept orchestration to `actor.rs`:

```rust
use crate::liquidity::store::{
    LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapKind,
    LiquiditySwapRecord, LiquiditySwapRole,
};

pub trait LoopOutChainAdapter {
    type Error;

    fn broadcast_payout_lock(
        &mut self,
        quote: &LoopOutQuoteTerms,
    ) -> Result<ckb_types::packed::OutPoint, Self::Error>;
}

pub fn accept_provider_loop_out<S, C>(
    store: &S,
    chain: &mut C,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
) -> Result<Hash256, LiquidityLoopOutError>
where
    S: LiquidityStore,
    C: LoopOutChainAdapter,
    C::Error: std::fmt::Display,
{
    if quote.expires_at <= now_ms {
        return Err(LiquidityLoopOutError::QuoteExpired);
    }

    let swap_id = quote.quote_id;
    store
        .insert_liquidity_swap(LiquiditySwapRecord {
            swap_id,
            quote_id: quote.quote_id,
            role: LiquiditySwapRole::Provider,
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: quote.asset.asset_id.clone(),
            state: LiquiditySwapState::Created,
            payment_hash: quote.payment_hash,
            payment_preimage: None,
            amount: quote.amount,
            onchain_outpoint: None,
            payout_deadline: Some(quote.payout_deadline),
            refund_after_lock_time: quote.refund_after_lock_time,
            expires_at: quote.expires_at,
            failure_reason: None,
            created_at: now_ms,
            updated_at: now_ms,
        })
        .map_err(map_store_error)?;
    store
        .update_liquidity_swap_state(
            &swap_id,
            LiquidityStateTransition {
                state: LiquiditySwapState::Quoted,
                updated_at: now_ms,
                reason: None,
            },
        )
        .map_err(map_store_error)?;
    store
        .update_liquidity_swap_state(
            &swap_id,
            LiquidityStateTransition {
                state: LiquiditySwapState::PayoutPending,
                updated_at: now_ms,
                reason: None,
            },
        )
        .map_err(map_store_error)?;

    let outpoint = chain
        .broadcast_payout_lock(&quote)
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
    store
        .update_liquidity_swap(
            &swap_id,
            crate::liquidity::store::LiquiditySwapUpdate {
                onchain_outpoint: Some(outpoint),
                updated_at: now_ms,
                ..Default::default()
            },
        )
        .map_err(map_store_error)?;

    Ok(swap_id)
}

fn map_store_error(error: LiquidityStoreError) -> LiquidityLoopOutError {
    LiquidityLoopOutError::Store(error.to_string())
}
```

Then add the command expiry helper:

```rust
impl QuoteLoopOutCommand {
    pub fn requested_expiry_ms(&self, now_ms: u64) -> Option<u64> {
        now_ms.checked_add(self.expires_after_seconds.checked_mul(1000)?)
    }
}
```

Add `#[cfg(test)]` deterministic adapters in `actor.rs` implementing `LiquidityStore` and `LoopOutChainAdapter`. The store adapter must maintain a `HashMap<Hash256, LiquiditySwapRecord>` and push event names in each trait method used above. The chain adapter must return a fixed `OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 0)` and push `"broadcast_payout"`. If `update_liquidity_swap` records `"persist_outpoint"`, append it after `"broadcast_payout"`; the test above intentionally checks only that the required persistence prefix occurs before chain broadcast.

- [ ] **Step 4: Run tests**

Run: `cargo test -p fnn --features rocksdb provider_accept_persists_payout_pending_before_chain_broadcast -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/types.rs
git commit -m "feat: define loop out provider workflow"
```

---

### Task 5: Add Payment Adapter Boundary And Client Payment Guards

**Files:**
- Create: `crates/fiber-lib/src/liquidity/payment.rs`
- Modify: `crates/fiber-lib/src/liquidity/mod.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/payment.rs`

- [ ] **Step 1: Write failing payment adapter tests**

Create `payment.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb payment_request_uses_gross_amount_and_fee_cap -- --nocapture`

Expected: FAIL because `LoopOutPaymentRequest` does not exist.

- [ ] **Step 3: Implement payment request model**

Add:

```rust
use fiber_types::Hash256;

use crate::liquidity::types::{loop_out_gross_payment_amount, LiquidityLoopOutError};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutPaymentRequest {
    pub payment_hash: Hash256,
    pub amount: u128,
    pub max_fee_amount: u128,
}

impl LoopOutPaymentRequest {
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
```

Update `mod.rs`:

```rust
pub mod payment;
```

- [ ] **Step 4: Run test**

Run: `cargo test -p fnn --features rocksdb payment_request_uses_gross_amount_and_fee_cap -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/mod.rs crates/fiber-lib/src/liquidity/payment.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: add loop out payment adapter"
```

---

### Task 6: Add Chain Adapter Boundary For Payout, Claim, And Refund

**Files:**
- Create: `crates/fiber-lib/src/liquidity/chain.rs`
- Modify: `crates/fiber-lib/src/liquidity/mod.rs`
- Test: `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Write failing payout output test**

Create `chain.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ckb_types::{bytes::Bytes, packed, prelude::*};

    fn script(args: &'static str) -> packed::Script {
        packed::Script::new_builder().args(Bytes::from(args).pack()).build()
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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb payout_builder_reuses_liquidity_lock_output_builder -- --nocapture`

Expected: FAIL because chain module and request do not exist.

- [ ] **Step 3: Implement payout output adapter**

Add:

```rust
use ckb_types::packed;

use crate::liquidity::tx::{
    build_liquidity_lock_output, LiquidityLockBuildError, LiquidityLockOutputParams,
    LiquidityLockScriptArtifact,
};

#[derive(Debug, Clone)]
pub struct LoopOutPayoutRequest {
    pub payment_hash: [u8; 32],
    pub claimant_lock: packed::Script,
    pub refund_lock: packed::Script,
    pub refund_after_lock_time: u64,
    pub amount: u128,
    pub asset_type_script: Option<packed::Script>,
    pub capacity: u64,
}

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
```

Update `mod.rs`:

```rust
pub mod chain;
```

- [ ] **Step 4: Run test**

Run: `cargo test -p fnn --features rocksdb payout_builder_reuses_liquidity_lock_output_builder -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/mod.rs crates/fiber-lib/src/liquidity/chain.rs
git commit -m "feat: add loop out chain adapter"
```

---

### Task 7: Add Liquidity RPC DTOs And RPC Module

**Files:**
- Modify: `crates/fiber-json-types/src/liquidity.rs`
- Create: `crates/fiber-lib/src/rpc/liquidity.rs`
- Modify: `crates/fiber-lib/src/rpc/mod.rs`
- Test: `crates/fiber-json-types/src/liquidity.rs`

- [ ] **Step 1: Write failing JSON DTO tests**

Add tests to `crates/fiber-json-types/src/liquidity.rs`:

```rust
#[test]
fn provider_accept_loop_out_params_serialize_quote_id() {
    let params = ProviderAcceptLoopOutParams {
        quote_id: Hash256([1u8; 32]),
        claimant_lock: "0x0102".to_string(),
        refund_lock: "0x0304".to_string(),
    };

    let value = serde_json::to_value(params).expect("json");

    assert_eq!(value["quote_id"].as_str().unwrap().len(), 66);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fiber-json-types provider_accept_loop_out_params_serialize_quote_id -- --nocapture`

Expected: FAIL because `ProviderAcceptLoopOutParams` does not exist.

- [ ] **Step 3: Add DTOs and RPC trait wired to actor calls**

Add DTOs:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderQuoteLoopOutParams {
    pub asset_id: String,
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    pub receiver: String,
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_provider_fee: u128,
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_routing_fee: u128,
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expires_after_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAcceptLoopOutParams {
    pub quote_id: Hash256,
    pub claimant_lock: String,
    pub refund_lock: String,
}
```

Create `crates/fiber-lib/src/rpc/liquidity.rs` with a jsonrpsee trait containing `quote_loop_out`, `loop_out`, `get_swap`, `list_swaps`, `provider_quote_loop_out`, and `provider_accept_loop_out`. Implement `get_swap` and `list_swaps` by calling `LiquidityStore::get_liquidity_swap` and `LiquidityStore::list_liquidity_swaps`; implement quote/execute/provider methods by sending the corresponding `LiquidityActorMessage` through a required `ActorRef<LiquidityActorMessage>` field and converting actor errors into `ErrorObjectOwned`.

> Task 7 correction: the existing `LiquidityActorMessage` variants do not yet carry typed reply ports or enough response data for RPC methods to delegate without inventing success responses. Task 7 creates the DTOs and RPC trait/module shape, implements store-backed `get_swap`/`list_swaps`, and returns internal errors from actor-backed methods. Task 8/9 must extend the actor request/response boundary before wiring these methods to the server.

The RPC implementation constructor must be:

```rust
pub struct LiquidityRpcServerImpl<S> {
    store: S,
    liquidity_actor: ractor::ActorRef<crate::liquidity::actor::LiquidityActorMessage>,
}

impl<S> LiquidityRpcServerImpl<S> {
    pub fn new(
        store: S,
        liquidity_actor: ractor::ActorRef<crate::liquidity::actor::LiquidityActorMessage>,
    ) -> Self {
        Self { store, liquidity_actor }
    }
}
```

Update `rpc/mod.rs`:

```rust
pub mod liquidity;
```

- [ ] **Step 4: Run DTO test and compile RPC module**

Run:

```bash
cargo test -p fiber-json-types provider_accept_loop_out_params_serialize_quote_id -- --nocapture
cargo check -p fnn --features rocksdb
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-json-types/src/liquidity.rs crates/fiber-lib/src/rpc/liquidity.rs crates/fiber-lib/src/rpc/mod.rs
git commit -m "feat: add loop out RPC surface"
```

---

### Task 8: Wire LiquidityActor Into RPC Server

**Files:**
- Modify: `crates/fiber-lib/src/rpc/mod.rs`
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/rpc/liquidity.rs`

- [ ] **Step 1: Write failing RPC thin-adapter test**

Add a unit test in `rpc/liquidity.rs` verifying that all M3 RPC method names are registered:

```rust
#[test]
fn liquidity_rpc_methods_are_registered_by_name() {
    let methods = liquidity_rpc_method_names();

    assert!(methods.contains(&"quote_loop_out"));
    assert!(methods.contains(&"loop_out"));
    assert!(methods.contains(&"get_swap"));
    assert!(methods.contains(&"list_swaps"));
    assert!(methods.contains(&"provider_quote_loop_out"));
    assert!(methods.contains(&"provider_accept_loop_out"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb liquidity_rpc_methods_are_registered_by_name -- --nocapture`

Expected: FAIL because helper does not exist.

- [ ] **Step 3: Spawn actor and register RPC methods**

Add helper and register module in `rpc/liquidity.rs`:

```rust
pub fn liquidity_rpc_method_names() -> Vec<&'static str> {
    vec![
        "quote_loop_out",
        "loop_out",
        "get_swap",
        "list_swaps",
        "provider_quote_loop_out",
        "provider_accept_loop_out",
    ]
}
```

Update `rpc/mod.rs` server method aggregation to spawn `LiquidityActor` before module registration and merge `LiquidityRpcServerImpl::new(store.clone(), liquidity_actor).into_rpc()` into `Methods`, matching the pattern used for payment/channel modules. Add `LiquidityStore` to `RpcServerStore` bounds. The actor arguments must include `store.clone()`, `network_actor.clone()`, and CKB adapter configuration so RPC handlers do not mutate liquidity state directly.

- [ ] **Step 4: Run tests/check**

Run:

```bash
cargo test -p fnn --features rocksdb liquidity_rpc_methods_are_registered_by_name -- --nocapture
cargo check -p fnn --features rocksdb
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/rpc/mod.rs crates/fiber-lib/src/rpc/liquidity.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: wire liquidity RPC module"
```

---

### Task 9: Implement Actor Quote/Accept Client And Provider Happy Path With Test Adapters

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/types.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing actor happy-path test**

Add a test using in-memory fake store/payment/chain adapters:

```rust
#[test]
fn loop_out_happy_path_orders_side_effects_after_persistence() {
    let mut harness = LoopOutActorTestHarness::new_with_real_orchestrator();

    harness.run_happy_path();

    assert_eq!(
        harness.events.borrow().as_slice(),
        vec![
            "client_insert_created",
            "provider_insert_created",
            "provider_transition_quoted",
            "provider_transition_payout_pending",
            "chain_broadcast_payout",
            "client_transition_payout_locked",
            "client_transition_payment_in_flight",
            "payment_send",
            "client_persist_preimage",
            "client_transition_claim_pending",
            "chain_broadcast_claim",
            "client_transition_success",
        ]
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb loop_out_happy_path_orders_side_effects_after_persistence -- --nocapture`

Expected: FAIL because harness does not exist.

- [ ] **Step 3: Implement test harness and actor orchestration methods**

Add a `#[cfg(test)]` harness in `actor.rs` that exercises production orchestration methods, not a separate event-order helper. The harness must use `TestLiquidityStore`, `TestLiquidityChain`, and a new `TestLoopOutPayment` adapter sharing one event log so cross-adapter ordering is preserved:

```rust
pub trait LoopOutPaymentAdapter {
    type Error;

    fn send_loop_out_payment(
        &mut self,
        request: crate::liquidity::payment::LoopOutPaymentRequest,
    ) -> Result<Hash256, Self::Error>;
}

#[cfg(test)]
struct LoopOutActorTestHarness {
    events: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
    store: TestLiquidityStore,
    chain: TestLiquidityChain,
    payment: TestLoopOutPayment,
}

#[cfg(test)]
impl LoopOutActorTestHarness {
    fn new_with_real_orchestrator() -> Self {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        Self {
            events: events.clone(),
            store: TestLiquidityStore::new(events.clone()),
            chain: TestLiquidityChain::new(events.clone()),
            payment: TestLoopOutPayment::new(events),
        }
    }

    fn run_happy_path(&mut self) {
        let now_ms = 1_000;
        let quote = test_loop_out_quote(now_ms + 60_000);
        create_client_loop_out(&self.store, quote.clone(), now_ms).unwrap();
        accept_provider_loop_out(&self.store, &mut self.chain, quote.clone(), now_ms).unwrap();
        mark_client_payout_locked(&self.store, quote.quote_id, now_ms + 1).unwrap();
        send_client_loop_out_payment(&self.store, &mut self.payment, quote.clone(), now_ms + 2).unwrap();
        claim_client_loop_out(&self.store, &mut self.chain, quote.quote_id, now_ms + 3).unwrap();
    }
}
```

Implement the production functions named in the harness in `actor.rs`. Each function must call M2 `LiquidityStore` APIs before external adapter methods and must reject invalid state transitions through the store. The deterministic adapters must push their events into the shared `Rc<RefCell<Vec<&'static str>>>` at the exact moment each store, payment, or chain method is called.

- [ ] **Step 4: Run test**

Run: `cargo test -p fnn --features rocksdb loop_out_happy_path_orders_side_effects_after_persistence -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/types.rs
git commit -m "feat: orchestrate loop out happy path"
```

---

### Task 10: Implement Restart Recovery Command Planning

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing recovery action tests**

Add tests:

```rust
#[test]
fn recovery_maps_non_terminal_loop_out_states_to_actions() {
    use fiber_types::LiquiditySwapState::*;

    assert_eq!(recovery_action_for_loop_out_state(PayoutPending), Some(RecoveryAction::WatchPayout));
    assert_eq!(recovery_action_for_loop_out_state(PayoutLocked), Some(RecoveryAction::ResumePayment));
    assert_eq!(recovery_action_for_loop_out_state(PaymentInFlight), Some(RecoveryAction::ReloadPayment));
    assert_eq!(recovery_action_for_loop_out_state(PaymentSettled), Some(RecoveryAction::BroadcastClaim));
    assert_eq!(recovery_action_for_loop_out_state(ClaimPending), Some(RecoveryAction::WatchClaim));
    assert_eq!(recovery_action_for_loop_out_state(Success), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb recovery_maps_non_terminal_loop_out_states_to_actions -- --nocapture`

Expected: FAIL because `RecoveryAction` and function do not exist.

- [ ] **Step 3: Implement recovery action mapping**

Add:

```rust
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RecoveryAction {
    WatchPayout,
    ResumePayment,
    ReloadPayment,
    BroadcastClaim,
    WatchClaim,
    RefundProviderPayout,
}

pub fn recovery_action_for_loop_out_state(state: LiquiditySwapState) -> Option<RecoveryAction> {
    use LiquiditySwapState::*;
    match state {
        PayoutPending => Some(RecoveryAction::WatchPayout),
        PayoutLocked => Some(RecoveryAction::ResumePayment),
        PaymentInFlight => Some(RecoveryAction::ReloadPayment),
        PaymentSettled => Some(RecoveryAction::BroadcastClaim),
        ClaimPending => Some(RecoveryAction::WatchClaim),
        RefundPending => Some(RecoveryAction::RefundProviderPayout),
        Created | Quoted | OnchainLockPending | OnchainLocked | Success | Failed | Refunded => None,
    }
}
```

- [ ] **Step 4: Run test**

Run: `cargo test -p fnn --features rocksdb recovery_maps_non_terminal_loop_out_states_to_actions -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: plan loop out recovery actions"
```

---

### Task 11: Add End-To-End Loop Out Actor Test With Deterministic Adapters

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing end-to-end test**

Add:

```rust
#[test]
fn loop_out_end_to_end_uses_real_actor_boundary_and_store() {
    let result = run_loop_out_end_to_end_test();

    assert_eq!(result.client_final_state, fiber_types::LiquiditySwapState::Success);
    assert_eq!(result.provider_final_state, fiber_types::LiquiditySwapState::Success);
    assert!(result.payment_preimage_persisted);
    assert!(result.claim_broadcast);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb loop_out_end_to_end_uses_real_actor_boundary_and_store -- --nocapture`

Expected: FAIL because test helper does not exist.

- [ ] **Step 3: Implement deterministic actor-boundary test harness**

Add a test-only result and helper that uses the same production orchestration methods as RPC-backed actor messages:

```rust
#[cfg(test)]
struct LoopOutEndToEndResult {
    client_final_state: LiquiditySwapState,
    provider_final_state: LiquiditySwapState,
    payment_preimage_persisted: bool,
    claim_broadcast: bool,
}

#[cfg(test)]
fn run_loop_out_end_to_end_test() -> LoopOutEndToEndResult {
    let mut harness = LoopOutActorTestHarness::new_with_real_orchestrator();
    harness.run_happy_path();

    let client = harness.store.get_client_swap().expect("client swap");
    let provider = harness.store.get_provider_swap().expect("provider swap");

    LoopOutEndToEndResult {
        client_final_state: client.state,
        provider_final_state: provider.state,
        payment_preimage_persisted: client.payment_preimage.is_some(),
        claim_broadcast: harness.events.borrow().contains(&"chain_broadcast_claim"),
    }
}
```

The helper must fail if the production methods skip store persistence, skip preimage persistence, or mark success before the claim adapter records `"broadcast_claim"`.

- [ ] **Step 4: Run test**

Run: `cargo test -p fnn --features rocksdb loop_out_end_to_end_uses_real_actor_boundary_and_store -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs
git commit -m "test: cover loop out end to end path"
```

---

### Task 12: Final Verification And Review

**Files:**
- Review all touched files.

- [ ] **Step 1: Run required checks**

Run:

```bash
cargo fmt --all -- --check
cargo test -p fiber-json-types liquidity -- --nocapture
cargo test -p fnn --features rocksdb loop_out -- --nocapture
cargo test -p fnn --features rocksdb liquidity -- --nocapture
cargo check -p fnn --features rocksdb
```

Expected: all PASS.

- [ ] **Step 2: Confirm no fake production paths remain**

Run: `rg "liquidity actor is not wired|loop_out_happy_path_event_order|return Ok\(|unimplemented!\(|todo!\(" crates/fiber-lib/src crates/fiber-json-types/src`

Expected: no fake production paths remain. Test-only helpers are allowed only if they call the same production orchestration methods that RPC actor messages call.

- [ ] **Step 3: Request final code review**

Use the `requesting-code-review` skill. Ask reviewer to focus on:

- real two-node RPC coordination;
- no Fiber p2p changes;
- payout lock confirmation before payment;
- payment preimage persisted before claim;
- provider refund path for unpaid payout locks;
- restart recovery idempotency;
- CKB/UDT asset correctness;
- no RPC-handler state mutation outside `LiquidityActor`.

- [ ] **Step 4: Fix review findings one at a time**

For each important finding:

1. Add or update a failing test.
2. Run it and verify failure.
3. Implement the minimal fix.
4. Rerun focused and required checks.
5. Commit with a focused message.

- [ ] **Step 5: Final status**

Run: `git status --short`

Expected: clean worktree.
