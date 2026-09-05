# Fiber Liquidity M4 Loop In Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the first manual Loop In runtime slice: quote, accept, client lock, provider payment, provider claim, client refund, and durable recovery boundaries.

**Architecture:** Reuse the M3 Loop Out architecture: `LiquidityActor` owns orchestration and state transitions, `CkbLiquidityChainWatcher` owns transaction build/send/trace, the store persists swaps and chain tx records before side effects, and RPC methods only delegate to the actor. Loop In uses the existing liquidity-lock contract with reversed roles: client funds the lock, provider claims with preimage, client refunds after timeout.

**Tech Stack:** Rust, Ractor actors, CKB SDK/types, existing `LiquidityStore`, `CkbChainActor`, `cargo test -p fnn --features rocksdb`, `cargo fmt`.

---

## File Structure

- Modify `crates/fiber-lib/src/liquidity/quote.rs`: add Loop In quote validation and gross amount calculation helpers.
- Modify `crates/fiber-lib/src/liquidity/tx.rs`: reuse witness and liquidity-lock output helpers; add tests only if builder behavior belongs at raw script-output level.
- Modify `crates/fiber-lib/src/liquidity/chain.rs`: add Loop In client lock, provider claim, and client refund transaction plans/builders plus watcher methods.
- Modify `crates/fiber-lib/src/liquidity/actor.rs`: add Loop In actor messages, state transitions, recovery actions, payment continuation handling, and tests.
- Modify `crates/fiber-lib/src/rpc/liquidity.rs`: add response mapping coverage for Loop In records if missing.
- Modify `crates/fiber-lib/src/rpc/mod.rs`: wire `quote_loop_in` and `loop_in` runtime methods to `LiquidityActor`.
- Modify `crates/fiber-lib/src/rpc/README.md`: regenerate or update RPC docs after methods are wired.
- Modify `docs/specs/liquidity-management.md` only if implementation reveals a spec ambiguity; otherwise keep canonical spec unchanged.

---

### Task 1: Loop In Quote Validation And Gross Amount

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/quote.rs`
- Test: `crates/fiber-lib/src/liquidity/quote.rs`

- [ ] **Step 1: Write failing tests for Loop In gross amount and asset validation**

Add tests in `#[cfg(test)] mod tests` in `crates/fiber-lib/src/liquidity/quote.rs`:

```rust
#[test]
fn loop_in_quote_uses_gross_onchain_amount_for_capacity_checks() {
    let mut asset = ckb_asset();
    asset.available_capacity = 1_050;

    let quote = build_loop_in_quote_terms(
        &asset,
        1_000,
        None,
        "invoice:hash".to_string(),
        60_000,
        1,
    )
    .expect("loop in quote should fit gross amount");

    assert_eq!(quote.amount, 1_000);
    assert_eq!(quote.provider_fee, asset.base_fee);
    assert_eq!(loop_in_gross_onchain_amount(&quote).unwrap(), 1_000 + asset.base_fee);
    assert_eq!(quote.payout_deadline, quote.expires_at);
}

#[test]
fn loop_in_quote_rejects_gross_amount_overflow_and_capacity_shortfall() {
    let mut asset = ckb_asset();
    asset.base_fee = 2;

    let overflow = build_loop_in_quote_terms(
        &asset,
        u128::MAX,
        None,
        "invoice:hash".to_string(),
        60_000,
        1,
    )
    .unwrap_err();
    assert!(overflow.to_string().contains("gross amount overflow"));

    asset.available_capacity = 1_001;
    let shortfall = build_loop_in_quote_terms(
        &asset,
        1_000,
        None,
        "invoice:hash".to_string(),
        60_000,
        1,
    )
    .unwrap_err();
    assert!(shortfall.to_string().contains("capacity"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb liquidity::quote -- --nocapture`

Expected: FAIL because `build_loop_in_quote_terms` and `loop_in_gross_onchain_amount` do not exist.

- [ ] **Step 3: Add minimal Loop In quote helpers**

In `crates/fiber-lib/src/liquidity/quote.rs`, add helpers next to existing Loop Out quote helpers:

