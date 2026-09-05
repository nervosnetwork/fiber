# Provider Claim Observation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a reusable CKB outpoint-spend tracer and use it to move provider Loop Out swaps from `PaymentSettled` to `Success` only after a committed, validated client claim.

**Architecture:** A new linked CKB tracing actor queries indexer input transactions by exact lock script, verifies the exact watched outpoint and confirmations, and returns the committed spending transaction plus input index. The mock chain provides equivalent race-safe history and notification semantics. Liquidity validates the claim witness and resumes the watcher after restart.

**Tech Stack:** Rust, Ractor, CKB JSON-RPC/indexer, ckb-types, Tokio, ckb-testtool, RocksDB, cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-08-26-fiber-liquidity-provider-claim-observation-design.md`

---

## File Structure

- Create `crates/fiber-lib/src/ckb/outpoint_tracing_actor.rs`: reusable polling actor and public tracer/result types.
- Modify `crates/fiber-lib/src/ckb/client.rs`: exact-script input search and exact-outpoint committed-spender discovery.
- Modify `crates/fiber-lib/src/ckb/actor.rs`: spawn/link tracer actor and route registration/removal messages.
- Modify `crates/fiber-lib/src/ckb/mod.rs`: module registration and exports.
- Modify `crates/fiber-lib/src/ckb/tests/test_utils.rs`: mock committed-spender history and race-safe tracer registration.
- Modify `crates/fiber-lib/src/ckb/tests/actor.rs`: shared mock-chain outpoint tracing matrix.
- Modify `crates/fiber-lib/src/liquidity/tx.rs`: strict claim witness parser.
- Modify `crates/fiber-lib/src/liquidity/chain.rs`: `watch_provider_claim` registration and callback validation.
- Modify `crates/fiber-lib/src/liquidity/actor.rs`: start/recover/deduplicate provider claim watches and make success observation idempotent.
- Modify `crates/fiber-lib/src/tests/liquidity_integration.rs`: complete the preserved CKB Loop Out E2E.

---

### Task 1: Exact Committed Outpoint-Spender Discovery

**Files:**
- Modify: `crates/fiber-lib/src/ckb/client.rs`
- Test: `crates/fiber-lib/src/ckb/client.rs` (`mod tests`)

- [ ] **Step 1: Write RED candidate-filter tests**

Add pure tests for a helper that selects only an input-indexer entry and for a helper that locates the exact watched input in a loaded transaction:

```rust
#[test]
fn exact_watched_input_index_ignores_other_inputs() {
    let watched = packed::OutPoint::new(packed::Byte32::from_slice(&[1; 32]).unwrap(), 2);
    let other = packed::OutPoint::new(packed::Byte32::from_slice(&[2; 32]).unwrap(), 0);
    let tx = TransactionBuilder::default()
        .input(CellInput::new(other, 0))
        .input(CellInput::new(watched.clone(), 0))
        .build();

    assert_eq!(find_watched_input_index(&tx, &watched), Some(1));
}

#[test]
fn exact_watched_input_index_rejects_same_script_different_outpoint() {
    let watched = packed::OutPoint::new(packed::Byte32::from_slice(&[1; 32]).unwrap(), 2);
    let tx = TransactionBuilder::default()
        .input(CellInput::new(
            packed::OutPoint::new(packed::Byte32::from_slice(&[1; 32]).unwrap(), 3),
            0,
        ))
        .build();

    assert_eq!(find_watched_input_index(&tx, &watched), None);
}
```

- [ ] **Step 2: Run RED**

Run:

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(exact_watched_input_index)'
```

Expected: compile failure because `find_watched_input_index` does not exist.

- [ ] **Step 3: Add discovery result and helpers**

In `crates/fiber-lib/src/ckb/client.rs`, add:

```rust
#[derive(Clone, Debug)]
pub struct CommittedOutPointSpend {
    pub transaction: TransactionView,
    pub input_index: usize,
    pub block_number: u64,
}

pub(crate) fn find_watched_input_index(
    transaction: &TransactionView,
    watched_outpoint: &packed::OutPoint,
) -> Option<usize> {
    transaction
        .input_pts_iter()
        .position(|outpoint| &outpoint == watched_outpoint)
}
```

Generalize `new_shutdown_tx_search_key` to `new_exact_lock_tx_search_key` and retain the shutdown helper as a caller.

- [ ] **Step 4: Add async exact-spender discovery**

Add a native async helper used by the tracing actor:

```rust
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn find_committed_outpoint_spend(
    rpc_url: &str,
    lock_script: &packed::Script,
    watched_outpoint: &packed::OutPoint,
    confirmations: u64,
) -> Result<Option<CommittedOutPointSpend>, anyhow::Error> {
    // Paginate get_transactions with ScriptType::Lock, SearchMode::Exact.
    // Consider CellType::Input entries only.
    // Load each candidate with get_transaction.
    // Require TxStatus::Committed and tip - block + 1 >= confirmations.
    // Convert the packed transaction to TransactionView.
    // Return only when find_watched_input_index matches exactly.
}
```

Use `new_ckb_rpc_async_client(rpc_url)`, `Order::Desc`, page size `100`, and the same pagination/error mapping as `find_first_input_tx_hash_async`. Query the tip once per scan with `get_tip_block_number`.

- [ ] **Step 5: Run GREEN and existing client tests**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(ckb::client)'
cargo fmt --all -- --check
```

Expected: all selected tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/fiber-lib/src/ckb/client.rs
git commit -m "feat: find committed CKB outpoint spenders"
```

---

### Task 2: Generic Outpoint-Spend Tracing Actor

**Files:**
- Create: `crates/fiber-lib/src/ckb/outpoint_tracing_actor.rs`
- Modify: `crates/fiber-lib/src/ckb/actor.rs`
- Modify: `crates/fiber-lib/src/ckb/mod.rs`
- Test: `crates/fiber-lib/src/ckb/outpoint_tracing_actor.rs` (`mod tests`)

- [ ] **Step 1: Write RED actor tests**

Add tests with an injected discovery function/test backend proving registration is retained while no spender exists, callback fires once after discovery, and historical discovery on first poll works:

```rust
#[tokio::test]
async fn outpoint_tracer_reports_historical_committed_spender_once() {
    // Spawn CkbOutPointTracingActor with a deterministic test discovery backend.
    // Register one tracer, trigger RunTracers, receive one result.
    // Trigger RunTracers again and assert no second callback/tracer remains.
}
```

- [ ] **Step 2: Run RED**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(outpoint_tracer)'
```

Expected: compile failure because the actor/types do not exist.

- [ ] **Step 3: Define tracer API**

Create `outpoint_tracing_actor.rs` with:

```rust
#[derive(Clone, Debug)]
pub struct CkbOutPointSpendTracingResult {
    pub outpoint: packed::OutPoint,
    pub spending_transaction: TransactionView,
    pub input_index: usize,
    pub block_number: u64,
}

#[derive(Debug)]
pub struct CkbOutPointSpendTracer {
    pub outpoint: packed::OutPoint,
    pub lock_script: packed::Script,
    pub confirmations: u64,
    pub callback: RpcReplyPort<Result<CkbOutPointSpendTracingResult, String>>,
}

pub struct CkbOutPointTracingArguments {
    pub rpc_url: String,
    pub polling_interval: Duration,
}

pub enum CkbOutPointTracingMessage {
    CreateTracer(CkbOutPointSpendTracer),
    RemoveTracers(packed::OutPoint),
    RunTracers,
    ReportResult(packed::OutPoint, Result<Option<CkbOutPointSpendTracingResult>, String>),
}
```

Group registrations by exact outpoint. An identical lock script/confirmation registration joins the callback list. A conflicting lock script sends `Err` immediately through the new callback and does not replace the existing watch.

- [ ] **Step 4: Implement polling and one-shot completion**

Follow `CkbTxTracingActor`'s interval/task pattern. Each poll invokes Task 1's `find_committed_outpoint_spend`; `None` retains tracers, retryable errors log and retain them, and `Some(result)` sends every callback exactly once and removes the group.

- [ ] **Step 5: Link from `CkbChainActor`**

In `ckb/actor.rs`:

```rust
pub enum CkbChainMessage {
    // existing variants...
    CreateOutPointSpendTracer(CkbOutPointSpendTracer),
    RemoveOutPointSpendTracers(packed::OutPoint),
}
```

Add `ckb_outpoint_tracing_actor: ActorRef<CkbOutPointTracingMessage>` to `CkbChainState`, spawn it linked in `pre_start` with the configured RPC URL/polling interval, and forward both messages.

In `ckb/mod.rs`, register the module and export `CkbOutPointSpendTracer` and `CkbOutPointSpendTracingResult`.

- [ ] **Step 6: Run GREEN and commit**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(outpoint_tracer)'
cargo check --locked -p fnn --features rocksdb
cargo fmt --all -- --check
git add crates/fiber-lib/src/ckb/outpoint_tracing_actor.rs crates/fiber-lib/src/ckb/actor.rs crates/fiber-lib/src/ckb/mod.rs
git commit -m "feat: add CKB outpoint spend tracer"
```

