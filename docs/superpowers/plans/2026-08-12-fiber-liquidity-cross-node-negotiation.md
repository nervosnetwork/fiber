# Fiber Liquidity Cross-Node Negotiation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the production prerequisites for cross-node liquidity E2E: immutable transferable quotes, real provider identity, idempotent acceptance, complete live-cell validation, provider controls, durable signed transactions, and public chain-transaction observability.

**Architecture:** Loop Out scripts are finalized during quoting, making `LiquidityQuoteEnvelope` an immutable, lossless transport for persisted `LoopOutQuoteTerms`; import and provider acceptance are get-before-insert idempotent operations. `LiquidityActorArguments` carries the actual Fiber pubkey, `CkbChainActor::GetLiveCell` returns output plus data, and provider administration gains a mutation RPC without gating recovery. Signed CKB transactions are persisted before broadcast and exposed, without witness bytes, through a dedicated query RPC.

**Tech Stack:** Rust, Ractor, jsonrpsee, CKB packed/json types, CKB invoices, RocksDB `LiquidityStore`, cargo-nextest.

---

## File Structure

- Modify `crates/fiber-json-types/src/liquidity.rs`: envelope and import/provider-mode DTOs.
- Modify `crates/fiber-lib/src/liquidity/quote.rs`: envelope conversion and import validation.
- Modify `crates/fiber-lib/src/liquidity/actor.rs`: provider identity, import/provider-mode messages and handlers.
- Modify `crates/fiber-lib/src/rpc/liquidity.rs`: public RPC wiring and tests.
- Modify `crates/fiber-lib/src/rpc/mod.rs`: pass real Fiber pubkey to the actor.
- Modify `crates/fiber-lib/src/ckb/actor.rs`: live-cell output/data value.
- Modify `crates/fiber-lib/src/ckb/tests/test_utils.rs`: mock live-cell parity.
- Modify `crates/fiber-lib/src/liquidity/chain.rs`: complete capacity/type/data validation.
- Modify `crates/fiber-types/src/liquidity.rs`: persist signed transaction bytes with chain transaction records.
- Modify `crates/fiber-lib/src/liquidity/store.rs` and RocksDB implementations: query records by swap and persist signed bytes.
- Add a `crates/fiber-store/src/migrations/` migration for the changed bincode record.
- Regenerate `crates/fiber-lib/src/rpc/README.md`.

This is phase 1 of the approved E2E design. Complete and verify it before executing `2026-08-12-fiber-liquidity-in-process-e2e.md`; the Bruno plan follows the in-process plan.

### Task 1: Define Quote Envelope DTOs

**Files:**
- Modify: `crates/fiber-json-types/src/liquidity.rs`

- [ ] **Step 1: Write failing CKB envelope serialization test**

Add a test constructing an envelope with `provider_pubkey`, `asset`, scripts, and hex amounts. Assert `amount == "0x64"`, provider pubkey round-trips, `client_invoice` is absent, and scripts remain `0x` strings.

- [ ] **Step 2: Run RED test**

Run: `cargo nextest run -p fiber-json-types -E 'test(liquidity_quote_envelope_serializes_complete_ckb_terms)'`

Expected: FAIL because `LiquidityQuoteEnvelope` does not exist.

- [ ] **Step 3: Add transport types**

Add:

```rust
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquidityQuoteEnvelope {
    pub quote_id: Hash256,
    pub swap_kind: LiquiditySwapKind,
    pub provider_pubkey: crate::Pubkey,
    pub asset: LiquidityAssetInfo,
    #[serde_as(as = "U128Hex")]
    pub amount: u128,
    #[serde_as(as = "U128Hex")]
    pub provider_fee: u128,
    #[serde_as(as = "U128Hex")]
    pub routing_fee_limit: u128,
    #[serde_as(as = "U64Hex")]
    pub onchain_fee_estimate_ckb: u64,
    #[serde_as(as = "U64Hex")]
    pub capacity_requirement_ckb: u64,
    pub payment_hash: Hash256,
    #[serde_as(as = "U64Hex")]
    pub expires_at: u64,
    #[serde_as(as = "U64Hex")]
    pub payout_deadline: u64,
    #[serde_as(as = "U64Hex")]
    pub refund_after_lock_time: u64,
    pub claimant_lock: String,
    pub refund_lock: String,
    pub client_invoice: Option<String>,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImportLiquidityQuoteParams {
    pub quote: LiquidityQuoteEnvelope,
    #[serde_as(as = "U128Hex")]
    pub max_provider_fee: u128,
    #[serde_as(as = "U128Hex")]
    pub max_routing_fee: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetLiquidityProviderModeParams {
    pub enabled: bool,
}
```