```rust
pub fn loop_in_gross_onchain_amount(
    quote: &LoopOutQuoteTerms,
) -> Result<u128, LiquidityLoopOutError> {
    quote
        .amount
        .checked_add(quote.provider_fee)
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)
}

pub fn build_loop_in_quote_terms(
    asset: &LiquidityAsset,
    amount: u128,
    requested_udt_type_script: Option<&ckb_jsonrpc_types::Script>,
    client_invoice: String,
    expires_after_ms: u64,
    now_ms: u64,
) -> Result<LoopOutQuoteTerms, LiquidityLoopOutError> {
    validate_quote_asset(asset, amount, requested_udt_type_script)?;
    let provider_fee = calculate_provider_fee(asset, amount)?;
    let gross = amount
        .checked_add(provider_fee)
        .ok_or(LiquidityLoopOutError::GrossAmountOverflow)?;
    if asset.available_capacity < gross {
        return Err(LiquidityLoopOutError::InvalidParameter(
            "requested loop in gross amount exceeds available asset capacity".to_string(),
        ));
    }
    let payment_hash = parse_payment_hash_from_invoice(&client_invoice)?;
    let expires_at = now_ms.saturating_add(expires_after_ms);

    Ok(LoopOutQuoteTerms {
        quote_id: HashAlgorithm::CkbHash
            .hash(format!("loop-in:{client_invoice}:{now_ms}").as_bytes())
            .into(),
        provider: default_provider_pubkey_for_tests(),
        asset: asset.clone(),
        amount,
        provider_fee,
        routing_fee_limit: 0,
        onchain_fee_estimate_ckb: 1_000,
        capacity_requirement_ckb: gross.try_into().unwrap_or(u64::MAX),
        payment_hash,
        expires_at,
        payout_deadline: expires_at,
        refund_after_lock_time: now_ms.saturating_add(120_000),
        claimant_lock: ckb_types::packed::Script::default(),
        refund_lock: ckb_types::packed::Script::default(),
    })
}
```

If existing names differ, use existing equivalents for `validate_quote_asset` and provider fee calculation instead of adding duplicate logic.

- [ ] **Step 4: Run tests to verify green**

Run: `cargo test -p fnn --features rocksdb liquidity::quote -- --nocapture`

Expected: PASS for new Loop In quote tests and existing quote tests.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/quote.rs
git commit -m "feat: add loop in quote validation"
```

---

### Task 2: Loop In Chain Transaction Builders

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Test: `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Write failing tests for client lock, provider claim, and client refund builders**

Add tests to `crates/fiber-lib/src/liquidity/chain.rs` near existing transaction builder tests:

```rust
#[test]
fn loop_in_client_lock_output_uses_provider_claim_and_client_refund_locks() {
    let artifact = liquidity_lock_artifact();
    let quote = test_loop_in_quote_terms();

    let (output, data) = build_loop_in_client_lock_output(&artifact, &quote)
        .expect("loop in client lock output");

    assert_eq!(output.lock().code_hash(), artifact.code_hash);
    assert!(data.raw_data().is_empty());
}

#[test]
fn loop_in_provider_claim_spends_client_lock_with_preimage_witness() {
    let quote = test_loop_in_quote_terms();
    let preimage: Hash256 = [4u8; 32].into();
    let outpoint = test_outpoint(30);
    let cell_dep = packed::CellDep::new_builder()
        .out_point(test_outpoint(31))
        .dep_type(ckb_types::core::DepType::Code)
        .build();

    let tx = build_loop_in_provider_claim_transaction(
        &quote,
        &outpoint,
        preimage,
        &[cell_dep.clone()],
    )
    .expect("provider claim tx");

    assert_eq!(tx.inputs().get(0).unwrap().previous_output(), outpoint);
    assert_eq!(tx.outputs().get(0).unwrap().lock(), quote.claimant_lock);
    assert_eq!(tx.witnesses().get(0).unwrap(), build_liquidity_lock_claim_witness(preimage.into()));
    assert_eq!(tx.cell_deps().into_iter().collect::<Vec<_>>(), vec![cell_dep]);
}

#[test]
fn loop_in_client_refund_spends_client_lock_with_since_and_refund_witness() {
    let quote = test_loop_in_quote_terms();
    let outpoint = test_outpoint(32);

    let tx = build_loop_in_client_refund_transaction(
        &quote,
        &outpoint,
        quote.refund_after_lock_time,
        &[],
    )
    .expect("client refund tx");

    let input = tx.inputs().get(0).unwrap();
    assert_eq!(input.previous_output(), outpoint);
    assert_eq!(u64::from(input.since()), quote.refund_after_lock_time);
    assert_eq!(tx.outputs().get(0).unwrap().lock(), quote.refund_lock);
    assert_eq!(tx.witnesses().get(0).unwrap(), build_liquidity_lock_refund_witness());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture`