---

### Task 3: Race-Safe Mock Outpoint Tracing

**Files:**
- Modify: `crates/fiber-lib/src/ckb/tests/test_utils.rs`
- Modify: `crates/fiber-lib/src/ckb/tests/actor.rs`

- [ ] **Step 1: Write RED shared-chain tests**

Add tests named with `shared_mock_chain_outpoint_spend`:

```rust
#[tokio::test]
async fn shared_mock_chain_outpoint_spend_notifies_only_after_commit() {
    // Register watcher for a live input.
    // Submit spender: callback remains pending and input remains live.
    // Commit: callback returns exact tx/input index and cell becomes dead.
}

#[tokio::test]
async fn shared_mock_chain_outpoint_spend_reject_does_not_notify() {
    // Reject pending spender; assert callback remains pending and input stays live.
}

#[tokio::test]
async fn shared_mock_chain_outpoint_spend_registration_after_commit_finds_history() {
    // Commit first, register later, assert immediate historical result.
}
```

- [ ] **Step 2: Run RED**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(shared_mock_chain_outpoint_spend)'
```

Expected: mock actor does not handle `CreateOutPointSpendTracer`.

- [ ] **Step 3: Add committed-spender state**

Extend `MockChainState` with:

```rust
committed_outpoint_spends: HashMap<packed::OutPoint, CkbOutPointSpendTracingResult>,
outpoint_spend_notifications: broadcast::Sender<CkbOutPointSpendTracingResult>,
```

During `MockChainController::commit`, after VM verification and while holding the backend write lock, enumerate `tx.input_pts_iter()`, record a result containing `tx.clone()` and input index, then notify after committed state is visible. Rejection records no spender.

- [ ] **Step 4: Implement race-safe registration**

Handle `CreateOutPointSpendTracer` in `MockChainActor` using subscribe-then-recheck:

1. subscribe to `outpoint_spend_notifications`;
2. read `committed_outpoint_spends`;
3. if history exists, return immediately;
4. otherwise wait for a matching exact outpoint and send once.

Use the existing tracer registration gate pattern to add a deterministic registration/commit race test. Handle removal by stopping/removing stored replier actors/tasks.

- [ ] **Step 5: Run GREEN and commit**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(shared_mock_chain_outpoint_spend)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(shared_mock_chain)' --no-fail-fast
cargo fmt --all -- --check
git diff --check
git add crates/fiber-lib/src/ckb/tests/test_utils.rs crates/fiber-lib/src/ckb/tests/actor.rs
git commit -m "test: support outpoint spend tracing in mock chain"
```

---

### Task 4: Liquidity Claim Validation and Recovery

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/tx.rs`
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: same files' existing test modules

- [ ] **Step 1: Write RED witness parser tests**

In `liquidity/tx.rs`, add:

```rust
#[test]
fn parse_claim_witness_returns_preimage() {
    let preimage = [7u8; 32];
    let witness = build_liquidity_lock_claim_witness(preimage);
    assert_eq!(parse_liquidity_lock_claim_witness(&witness).unwrap(), preimage);
}

#[test]
fn parse_claim_witness_rejects_refund_and_malformed_layout() {
    assert!(parse_liquidity_lock_claim_witness(&build_liquidity_lock_refund_witness()).is_err());
    assert!(parse_liquidity_lock_claim_witness(&packed::Bytes::default()).is_err());
}
```

- [ ] **Step 2: Run RED and implement parser**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(parse_claim_witness)'
```

Implement a strict parser using `packed::WitnessArgs::default().as_bytes().len()`: require exactly prefix + one branch byte + 32 preimage bytes, require branch byte `1`, and return `[u8; 32]`. Branch `2` is refund and must return a descriptive error.

- [ ] **Step 3: Add RED provider claim-watch tests**

In `liquidity/chain.rs`, add tests for a callback registered against a provider Loop Out swap:

- valid committed claim sends `ProviderClaimObserved`;
- refund witness sends nothing;
- wrong preimage sends nothing;
- witness at the reported multi-input index is used.

- [ ] **Step 4: Add `watch_provider_claim` to the chain boundary**

Extend `LoopOutChainAdapter`:

```rust
async fn watch_provider_claim(
    &mut self,
    swap_id: Hash256,
    myself: ActorRef<LiquidityActorMessage>,
) -> Result<(), Self::Error>;
```

In `CkbLiquidityChainWatcher`, load the provider Loop Out swap and quote, require `swap.onchain_outpoint`, build the exact liquidity-lock script from the persisted quote and configured artifact, and register `CkbChainMessage::CreateOutPointSpendTracer`.

The callback must:

1. use `result.input_index` to select the matching witness;
2. parse the claim witness;
3. verify `HashAlgorithm::CkbHash.hash(preimage) == swap.payment_hash`;
4. send `ProviderClaimObserved(swap_id)` only on success.

- [ ] **Step 5: Start observation after payment settlement**

In `handle_payment_settled`, provider Loop Out branch, after `mark_provider_payment_settled`, register `watch_provider_claim` unless `watched_claim_swaps` already contains the swap. Insert into the set only after successful registration.

- [ ] **Step 6: Make provider recovery role-specific**

In `resume_swap`, for `RecoveryAction::BroadcastClaim` and role `Provider`, call `watch_provider_claim` instead of `watch_claim`. Apply the same path for `PaymentSettled` and `ClaimPending`; retain existing client broadcast/watch behavior.

Update `mark_provider_claim_observed`:

```rust
if swap.role == LiquiditySwapRole::Provider
    && swap.swap_kind == LiquiditySwapKind::LoopOut
    && swap.state == LiquiditySwapState::Success
{
    return Ok(());
}
```

Wrong roles/kinds and incompatible terminal states remain errors.

- [ ] **Step 7: Run GREEN and commit**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(/provider_.*claim/)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::chain)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::actor)' --no-fail-fast
cargo fmt --all -- --check
git diff --check
git add crates/fiber-lib/src/liquidity/tx.rs crates/fiber-lib/src/liquidity/chain.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: observe provider loop out claims"
```

---

### Task 5: E2E Completion and Verification

**Files:**
- Modify: `crates/fiber-lib/src/tests/liquidity_integration.rs`
- Modify: `crates/fiber-lib/src/tests/liquidity_test_utils.rs`
- Modify: `crates/fiber-lib/src/tests/mod.rs`

- [ ] **Step 1: Run preserved RED E2E**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_ckb_loop_out_e2e)'
```

Expected before Tasks 1-4: client reaches `Success`; provider times out in `PaymentSettled`.

- [ ] **Step 2: Keep the acceptance path public-RPC-only**

The preserved test must:

- transfer quote/import/accept/execute through HTTP RPC helpers;
- pass `provider_accept_loop_out.payout_outpoint` into client `loop_out`;
- use direct mock-chain access only to list/commit pending transactions;
- never send `ProviderClaimObserved` directly or read peer liquidity stores.

After committing the client claim through `MockChainController::commit`, wait for both client and provider `Success` through `get_swap` RPC.

- [ ] **Step 3: Add restart acceptance coverage**

Add a focused integration test that reaches provider `PaymentSettled`, stops/restarts the provider with the same store/backend, commits the pending client claim, and asserts historical outpoint-spend discovery moves the restarted provider to `Success` exactly once.

- [ ] **Step 4: Run complete verification**

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_ckb_loop_out_e2e)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(shared_mock_chain_outpoint_spend)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(rpc::liquidity)'
cargo nextest run -p fiber-json-types -E 'test(liquidity)'
cargo check --locked -p fnn --features rocksdb
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
cargo fmt --all -- --check
make check-migrate
make check-dirty-rpc-doc
git diff --check
```

- [ ] **Step 5: Commit integration tests**

```bash
git add crates/fiber-lib/src/tests/liquidity_integration.rs crates/fiber-lib/src/tests/liquidity_test_utils.rs crates/fiber-lib/src/tests/mod.rs
git commit -m "test: complete CKB Loop Out integration flow"
```

---

## Plan Self-Review

- Spec coverage: exact outpoint discovery (Task 1), reusable tracer (Task 2), mock history/race semantics (Task 3), claim validation and recovery (Task 4), and E2E/restart verification (Task 5).
- Security constraints: exact outpoint, committed confirmations, claim branch, 32-byte preimage, CKB hash, and explicit refund rejection are all tested before provider success.
- Recovery: historical scan and role-specific provider recovery are explicit; no provider-local client claim record is invented.
- Type consistency: every layer uses `packed::OutPoint`, `TransactionView`, and `CkbOutPointSpendTracingResult` as defined in Task 2.
- No canonical client transaction hash is assumed.
