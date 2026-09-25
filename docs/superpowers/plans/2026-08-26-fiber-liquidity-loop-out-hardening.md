# CKB Loop Out Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent payment against an invalid provider payout and align CKB Loop Out fee, cap, invoice-idempotence, and watcher-registration behavior with the approved protocol.

**Architecture:** The client validates the confirmed payout live cell before payment eligibility. Payment principal and routing budget are separated, execution caps are rechecked before side effects, existing invoices are compared by observable terms, and generic outpoint tracer registration receives an explicit acknowledgement.

**Tech Stack:** Rust, Ractor, CKB JSON-RPC, CKB invoice types, RocksDB, ckb-testtool, cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-08-26-fiber-liquidity-loop-out-hardening-design.md`

---

## File Structure

- Modify `crates/fiber-lib/src/liquidity/chain.rs`: shared live-cell validation and client payout authorization.
- Modify `crates/fiber-lib/src/liquidity/actor.rs`: validation ordering, fee caps, principal calculation, and watcher error persistence.
- Modify `crates/fiber-lib/src/liquidity/payment.rs`: principal semantics and existing-invoice equivalence.
- Modify `crates/fiber-lib/src/liquidity/types.rs`: checked provider-principal helper while retaining total-budget validation.
- Modify `crates/fiber-lib/src/ckb/outpoint_tracing_actor.rs`: registration acknowledgement.
- Modify `crates/fiber-lib/src/ckb/tests/test_utils.rs`: mock acknowledgement semantics.
- Modify `crates/fiber-lib/src/liquidity/tx.rs` only if shared validation helpers require no duplicate parsing logic.
- Modify `crates/fiber-lib/src/tests/liquidity_integration.rs`: corrected balance delta and malicious-payout regression.
- Modify `crates/fiber-lib/src/tests/liquidity_test_utils.rs` only for bounded public-RPC test helpers.

---

### Task 1: Validate Confirmed Payout Cell Before Payment

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: existing test modules in both files

- [ ] **Step 1: Write RED direction-aware validation tests**

Add pure/live-cell tests proving:

```rust
#[test]
fn loop_out_payout_args_require_quote_amount_not_loop_in_gross_amount() {
    let quote = test_loop_out_quote_terms();
    let args = liquidity_lock_args_for_amount(&quote, quote.amount);
    assert!(validate_liquidity_lock_args(
        &args,
        &quote,
        quote.amount,
        &code_hash,
        hash_type,
        &code_hash,
        hash_type,
    ).is_ok());
}
```

Add actor/chain tests for committed payout cells with missing output, wrong
contract, payment hash, claimant/refund hash, refund time, amount, capacity, and
unexpected type script. Each must leave the client in `PayoutPending` and record
no `send_payment` event.

- [ ] **Step 2: Run RED**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(/loop_out_.*payout.*valid/)' --no-fail-fast
```

Expected: current client path sends payment after transaction confirmation or
the validation helper expects `amount + provider_fee` for Loop Out.

- [ ] **Step 3: Refactor shared live-cell validation**

Change `validate_liquidity_lock_args` to accept an explicit
`expected_onchain_amount: u128` instead of calling
`loop_in_gross_onchain_amount` internally.

Extract a shared helper:

```rust
fn validate_observed_liquidity_lock_cell(
    cell: &LiveCell,
    quote: &LoopOutQuoteTerms,
    artifact: &LiquidityLockArtifact,
    expected_onchain_amount: u128,
    context: &str,
) -> Result<(), LiquidityLoopOutError>
```

It performs lock args, type script, UDT data, amount, and capacity checks. Existing
Loop In validation calls it with `loop_in_gross_onchain_amount(quote)?`. Loop Out
calls it with `quote.amount`.

- [ ] **Step 4: Add chain adapter payout validation**

Extend `LoopOutChainAdapter`:

```rust
async fn validate_observed_loop_out_payout(
    &mut self,
    quote: &LoopOutQuoteTerms,
    outpoint: &packed::OutPoint,
) -> Result<(), Self::Error>;
```

`CkbLiquidityChainWatcher` fetches the exact live cell with `GetLiveCell`, rejects
missing/spent output, and calls the shared helper with `quote.amount`. Update all
test adapters with an explicit event/result implementation.

- [ ] **Step 5: Enforce ordering in `handle_payout_confirmed`**

Keep the persisted payout chain transaction status `Confirmed` because the
transaction committed. For client Loop Out, before `mark_client_payout_locked`
or payment spawning:

```rust
let quote = self.quote_terms(&swap.quote_id)?;
let outpoint = swap.onchain_outpoint.as_ref().ok_or_else(...)?;
self.chain
    .validate_observed_loop_out_payout(&quote, outpoint)
    .await
    .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
```

On validation failure, persist the descriptive failure context and return without
transitioning to `PayoutLocked` or sending payment. Do not set chain tx status to
`Rejected`.

- [ ] **Step 6: Run GREEN and commit**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(/loop_out_.*payout.*valid/)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::chain)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::actor)' --no-fail-fast
cargo fmt --all -- --check
git diff --check
git add crates/fiber-lib/src/liquidity/chain.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "fix: validate Loop Out payout before payment"
```

---

### Task 2: Correct Payment Principal and Execution Fee Caps

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/types.rs`
- Modify: `crates/fiber-lib/src/liquidity/payment.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: existing test modules

- [ ] **Step 1: Write RED principal tests**

```rust
#[test]
fn loop_out_payment_principal_excludes_routing_budget() {
    let request = LoopOutPaymentRequest::new(hash, provider, 1000, 1, 100).unwrap();
    assert_eq!(request.amount, 1001);
    assert_eq!(request.max_fee_amount, 100);
}
```

Add provider invoice registration test asserting invoice amount `1001`, not
`1101`.

- [ ] **Step 2: Write RED execution-cap tests**

Add actor tests where imported quote fee exceeds `LoopOutParams.max_provider_fee`
or routing limit exceeds `max_routing_fee`. Assert the structured error and zero
swap writes, outpoint writes, chain records, watcher registrations, invoice
registrations, and payment events.

- [ ] **Step 3: Implement checked principal helper**

In `liquidity/types.rs`:

```rust
pub fn loop_out_payment_principal(
    amount: u128,
    provider_fee: u128,
) -> Result<u128, LiquidityLoopOutError> {
    amount
        .checked_add(provider_fee)
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}
```

Keep `loop_out_gross_payment_amount(amount, provider_fee, routing_fee_limit)` as
the total budget overflow check.

- [ ] **Step 4: Use principal for payment and invoice**

`LoopOutPaymentRequest::new` sets `amount` from
`loop_out_payment_principal(amount, provider_fee)` and `max_fee_amount` from the
routing limit. Provider invoice registration uses the same principal helper.

- [ ] **Step 5: Validate execution caps first**

At the beginning of `handle_loop_out`, after loading/validating the quote but
before `create_client_loop_out`:

```rust
if quote.provider_fee > params.max_provider_fee {
    return Err(LiquidityLoopOutError::ProviderFeeTooHigh);
}
if quote.routing_fee_limit > params.max_routing_fee {
    return Err(LiquidityLoopOutError::RoutingFeeTooHigh);
}
```

- [ ] **Step 6: Run GREEN and commit**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(/loop_out_.*(principal|fee_cap)/)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::payment)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::actor)' --no-fail-fast
cargo fmt --all -- --check
git add crates/fiber-lib/src/liquidity/types.rs crates/fiber-lib/src/liquidity/payment.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "fix: separate Loop Out principal from routing budget"
```

---

### Task 3: Validate Existing Provider Invoice Equivalence

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/payment.rs`
- Test: same file

- [ ] **Step 1: Write RED equivalence matrix**

Using the payment adapter's mock network actor, preinsert invoices with the same
payment hash but vary one field per test:

- identical Open, Received, and Paid: success;
- different amount, currency, UDT script, or hash algorithm: failure;
- Cancelled or Expired: failure;
- missing `GetInvoice` result after `InvoiceAlreadyExists`: failure.

- [ ] **Step 2: Run RED**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(provider_invoice_.*equivalence)' --no-fail-fast
```

Expected: all `InvoiceAlreadyExists` cases currently return success.

- [ ] **Step 3: Add observable equivalence helper**

```rust
fn ensure_provider_invoice_equivalent(
    existing: &CkbInvoice,
    status: CkbInvoiceStatus,
    expected_hash: Hash256,
    expected_amount: u128,
    expected_currency: Currency,
    expected_udt: Option<&packed::Script>,
) -> Result<(), LiquidityLoopOutError>
```

Compare payment hash, amount, currency, `udt_type_script`, and hash algorithm
with default `CkbHash`. Allow Open/Received/Paid; reject Cancelled/Expired.

- [ ] **Step 4: Fetch and validate on duplicate**

When `AddInvoice` returns `InvoiceAlreadyExists`, call
`NetworkActorCommand::GetInvoice(payment_hash)`, map lookup errors, and call the
helper. The requested preimage hash check remains before AddInvoice.