Expected: FAIL because Loop In builder functions and `test_loop_in_quote_terms` do not exist.

- [ ] **Step 3: Add builder functions by reusing M3 primitives**

Add these functions in `crates/fiber-lib/src/liquidity/chain.rs` after existing Loop Out builders:

```rust
pub fn build_loop_in_client_lock_output(
    artifact: &LiquidityLockScriptArtifact,
    quote: &LoopOutQuoteTerms,
) -> Result<(packed::CellOutput, packed::Bytes), LiquidityLockBuildError> {
    let gross_amount = quote.amount.saturating_add(quote.provider_fee);
    build_loop_out_payout_output(
        artifact,
        &LoopOutPayoutRequest {
            payment_hash: quote.payment_hash.into(),
            claimant_lock: quote.claimant_lock.clone(),
            refund_lock: quote.refund_lock.clone(),
            refund_after_lock_time: quote.refund_after_lock_time,
            amount: gross_amount,
            asset_type_script: quote.asset.udt_type_script.clone().map(Into::into),
            capacity: quote.capacity_requirement_ckb,
        },
    )
}

pub fn build_loop_in_provider_claim_transaction(
    quote: &LoopOutQuoteTerms,
    lock_outpoint: &packed::OutPoint,
    payment_preimage: Hash256,
    liquidity_lock_cell_deps: &[packed::CellDep],
) -> Result<TransactionView, LiquidityLoopOutError> {
    build_loop_out_claim_transaction(quote, lock_outpoint, payment_preimage, liquidity_lock_cell_deps)
}

pub fn build_loop_in_client_refund_transaction(
    quote: &LoopOutQuoteTerms,
    lock_outpoint: &packed::OutPoint,
    refund_after_lock_time: u64,
    liquidity_lock_cell_deps: &[packed::CellDep],
) -> Result<TransactionView, LiquidityLoopOutError> {
    build_loop_out_refund_transaction(quote, lock_outpoint, refund_after_lock_time, liquidity_lock_cell_deps)
}
```

Add a test helper:

```rust
fn test_loop_in_quote_terms() -> LoopOutQuoteTerms {
    LoopOutQuoteTerms {
        quote_id: [21u8; 32].into(),
        amount: 100,
        provider_fee: 2,
        payout_deadline: 0,
        refund_after_lock_time: 40_000,
        claimant_lock: script("provider-claim"),
        refund_lock: script("client-refund"),
        ..test_loop_out_quote_terms()
    }
}
```

- [ ] **Step 4: Run tests to verify green**

Run: `cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/chain.rs
git commit -m "feat: add loop in chain transaction builders"
```

---

### Task 3: Loop In Actor Quote And Accept Skeleton

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing tests for Loop In actor messages and persistence ordering**

Add tests near existing actor message and Loop Out runtime tests:

```rust
#[test]
fn actor_message_names_cover_loop_in_runtime() {
    let names = LiquidityActorMessage::variant_names();

    assert!(names.contains(&"quote_loop_in"));
    assert!(names.contains(&"loop_in"));
    assert!(names.contains(&"loop_in_lock_confirmed"));
}

#[tokio::test]
async fn loop_in_accept_persists_before_client_lock_broadcast() {
    let events = Shared::new(Vec::new());
    let store = TestLiquidityStore::new(events.clone(), "client");
    let quote = test_loop_in_quote(now_ms() + 60_000);
    store.insert_loop_in_quote(quote.clone(), now_ms()).unwrap();
    let actor = spawn_test_liquidity_actor(
        store.clone(),
        TestLoopOutPayment::new(events.clone()),
        TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
    )
    .await;

    let response = call_loop_in(actor, quote.quote_id).await.unwrap();

    assert_eq!(response.state, "onchain_lock_pending");
    assert_eq!(
        events.borrow().as_slice(),
        ["client_insert_swap", "broadcast_loop_in_lock"]
    );
}
```

