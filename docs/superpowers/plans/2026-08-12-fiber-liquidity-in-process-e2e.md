# Fiber Liquidity In-Process E2E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build deterministic multi-node liquidity integration tests using real Fiber networking, liquidity actors, RocksDB, CKB transaction builders/tracers, and the real liquidity-lock contract under CKB testtool.

**Architecture:** Add the pinned contract artifact to MockContext, share one controllable mock chain across nodes, and create a focused `LiquidityNetworkNode` test fixture that drives only public RPC. Tests explicitly commit/reject transactions to verify ordering, state transitions, contract enforcement, idempotence, refunds, and recovery.

**Tech Stack:** Rust, Tokio, Ractor, NetworkNode, ckb-testtool, RocksDB, cargo-nextest.

---

## File Structure

- Add `tests/deploy/contracts/liquidity-lock` and document provenance.
- Modify `crates/fiber-lib/src/ckb/tests/test_utils.rs`: deploy contract and add shared controllable chain behavior.
- Modify `crates/fiber-lib/src/tests/test_utils.rs`: shared chain injection and liquidity-enabled RPC startup.
- Add `crates/fiber-lib/src/tests/liquidity_test_utils.rs`: focused fixture and bounded polling.
- Add `crates/fiber-lib/src/tests/liquidity_integration.rs`: integration matrix.
- Modify `crates/fiber-lib/src/tests/mod.rs`: register modules.
- Modify `crates/fiber-lib/src/liquidity/payment.rs`: propagate the exact UDT script through real payments.
- Modify `.config/nextest.toml`: serialize chain-time/restart tests.

**Prerequisite:** Complete every task in
`docs/superpowers/plans/2026-08-12-fiber-liquidity-cross-node-negotiation.md`.
Before Task 1, run its Task 14 verification. This plan depends on immutable quote
envelopes, import/provider-mode/chain-transaction RPCs, live-cell data, and durable
signed transaction bytes.

### Task 1: Pin And Deploy Liquidity-Lock Artifact

- [ ] **Step 1: Write RED contract lookup smoke test**

Add `liquidity_lock_mock_context_resolves_script_and_deps`. Assert
`try_get_script_by_contract(Contract::LiquidityLock, &[])` returns a script and
`get_cell_deps_by_contracts(vec![Contract::LiquidityLock]).await` is non-empty;
both resolve through the test `MOCK_CONTEXT`.

- [ ] **Step 2: Build and copy the exact artifact**

Use sibling checkout `${FIBER_SCRIPTS_DIR:-../fiber-scripts}` at commit
`4da5d299fe449bb799bf057e0bb1f02c8fe7d101`. From that checkout run:

```bash
mkdir -p build/release
make -C contracts/liquidity-lock build TOP="$(pwd)/" BUILD_DIR=build/release
shasum -a 256 build/release/liquidity-lock
```

Copy `build/release/liquidity-lock` to
`tests/deploy/contracts/liquidity-lock`. Record the source commit, exact command,
and expected SHA-256
`b7bfe051b6062f1ebb814ddf392406f86df9f403280eb48ec93a4d141c72dc4e`
in `tests/deploy/contracts/README.md`; fail if the checksum differs.

- [ ] **Step 3: Deploy in MockContext**

Add `(Contract::LiquidityLock, include_bytes!(...))` to the fixed binary array and register its code dep.

- [ ] **Step 4: Add VM enforcement tests**

Use the existing CKB testtool context to prove valid claim succeeds, wrong preimage fails, and early refund fails.

- [ ] **Step 5: Run and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_lock_mock_context)'`

Stage the artifact, README, and exact CKB test files, inspect
`git diff --cached`, then commit with
`git commit -m "test: deploy liquidity lock in mock chain"`.

### Task 2: Shared Controllable Mock Chain

- [ ] **Step 1: Write RED two-node visibility test**