Add `///` documentation to every public type and field, and add
`#[schemars(schema_with = "schema_as_uint_hex")]` to each hex integer field.
Do not derive `Eq`/`PartialEq` for the envelope unless `LiquidityAssetInfo` gains
the same derives; idempotence compares validated `LoopOutQuoteTerms`.

- [ ] **Step 4: Add UDT and import-param serialization tests**

Assert exact UDT script JSON, 16-byte-related amount values, optional invoice, and fee caps round-trip.

- [ ] **Step 5: Run GREEN tests and commit**

Run: `cargo nextest run -p fiber-json-types -E 'test(liquidity)'`

Stage only `crates/fiber-json-types/src/liquidity.rs`, inspect `git diff --cached`, then commit with `git commit -m "feat: add liquidity quote envelope types"`.

### Task 2: Finalize Loop Out Terms During Quote

**Files:**
- Modify: `crates/fiber-json-types/src/liquidity.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing immutable-quote tests**

Name tests `provider_loop_out_quote_persists_final_scripts` and
`provider_loop_out_accept_does_not_mutate_quote`. Construct distinct valid
claimant/refund Molecule script hex, request a quote, and assert the persisted
terms contain those scripts before acceptance.

- [ ] **Step 2: Run RED tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/provider_loop_out_(quote_persists_final_scripts|accept_does_not_mutate_quote)/)'`

Expected: FAIL because scripts currently arrive at acceptance.

- [ ] **Step 3: Move scripts to quote parameters**

Add documented `claimant_lock` and `refund_lock` fields to
`ProviderQuoteLoopOutParams`. Remove those fields from
`ProviderAcceptLoopOutParams`, leaving only `quote_id`. Parse and validate both
scripts in `handle_provider_quote_loop_out`, persist them in the initial terms,
and remove the quote mutation/reinsert from `handle_provider_accept_loop_out`.
Add the same final script fields to `QuoteLoopOutParams` and pass them through the
local convenience path so local and provider quote semantics remain identical.
Remove the now-redundant `receiver` field from both Loop Out quote DTOs and quote-ID
hashing: the final claimant script is the on-chain destination and no receiver
descriptor may be silently discarded.

- [ ] **Step 4: Run GREEN tests and affected JSON tests**

Run:

```bash
cargo nextest run -p fiber-json-types -E 'test(liquidity)'
cargo nextest run -p fnn --features rocksdb -E 'test(/provider_loop_out_(quote_persists_final_scripts|accept_does_not_mutate_quote)/)'
```

- [ ] **Step 5: Commit scoped files**

Stage the two listed files, inspect the cached diff, and commit with
`git commit -m "fix: finalize loop out terms during quote"`.

### Task 3: Envelope Conversion And Import Validation

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/quote.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs` only if existing script helpers must become `pub(crate)`.

- [ ] **Step 1: Write RED table tests**

Add exact terms -> envelope -> terms equality tests for CKB/UDT and both swap
directions. Add rejection tests for expired quote, provider/routing fee cap,
malformed script hex, CKB with UDT script, UDT without script, gross overflow,
Loop Out missing final scripts or carrying an invoice, Loop In missing invoice,
invoice payment-hash mismatch, invoice amount mismatch, and invoice UDT mismatch.

- [ ] **Step 2: Run RED selection**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(imported_quote)'`

Expected: FAIL because conversion/validation functions do not exist.

- [ ] **Step 3: Implement exact conversion helpers**

Add:

```rust
pub fn liquidity_quote_envelope_from_terms(
    terms: &LoopOutQuoteTerms,
) -> LiquidityQuoteEnvelope;

pub fn validate_imported_quote(
    envelope: LiquidityQuoteEnvelope,
    max_provider_fee: u128,
    max_routing_fee: u128,
    now_ms: u64,
) -> Result<LoopOutQuoteTerms, LiquidityLoopOutError>;
```

Use `fiber_json_types::Pubkey` conversions, `Script::from_slice`, existing invoice parsing/validation, `loop_in_gross_onchain_amount`, and `loop_out_gross_payment_amount`. Preserve the non-optional persisted `payout_deadline` in both directions. Require final non-default scripts and no invoice for Loop Out; require a matching invoice for Loop In. Move the existing JSON asset/script conversions from `actor.rs` into `quote.rs` or expose all required helpers explicitly; never silently discard an unexpected CKB UDT script.