If `insert_loop_in_quote` does not exist yet, add it as a test-store helper that delegates to existing quote insertion while preserving `swap_kind` through the quote terms.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture`

Expected: FAIL because Loop In actor messages and test helpers do not exist.

- [ ] **Step 3: Add actor messages and minimal handlers**

In `LiquidityActorMessage`, add:

```rust
QuoteLoopIn(
    QuoteLoopInParams,
    RpcReplyPort<Result<LiquidityQuoteResponse, LiquidityLoopOutError>>,
),
LoopIn(
    LoopInParams,
    RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
),
LoopInLockConfirmed(Hash256),
```

Add names in `variant_names()`:

```rust
"quote_loop_in",
"loop_in",
"loop_in_lock_confirmed",
```

Add match arms in `handle`:

```rust
LiquidityActorMessage::QuoteLoopIn(params, reply) => {
    let result = state.handle_quote_loop_in(params);
    let _ = reply.send(result);
}
LiquidityActorMessage::LoopIn(params, reply) => {
    let result = state.handle_loop_in(params, myself.clone()).await;
    let _ = reply.send(result);
}
LiquidityActorMessage::LoopInLockConfirmed(swap_id) => {
    if let Err(error) = mark_loop_in_client_lock_confirmed(&state.store, swap_id, now_ms()) {
        tracing::warn!(?swap_id, %error, "ignoring loop in lock continuation");
    }
}
```

- [ ] **Step 4: Implement minimal quote and accept helpers**

Add helpers in `actor.rs`:

```rust
pub async fn accept_client_loop_in<S, C>(
    store: &S,
    chain: &mut C,
    quote: LoopOutQuoteTerms,
    now_ms: u64,
    myself: ActorRef<LiquidityActorMessage>,
) -> Result<Hash256, LiquidityLoopOutError>
where
    S: LiquidityStore,
    C: LiquidityChainWatcher,
    C::Error: ToString,
{
    let swap = loop_in_record(&quote, LiquiditySwapRole::Client, now_ms);
    let swap_id = swap.swap_id;
    store.insert_liquidity_swap(swap).map_err(map_store_error)?;
    chain
        .broadcast_loop_in_lock(&quote, myself)
        .await
        .map_err(|error| LiquidityLoopOutError::Chain(error.to_string()))?;
    Ok(swap_id)
}
```

Add `loop_in_record` mirroring `loop_out_record` but with `swap_kind: LiquiditySwapKind::LoopIn` and initial state `OnchainLockPending`.

- [ ] **Step 5: Run tests to verify green**

Run: `cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture`

Expected: PASS for new skeleton tests and existing actor tests.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: add loop in actor skeleton"
```

---

### Task 4: Extend Chain Watcher Trait For Loop In Lock/Claim/Refund

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/chain.rs`, `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing chain watcher tests**

Add tests in `chain.rs`:

```rust
#[tokio::test]
async fn ckb_watcher_broadcast_loop_in_lock_persists_tx_identity_before_send_tx() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (ckb_actor, _handle) = ractor::Actor::spawn(None, MockCkbActor, events.clone()).await.unwrap();
    let quote = test_loop_in_quote_terms();
    let store = NoopLiquidityStore::default();
    store.insert_loop_in_quote(quote.clone(), 1).unwrap();
    let mut watcher = CkbLiquidityChainWatcher::new_with_liquidity_lock_script(
        ckb_actor,
        store.clone(),
        liquidity_lock_script_for_quote(&quote),
        vec![],
    );

    watcher
        .broadcast_loop_in_lock(&quote, spawn_mock_liquidity_actor().await.0)
        .await
        .unwrap();

    wait_for_mock_events(&events, 1).await;
    assert_eq!(*events.lock().unwrap(), vec![MockCkbEvent::SendTx]);
    let record = store
        .get_liquidity_chain_tx(&quote.quote_id, LiquidityChainTxRole::Payout)
        .unwrap()
        .expect("loop in lock tx record");
    assert_eq!(record.status, LiquidityChainTxStatus::Broadcast);
    assert!(record.outpoint.is_some());
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p fnn --features rocksdb ckb_watcher_broadcast_loop_in_lock_persists_tx_identity_before_send_tx -- --nocapture`