- [ ] **Step 5: Run GREEN and commit**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(provider_invoice)' --no-fail-fast
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
cargo fmt --all -- --check
git add crates/fiber-lib/src/liquidity/payment.rs
git commit -m "fix: validate existing provider invoice terms"
```

---

### Task 4: Acknowledge Outpoint Tracer Registration

**Files:**
- Modify: `crates/fiber-lib/src/ckb/outpoint_tracing_actor.rs`
- Modify: `crates/fiber-lib/src/ckb/tests/test_utils.rs`
- Modify: `crates/fiber-lib/src/ckb/tests/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write RED acknowledgement tests**

Add production actor and mock tests asserting:

- new registration acknowledges `Ok(())` after installation;
- identical registration acknowledges `Ok(())` and both spend callbacks remain;
- conflicting metadata acknowledges `Err` and original remains;
- historical/race registration acknowledges exactly once;
- liquidity `watch_provider_claim` returns Err on conflict and actor does not add
  the swap to `watched_claim_swaps`.

- [ ] **Step 2: Add registration port**

Extend `CkbOutPointSpendTracer`:

```rust
pub registration: RpcReplyPort<Result<(), String>>,
```

Update every constructor. The tracing actor sends acknowledgement after storing
or joining a group, and sends `Err` on metadata conflict. Spend callback remains
reserved for eventual spend discovery.

- [ ] **Step 3: Implement mock acknowledgement**

Mock history fast path, live registration, and registration-race paths send
`Ok(())` exactly once after the replier is installed or history is accepted.
Removal behavior remains unchanged.

- [ ] **Step 4: Await acknowledgement in liquidity**

`watch_provider_claim` creates a registration oneshot, sends the tracer, and
awaits the result before returning. Registration channel closure maps to a
descriptive chain error. `handle_payment_settled`/recovery insert the watched guard
only after the method returns `Ok` (existing ordering retained).

- [ ] **Step 5: Run GREEN and commit**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(outpoint_tracer)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(shared_mock_chain_outpoint_spend)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(/provider_.*claim/)' --no-fail-fast
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
cargo fmt --all -- --check
git add crates/fiber-lib/src/ckb/outpoint_tracing_actor.rs crates/fiber-lib/src/ckb/tests/test_utils.rs crates/fiber-lib/src/ckb/tests/actor.rs crates/fiber-lib/src/liquidity/chain.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "fix: acknowledge outpoint tracer registration"
```

---

### Task 5: Adversarial E2E and Full Verification

**Files:**
- Modify: `crates/fiber-lib/src/tests/liquidity_integration.rs`
- Modify: `crates/fiber-lib/src/tests/liquidity_test_utils.rs` only if a bounded helper is required

- [ ] **Step 1: Correct happy/restart balance expectations**

Both existing CKB E2Es derive expected principal as checked
`quote.amount + quote.provider_fee`; routing cap is not included. Assert exact
client decrease/provider increase and empty TLCs using bounded public-RPC polling.

- [ ] **Step 2: Add malicious committed payout regression**

Create/import a quote, commit a transaction whose supplied outpoint is missing or
has a wrong lock/amount, execute client `loop_out` with that outpoint, and wait for
the confirmed transaction observation. Assert through public RPC:

- client remains `PayoutPending` with validation failure context;
- zero offered/received/pending TLCs;
- no channel balance movement;
- no client claim transaction;
- no direct store access or synthetic actor messages.

Use `MockChainController` only to submit/commit the adversarial transaction.

- [ ] **Step 3: Run full verification**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(/liquidity_ckb_loop_out/)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(rpc::liquidity)'
cargo nextest run -p fiber-json-types -E 'test(liquidity)'
cargo check --locked -p fnn --features rocksdb
cargo check --locked -p fnn --no-default-features --target wasm32-unknown-unknown
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
cargo fmt --all -- --check
make check-migrate
make check-dirty-rpc-doc
git diff --check
```

- [ ] **Step 4: Commit**

```bash
git add crates/fiber-lib/src/tests/liquidity_integration.rs crates/fiber-lib/src/tests/liquidity_test_utils.rs
git commit -m "test: cover invalid CKB Loop Out payouts"
```

---

## Plan Self-Review

- Payout authorization is handled at the chain boundary before actor payment
  eligibility (Task 1).
- Principal, routing cap, and execution caps are independent and tested before
  side effects (Task 2).
- Existing invoice idempotence is observable-term equivalence, not hash-only
  acceptance (Task 3).
- Generic tracer registration and spend observation are separate one-shot
  channels (Task 4).
- CKB E2Es cover honest, restart, and malicious provider outpoints (Task 5).
- UDT payment propagation is intentionally absent and remains original Task 6.