Add `shared_mock_chain_exposes_committed_cells_to_both_nodes`. Submit through A,
assert the output is absent while pending, commit it, then assert B receives exact
output and data through `GetLiveCell` and its tracer.

- [ ] **Step 2: Add shared-state injection**

Expose the existing `Arc<RwLock<MockChainState>>` through
`NetworkNodeConfigBuilder`, `NetworkNodeConfig`, and `NetworkNode`; preserve the
same Arc through `get_node_config` and restart instead of creating a new state.

- [ ] **Step 3: Add transaction controller middleware**

Create one fixture-owned backend containing the testtool `Context` plus the
Arc-backed transaction/tracer state, with `pending`, `commit(tx_hash)`, and
`reject(tx_hash, reason)`. `SendTx` stores `Pending`; commit performs VM
verification, atomically consumes inputs/creates outputs, stores `Committed`, and
notifies tracers; reject stores `Rejected`, consumes/creates nothing, and notifies
tracers. Repeating the same resolution is a no-op; a conflicting resolution fails.
Perform verification and state mutation under one backend critical section. Keep
immediate commit as the default for existing tests.

- [ ] **Step 4: Run visibility/order tests and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(shared_mock_chain)'`

Stage only shared-chain fixture/tests, inspect the cached diff, and commit with
`git commit -m "test: share controllable CKB state across nodes"`.

### Task 3: Liquidity-Enabled Network Fixture

- [ ] **Step 1: Write RED RPC smoke test**

Create two nodes with liquidity RPC enabled and assert `get_liquidity_provider_status` responds rather than `actor unavailable`.

- [ ] **Step 2: Pass chain actor into start_rpc**

Change the NetworkNode test startup call from `ckb_chain_actor: None` to `Some(chain_actor.clone())` when configured. Add a `gen_liquidity_rpc_config()` helper that appends `liquidity` without changing default suites.

- [ ] **Step 3: Add `LiquidityNetworkNode` helper**

Compose two interconnected nodes, shared chain controller, funded payment channel,
RPC quote/import helpers, provider/asset initialization, and bounded
`wait_for_swap_state`/`wait_for_chain_tx` diagnostics. Own RPC, liquidity, chain,
network, and spawned-task handles; stop and await the old runtime before restart.
All quote transfer and lifecycle assertions call RPC module methods. Controlled
chain inspection is permitted only to resolve pending transactions and verify VM
effects, never to copy or mutate peer liquidity stores.

- [ ] **Step 4: Run smoke test and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_network_fixture)'`

Stage only fixture files and registration, inspect the cached diff, and commit with
`git commit -m "test: add liquidity network fixture"`.

### Task 4: CKB Loop Out Happy Path

- [ ] **Step 1: Write RED public-RPC test**

Use provider quote envelope with final scripts -> client import -> provider accept
by quote ID -> client execute. Hold payout pending and assert no payment record and
unchanged channel balances; commit payout; assert payment settlement, claim record
from `list_liquidity_chain_transactions`, claim confirmation, and `Success` on both
nodes via RPC.

- [ ] **Step 2: Implement only missing fixture helpers**

Do not access peer stores. Capture tx hashes/outpoints from public swap/chain response helpers or controlled-chain observations.

- [ ] **Step 3: Run GREEN and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_ckb_loop_out_e2e)'`

Stage only the integration test and required fixture helpers, inspect the cached
diff, and commit with `git commit -m "test: add CKB Loop Out integration flow"`.

### Task 5: CKB Loop In Happy Path

- [ ] **Step 1: Write RED flow test**

Create client invoice, request/import envelope, execute local-wallet lock, hold confirmation, assert no provider payment, commit lock, call provider accept with public tx/outpoint identity, settle payment, verify provider claim under CKB-VM, and confirm both sides `Success`.

- [ ] **Step 2: Run GREEN and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_ckb_loop_in_e2e)'`

Stage only the integration test and required fixture helpers, inspect the cached
diff, and commit with `git commit -m "test: add CKB Loop In integration flow"`.