Expected: FAIL because `broadcast_loop_in_lock` does not exist.

- [ ] **Step 3: Add trait methods**

Extend `LiquidityChainWatcher`:

```rust
async fn broadcast_loop_in_lock(
    &mut self,
    quote: &LoopOutQuoteTerms,
    myself: ActorRef<LiquidityActorMessage>,
) -> Result<(), Self::Error>;

async fn watch_loop_in_lock(
    &mut self,
    swap_id: Hash256,
    myself: ActorRef<LiquidityActorMessage>,
) -> Result<(), Self::Error>;
```

Update test adapters in `actor.rs` and `chain.rs` with minimal implementations that push `broadcast_loop_in_lock` and `watch_loop_in_lock` events.

- [ ] **Step 4: Implement CKB watcher lock broadcast/watch**

In `CkbLiquidityChainWatcher`, implement `broadcast_loop_in_lock` by:

1. Building the liquidity-lock output using `build_loop_in_client_lock_output`.
2. Calling existing `Fund` and `Sign` flow like Loop Out payout.
3. Persisting `LiquidityChainTxRole::Payout` with `Planned` before `SendTx`.
4. Marking `Broadcast` after successful `SendTx`.
5. Creating or scheduling tracer through `watch_loop_in_lock`.

Use the existing payout implementation as the code template, but send `LiquidityActorMessage::LoopInLockConfirmed(swap_id)` from the tracer callback for Loop In lock confirmation.

- [ ] **Step 5: Run chain and actor tests**

Run:

```bash
cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture
cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/chain.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: wire loop in lock transactions"
```

---

### Task 5: Provider Payment And Claim Orchestration

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`, `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Write failing provider-side tests**

Add actor tests:

```rust
#[tokio::test]
async fn provider_loop_in_payment_waits_for_onchain_lock_confirmation() {
    let events = Shared::new(Vec::new());
    let store = TestLiquidityStore::new(events.clone(), "provider");
    let quote = test_loop_in_quote(now_ms() + 60_000);
    let swap = loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms());
    store.insert_liquidity_swap(swap.clone()).unwrap();
    let actor = spawn_test_liquidity_actor(
        store.clone(),
        TestLoopOutPayment::new(events.clone()),
        TestLiquidityChain::new(events.clone()),
    )
    .await;

    actor.send_message(LiquidityActorMessage::LoopInLockConfirmed(swap.swap_id)).unwrap();
    wait_for_event(&events, "send_payment").await;

    assert_eq!(
        store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap().state,
        LiquiditySwapState::PaymentInFlight
    );
}

#[tokio::test]
async fn provider_loop_in_claim_persists_preimage_before_chain_claim() {
    let events = Shared::new(Vec::new());
    let store = TestLiquidityStore::new(events.clone(), "provider");
    let quote = test_loop_in_quote(now_ms() + 60_000);
    let mut swap = loop_in_record(&quote, LiquiditySwapRole::Provider, now_ms());
    swap.state = LiquiditySwapState::PaymentSettled;
    swap.payment_preimage = Some([4u8; 32].into());
    store.insert_liquidity_swap(swap.clone()).unwrap();
    let actor = spawn_test_liquidity_actor(
        store.clone(),
        TestLoopOutPayment::new(events.clone()),
        TestLiquidityChain::new_with_label(events.clone(), "runtime_provider"),
    )
    .await;

    assert_eq!(call_resume_non_terminal(actor).await, 1);
    assert_eq!(event_count(&events, "broadcast_claim"), 1);
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture`

Expected: FAIL because Loop In provider continuations are not implemented.

- [ ] **Step 3: Implement provider payment after lock confirmation**

Add a helper in `actor.rs`:

```rust
pub async fn send_provider_loop_in_payment<S, P>(
    store: &S,
    payment: &mut P,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
    P: LoopOutPaymentAdapter,
    P::Error: ToString,
{
    transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)?;
    let swap = store
        .get_liquidity_swap(&swap_id)
        .map_err(map_store_error)?
        .ok_or_else(|| LiquidityLoopOutError::Store(format!("liquidity swap not found: {swap_id:?}")))?;
    payment
        .send_payment(LoopOutPaymentRequest {
            payment_hash: swap.payment_hash,
            amount: swap.amount,
            max_fee: 0,
        })
        .await
        .map_err(|error| LiquidityLoopOutError::Payment(error.to_string()))?;
    Ok(())
}
```

Use existing payment request types and field names from `payment.rs`; if names differ, adapt without changing behavior.

- [ ] **Step 4: Implement provider claim recovery**

Reuse `broadcast_claim` for Loop In provider claim because it spends the liquidity-lock with the preimage. Ensure `LoopOutClaimPlan::from_record` accepts `LiquiditySwapKind::LoopIn` provider records in `PaymentSettled` or `ClaimPending`.

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture
cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/chain.rs
git commit -m "feat: orchestrate loop in provider payment and claim"
```

---

### Task 6: Client Refund Recovery

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`, `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Write failing client refund tests**

Add actor test:

```rust
#[tokio::test]
async fn loop_in_client_refund_pending_recovery_broadcasts_refund_and_schedules_watch() {
    let events = Shared::new(Vec::new());
    let store = TestLiquidityStore::new(events.clone(), "client");
    let quote = test_loop_in_quote(now_ms() + 60_000);
    let mut swap = loop_in_record(&quote, LiquiditySwapRole::Client, now_ms());
    swap.state = LiquiditySwapState::RefundPending;
    swap.onchain_outpoint = Some(OutPoint::new(Byte32::from_slice(&[44u8; 32]).unwrap(), 0));
    store.insert_liquidity_swap(swap).unwrap();
    let actor = spawn_test_liquidity_actor(
        store,
        TestLoopOutPayment::new_with_label(events.clone(), "runtime"),
        TestLiquidityChain::new_with_label(events.clone(), "runtime_client"),
    )
    .await;

    assert_eq!(call_resume_non_terminal(actor).await, 1);
    assert_eq!(event_count(&events, "broadcast_refund"), 1);
    assert_eq!(event_count(&events, "watch_refund"), 1);
}
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test -p fnn --features rocksdb loop_in_client_refund_pending_recovery_broadcasts_refund_and_schedules_watch -- --nocapture`

Expected: FAIL because Loop In client refund recovery is not mapped.

- [ ] **Step 3: Extend refund planning to allow Loop In client refunds**

In `LoopOutRefundTxPlan::from_record`, allow either:

```rust
let is_loop_out_provider_refund =
    record.swap_kind == LiquiditySwapKind::LoopOut && record.role == LiquiditySwapRole::Provider;
let is_loop_in_client_refund =
    record.swap_kind == LiquiditySwapKind::LoopIn && record.role == LiquiditySwapRole::Client;
if !(is_loop_out_provider_refund || is_loop_in_client_refund) {
    return Err(LiquidityLoopOutError::Chain(
        "cannot build refund for this liquidity swap role".to_string(),
    ));
}
```

Keep the `RefundPending` and outpoint requirements.

- [ ] **Step 4: Extend recovery action selection**

In `resume_swap`, when `RefundPending` belongs to a Loop In client, use the same refund broadcast/watch path as M3 provider refund.

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture
cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/chain.rs
git commit -m "feat: add loop in client refund recovery"
```

---

### Task 7: RPC Wiring And Documentation