- [ ] **Step 4: Run GREEN quote tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::quote)'`

- [ ] **Step 5: Commit**

Stage the listed files, inspect the cached diff, and commit with `git commit -m "feat: validate imported liquidity quotes"`.

### Task 4: Actor Quote Import Idempotence

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write RED actor tests**

Add tests proving: first import persists; identical import returns the same envelope without another write event; same quote ID with changed amount fails and preserves original; expired/fee-invalid import leaves store unchanged.

- [ ] **Step 2: Run RED tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(import_liquidity_quote)'`

- [ ] **Step 3: Add actor message and handler**

Add:

```rust
ImportLiquidityQuote(
    ImportLiquidityQuoteParams,
    RpcReplyPort<Result<LiquidityQuoteEnvelope, LiquidityLoopOutError>>,
),
```

Handler calls `validate_imported_quote`, checks `get_loop_out_quote`, returns success for equal terms, rejects unequal terms, and inserts only when absent.

- [ ] **Step 4: Run GREEN actor tests and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::actor)'`

Stage `crates/fiber-lib/src/liquidity/actor.rs`, inspect the cached diff, and commit with `git commit -m "feat: import cross-node liquidity quotes"`.

### Task 5: Provider Acceptance Idempotence

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write RED tests for both directions**

Add `provider_accept_loop_out_is_idempotent` and
`provider_accept_loop_in_is_idempotent`. Repeat identical accepts and assert the
same response, one swap, one signed transaction build, one broadcast, and one
watch registration. For Loop In, a repeated request with a different outpoint
must fail and preserve the original swap.

- [ ] **Step 2: Run RED tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/provider_accept_loop_(in|out)_is_idempotent/)'`

- [ ] **Step 3: Implement get-before-side-effect handlers**

If the provider swap already exists, verify direction and, for Loop In, persisted
outpoint equality, then return `swap_response` without rebuilding, broadcasting,
watching, or writing. Reject a conflicting existing swap with a dedicated terms
conflict error.

- [ ] **Step 4: Run GREEN and commit**

Run the RED command again. Stage only `actor.rs`, inspect the cached diff, and
commit with `git commit -m "fix: make liquidity quote acceptance idempotent"`.

### Task 6: Use The Real Provider Identity

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/rpc/mod.rs`

- [ ] **Step 1: Write RED actor identity tests**

Spawn with a non-default fixture pubkey and assert both Loop Out and Loop In envelopes carry that exact pubkey.

- [ ] **Step 2: Add actor argument/state field**

Add `provider_pubkey: fiber_types::Pubkey` to `LiquidityActorArguments` and state. Replace production uses of `deterministic_provider_pubkey()` with `self.provider_pubkey`; update every test actor constructor explicitly.

- [ ] **Step 3: Wire production startup**

Before consuming `fiber_config`, derive:

```rust
let provider_pubkey = fiber_config
    .as_ref()
    .map(|config| crate::fiber::types::pubkey_from_tentacle(config.public_key()));
```

Inside the liquidity-enabled startup branch, return a configuration error if
`fiber_config` is absent; otherwise pass the converted key through actor arguments.
Add a RED startup test for missing configuration before this implementation.

- [ ] **Step 4: Run actor/RPC startup tests and commit**

Run:

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::actor)'
cargo nextest run -p fnn --features rocksdb -E 'test(rpc)'
```

Stage `actor.rs` and `rpc/mod.rs`, inspect the cached diff, and commit with `git commit -m "fix: use node identity for liquidity quotes"`.

### Task 7: Public Quote Envelope And Import RPC

**Files:**
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Regenerate: `crates/fiber-lib/src/rpc/README.md`

- [ ] **Step 1: Write RED registration/delegation/timeout tests**

Cover `import_liquidity_quote`, `provider_quote_loop_out`, and `quote_loop_in`
returning complete envelopes. Assert actor unavailable and timeout errors use
existing liquidity RPC semantics.

- [ ] **Step 2: Run RED RPC tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/rpc::liquidity::.*(import|envelope)/)'`

- [ ] **Step 3: Change provider quote reply types**

Change provider-facing actor/RPC quote methods to `LiquidityQuoteEnvelope`. Keep `quote_loop_out` local convenience only if its response remains unambiguous; cross-node tests must use provider methods plus import.

- [ ] **Step 4: Wire import RPC**

Add trait, server delegate, inherent method, method-name entry, mock arm, and tests using `call_liquidity_actor`.