### Task 6: UDT Loop Out And Loop In

- [ ] **Step 1: Write RED UDT payment test**

Add `network_liquidity_payment_preserves_udt_type_script`. Add
`udt_type_script: Option<packed::Script>` to `LoopOutPaymentRequest`, populate it
from persisted quote terms in direct and invoice paths, and map it into
`SendPaymentCommand` instead of `None`. Use one fixture-owned UDT script byte-for-byte
for provider asset, channel funding type, whitelist, lock/payout output, and payment.

- [ ] **Step 2: Add UDT Loop Out happy path**

Assert preserved type/data through payout and claim.

- [ ] **Step 3: Add UDT Loop In happy path**

Assert 16-byte little-endian gross amount in observed lock and preserved type/data through provider claim.

- [ ] **Step 4: Run and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/liquidity_udt_loop_(in|out)_e2e/)'`

Stage payment and integration files, inspect the cached diff, and commit with
`git commit -m "test: cover UDT liquidity integration flows"`.

### Task 7: Validation And Contract Failure Matrix

- [ ] Add one table row each for payment hash, claimant/refund lock, refund `since`,
  gross amount/capacity, UDT type, missing data, 15/17-byte data, and wrong UDT
  amount. Each row starts from a named state and asserts zero provider payments.
- [ ] Add wrong-preimage CKB-VM rejection and one-unit-before-maturity refund
  rejection tests; advance the mock chain according to the encoded `since` metric.
- [ ] Add provider disabled for all quote/accept RPCs, recovery while disabled,
  disabled asset, expired quote, fee caps, duplicate import/accept/confirmation,
  rejected payout, and conflicting duplicate Loop In outpoint tests.
- [ ] Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/liquidity_.*(reject|mismatch|idempotent)/)'`
- [ ] Stage only matrix tests/helpers, inspect the cached diff, and commit with
  `git commit -m "test: cover liquidity validation and failure paths"`.

### Task 8: Refund And Recovery Matrix

- [ ] **Step 1: Add controllable lock-time tests**

Cover Loop Out provider refund and Loop In client refund; verify early refund rejected and eligible refund confirmed.

- [ ] **Step 2: Add every-state recovery table**

For each reachable role/direction combination in `Created` and `Quoted`, assert
restart resumes no premature payment or chain side effect. Then cover each Loop
Out state `PayoutPending`, `PayoutLocked`, `PaymentInFlight`,
`PaymentSettled`, `ClaimPending`, and `RefundPending`, and each Loop In state
`OnchainLockPending`, `OnchainLocked`, `PaymentInFlight`, `PaymentSettled`,
`ClaimPending`, and `RefundPending`, stop and await the old runtime, restart with
the same RocksDB store, and assert the exact resumed watch/payment/rebroadcast.
For transaction-pending states, assert the signed bytes reload to the same hash.
Every row asserts no duplicate payment, transaction, or watcher registration.

- [ ] **Step 3: Configure serial nextest group**

Add:

```toml
[test-groups.liquidity-chain-control]
max-threads = 1

[[profile.default.overrides]]
filter = 'test(/liquidity_.*(refund|recovery)/)'
test-group = 'liquidity-chain-control'
```

Apply the same test group to every integration test using the shared mutable
testtool backend, not only names matching refund/recovery.

- [ ] **Step 4: Run and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/liquidity_.*(refund|recovery)/)' --no-fail-fast`

Stage recovery tests/helpers and `.config/nextest.toml`, inspect the cached diff,
and commit with `git commit -m "test: cover liquidity refund and recovery"`.

### Task 9: In-Process Verification

- [ ] Run the complete liquidity selection three separate times without `--retries`:
  `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)' --no-fail-fast`.
- [ ] Run `cargo check --locked -p fnn --features rocksdb`,
  `cargo fmt --all -- --check`,
  `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`,
  `make check-migrate`, and `make check-dirty-rpc-doc`.
- [ ] Request spec and code-quality review and fix all blocking findings.