**Files:**
- Modify: `crates/fiber-lib/src/rpc/mod.rs`
- Modify: `crates/fiber-lib/src/rpc/README.md`
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs`
- Test: `crates/fiber-lib/src/rpc/liquidity.rs`, actor tests as needed

- [ ] **Step 1: Write failing RPC runtime tests**

Add or extend RPC tests to verify `quote_loop_in` and `loop_in` no longer return unavailable when liquidity actor is wired:

```rust
#[tokio::test]
async fn liquidity_rpc_quote_loop_in_delegates_to_actor_when_runtime_available() {
    let harness = manual_loop_out_runtime_harness();

    let response = harness
        .rpc
        .quote_loop_in(QuoteLoopInParams {
            provider: "local".to_string(),
            asset_id: "ckb".to_string(),
            amount: 1_000,
            client_invoice: "invoice:hash".to_string(),
            max_provider_fee: 10,
            max_routing_fee: 10,
            expires_after_seconds: 60,
        })
        .await;

    assert!(response.is_ok());
}
```

Use existing RPC harness names from `rpc/mod.rs` tests; do not create a new runtime harness if one already exists.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p fnn --features rocksdb rpc:: -- --nocapture`

Expected: FAIL because Loop In mutation RPCs still return unavailable or are not delegated.

- [ ] **Step 3: Wire RPC methods**

In `rpc/mod.rs`, update `quote_loop_in` and `loop_in` to call `ractor::call!` with:

```rust
LiquidityActorMessage::QuoteLoopIn(params, reply)
LiquidityActorMessage::LoopIn(params, reply)
```

Preserve fail-closed behavior when liquidity runtime is unavailable.

- [ ] **Step 4: Update RPC docs**

Run: `make gen-rpc-doc`

Expected: `crates/fiber-lib/src/rpc/README.md` updates for Loop In runtime methods.

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test -p fnn --features rocksdb rpc:: -- --nocapture
cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-lib/src/rpc/mod.rs crates/fiber-lib/src/rpc/liquidity.rs crates/fiber-lib/src/rpc/README.md
git commit -m "feat: wire loop in liquidity rpc"
```

---

### Task 8: Final Verification And Issue Update

**Files:**
- Modify: `docs/superpowers/plans/2026-07-27-fiber-liquidity-m4-loop-in-runtime.md` if task statuses are tracked in-file.
- No code changes expected unless verification finds failures.

- [ ] **Step 1: Run targeted tests**

Run:

```bash
cargo test -p fnn --features rocksdb liquidity::tx -- --nocapture
cargo test -p fnn --features rocksdb liquidity::quote -- --nocapture
cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture
cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture
cargo test -p fnn --features rocksdb rpc:: -- --nocapture
cargo check -p fnn --features rocksdb
cargo fmt --all -- --check
```

Expected: all commands exit 0.

- [ ] **Step 2: Request code review**

Use the requesting-code-review skill with this scope:

```text
Review M4 Loop In runtime slice: quote_loop_in/loop_in actor and RPC wiring,
client lock transaction lifecycle, provider payment+claim orchestration, client
refund recovery, tracer rejection handling, and compatibility with M3 Loop Out.
```

- [ ] **Step 3: Fix review findings**

If review returns `CHANGES_REQUIRED`, add a failing regression for each finding, implement the minimal fix, and rerun the targeted verification commands from Step 1.

- [ ] **Step 4: Commit verification fixes**

If Step 3 changed files, run:

```bash
git add crates/fiber-lib/src docs/superpowers/plans docs/superpowers/specs
git commit -m "fix: harden loop in liquidity runtime"
```

- [ ] **Step 5: Update GitHub issue**

Post a comment to `nervosnetwork/fiber#1541` that starts with:

```markdown
> *This was generated by AI during triage.*
```

Include:

- latest branch and commit hash;
- completed M4 scope;
- verification commands and results;
- next milestone or remaining gaps.

- [ ] **Step 6: Push branch**

Run:

```bash
git push origin liquidity-management-m0
```

Expected: remote branch updates successfully.

---

## Self-Review Notes

- Spec coverage: tasks cover quote/RPC, client lock, provider payment and claim,
  client refund, recovery, rejected tracer behavior, documentation, and final issue
  update.
- Deferred scope: provider discovery, p2p messages, automated planning, and broad
  wallet redesign remain excluded as required by the spec.
- Type naming risk: existing code still uses `LoopOutQuoteTerms` for shared quote
  terms. The plan intentionally reuses it in M4 to minimize churn, but a later
  cleanup may rename it to a direction-neutral type.