- [ ] **Step 5: Regenerate docs and run GREEN tests**

Run `make gen-rpc-doc`, then `cargo nextest run -p fnn --features rocksdb -E 'test(rpc::liquidity)'`.

- [ ] **Step 6: Commit**

Stage the listed RPC/actor/doc files, inspect the cached diff, and commit with `git commit -m "feat: expose cross-node liquidity quote RPCs"`.

### Task 8: Preserve Live Cell Data

**Files:**
- Modify: `crates/fiber-lib/src/ckb/actor.rs`
- Modify: `crates/fiber-lib/src/ckb/tests/test_utils.rs`
- Test: `crates/fiber-lib/src/ckb/tests/actor.rs`

- [ ] **Step 1: Write RED output/data round-trip tests**

Construct a live cell with non-empty 16-byte data and assert `GetLiveCell` returns output and exact data; spent/missing outpoints return `None`.

- [ ] **Step 2: Define focused domain value**

```rust
#[derive(Clone, Debug)]
pub struct LiveCell {
    pub output: packed::CellOutput,
    pub data: packed::Bytes,
}
```

Change the message reply to `Option<LiveCell>`. Request `with_data = true`, require
the returned `CellData`, convert `content.into_bytes().pack()`, and return an RPC
error if a supposedly live response omits requested data.

- [ ] **Step 3: Implement mock parity and run tests**

First reject entries marked consumed in `MockChainState`; then read output and data
from the testtool context. Return `None` for missing, pending, rejected, and consumed
outpoints.

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(ckb::tests::actor)'`

- [ ] **Step 4: Commit**

Stage the listed CKB files, inspect the cached diff, and commit with `git commit -m "feat: preserve live cell output data"`.

### Task 9: Complete CKB And UDT Observed-Output Validation

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Write RED validation matrix**

Add tests for valid CKB capacity; wrong liquidity-lock code hash/args payment hash,
claimant lock, refund lock, amount, refund time, and asset hash; unexpected CKB
type script; insufficient CKB capacity; valid UDT type/data; missing data;
15/17-byte malformed data; wrong little-endian UDT amount/type; and insufficient
UDT operational capacity. Every rejection asserts zero payment-adapter calls.

- [ ] **Step 2: Implement validation**

Use `cell.output` and `cell.data`; require capacity >= `capacity_requirement_ckb`. For CKB also require gross amount fits `u64` and capacity >= gross. For UDT require exact type and exactly 16 data bytes decoding to gross amount.

- [ ] **Step 3: Run chain tests and commit**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::chain)'`

Stage `chain.rs`, inspect the cached diff, and commit with `git commit -m "fix: validate observed liquidity cell data"`.

### Task 10: Provider Mode Mutation RPC And Complete Gates

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs`
- Regenerate: `crates/fiber-lib/src/rpc/README.md`

- [ ] **Step 1: Write RED actor tests**

Test enable, repeated enable, disable, returned status, disabled Loop Out quote/accept, disabled Loop In quote/accept, and `ResumeNonTerminal` continuing while disabled.

- [ ] **Step 2: Add actor mutation**

Add `SetLiquidityProviderMode(SetLiquidityProviderModeParams, reply)`. Handler
reads the current value and writes only when it changes, then returns
`handle_get_liquidity_provider_status`. Gate `handle_quote_loop_in` in addition to
existing provider handlers; do not gate recovery messages.

- [ ] **Step 3: Run actor RED tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/provider_mode.*(idempotent|gates|recovery)/)'`

- [ ] **Step 4: Add RPC registration/delegation tests and implementation**

Wire `set_liquidity_provider_mode` through the existing actor timeout helper.

- [ ] **Step 5: Regenerate docs, verify, and commit**

Run:

```bash
make gen-rpc-doc
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::actor)'
cargo nextest run -p fnn --features rocksdb -E 'test(rpc::liquidity)'
```

Stage the listed actor/RPC/doc files, inspect the cached diff, and commit with `git commit -m "feat: expose liquidity provider mode control"`.

### Task 11: Encode Refund Lock Time As CKB Since

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/quote.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/tx.rs` if builders do not already pass the persisted value unchanged

- [ ] **Step 1: Write RED encoding tests**

Add `refund_lock_time_is_absolute_timestamp_since` and
`refund_lock_time_rounds_milliseconds_up_to_seconds`. Decode the quote value with
`ckb_sdk::Since`; assert `SinceType::Timestamp`, absolute mode, payload equal to
`refund_unix_ms.div_ceil(1000)`, and no reuse of raw expiry milliseconds.

- [ ] **Step 2: Run RED tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/refund_lock_time_.*since|refund_lock_time_rounds/)'`

- [ ] **Step 3: Implement one encoding helper**

Add a checked helper that computes the target Unix milliseconds, rounds up to
seconds, and returns
`Since::new(SinceType::Timestamp, unix_seconds, false).value()`. Use it for both
directions. Builders copy the persisted encoded value directly to input `since`.

- [ ] **Step 4: Run GREEN and commit**

Run the RED command again. Stage only quote/actor/tx files, inspect the cached diff,
and commit with `git commit -m "fix: encode liquidity refund lock time as CKB since"`.

### Task 12: Persist Signed Transactions For Restart Recovery

> **Implementation note (user-approved deviation):** persisted under a dedicated
> store key (`LiquidityChainTxSignedTx`) instead of an inline `LiquidityChainTxRecord`
> field, avoiding a bincode migration and new `fiber-store` dependencies. Payout and
> Loop In lock are persisted; claim/refund are deterministic rebuilds and are not
> persisted. See spec section 1.7.

**Files:**
- Modify: `crates/fiber-types/src/schema.rs` (new key prefix)
- Modify: `crates/fiber-lib/src/liquidity/store.rs` (trait methods)
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs` and mock stores
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Write RED serialization/store tests**

Add `insert/get_liquidity_chain_tx_signed_tx` store methods and round-trip a real
packed transaction for the payout role (Loop In lock reuses the `Payout` role).

- [ ] **Step 2: Run RED tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_chain_tx_signed_tx)'`

- [ ] **Step 3: Persist before broadcast**

Persist payout and Loop In lock signed bytes before `SendTx`; remote-observed
records never store bytes. Never expose signed bytes through JSON-RPC.

- [ ] **Step 4: Write RED restart/hash tests**

Recreate `CkbLiquidityChainWatcher` with an empty in-memory pending map and assert
it reloads and rebroadcasts the exact transaction. Add missing bytes and hash
mismatch tests that fail closed.

- [ ] **Step 5: Implement safe reload and run GREEN**

Decode persisted bytes, compare `tx.hash()` with the chain record, and only then
rebroadcast. Run the focused selection, `liquidity::chain`, store, and `make check-migrate`.

- [ ] **Step 6: Commit scoped persistence changes**

Commit with `git commit -m "feat: persist and reload signed liquidity transactions"`.

### Task 13: Public Chain Transaction Query RPC

> **Implementation note (user-approved deviation):** no stored `LoopInLock` role is
> added. The actor maps a Loop In swap's stored `Payout` role to a `loop_in_lock`
> semantic label in the JSON response. See spec section 1.7.

**Files:**
- Modify: `crates/fiber-json-types/src/liquidity.rs`
- Modify: `crates/fiber-lib/src/liquidity/store.rs` and store implementations
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs`
- Regenerate: `crates/fiber-lib/src/rpc/README.md`

- [ ] **Step 1: Write RED DTO/store tests**

Define documented `ListLiquidityChainTransactionsParams { swap_id }`, public
record/response DTOs containing role, hash, optional outpoint, status, failure
reason, and timestamps, but no signed bytes. Test stable
`payout`/`loop_in_lock`/`claim`/`refund` ordering and an empty result for an
unknown swap.

- [ ] **Step 2: Run RED tests**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(/list_liquidity_chain_transactions/)'`

- [ ] **Step 3: Add store, actor, and RPC implementation**

Add `list_liquidity_chain_txs_by_swap(&Hash256)`, map the stored role to the
semantic JSON label (Loop In `Payout` → `loop_in_lock`), add the actor message,
and register/delegate `list_liquidity_chain_transactions` with existing
timeout/error semantics.

- [ ] **Step 4: Run GREEN, regenerate docs, and commit**

Run:

```bash
cargo nextest run -p fnn --features rocksdb -E 'test(/list_liquidity_chain_transactions/)'
make gen-rpc-doc
```

Stage the listed files and generated docs, inspect the cached diff, and commit with
`git commit -m "feat: expose liquidity chain transactions"`.

### Task 14: Negotiation Verification

- [ ] Run:

```bash
cargo nextest run -p fiber-json-types -E 'test(liquidity)'
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(rpc)' --no-fail-fast
cargo check --locked -p fnn --features rocksdb
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
make check-migrate
make gen-rpc-doc
make check-dirty-rpc-doc
```

- [ ] Request spec and code-quality review; fix all Critical/Important findings.
