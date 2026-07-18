# Fiber Liquidity M3 CKB Chain Integration Design

## Status

Approved direction for implementation planning.

## Context

M3 runtime wiring now has a real `LiquidityActor`, mutation RPC delegation, durable Loop Out quotes, a real Fiber payment adapter, startup recovery, and a deterministic Manual Loop Out harness. The remaining intentional gap is `CkbLiquidityChainWatcher`: its payout, claim, refund, and watch methods fail closed with `not_wired` errors instead of building, broadcasting, or tracing real CKB transactions.

This design wires the full M3 Loop Out chain path behind `CkbLiquidityChainWatcher` while preserving the runtime invariants already enforced by the actor:

- persist before side effects;
- pay only after payout confirmation;
- validate and persist preimage before claim;
- mark success only after claim confirmation or provider claim observation;
- keep recovery idempotent and avoid duplicate economic actions.

## Goals

- Build, send, and trace provider payout lock transactions.
- Build, send, and trace client claim transactions.
- Build, send, and trace provider refund transactions.
- Persist CKB transaction identity before or atomically with broadcast intent.
- Reuse existing CKB actor sending/tracing APIs and existing wallet/funding machinery where possible.
- Make restart recovery watch known transactions instead of rebuilding duplicate payouts, claims, or refunds.
- Keep the scope limited to M3 Manual Loop Out.

## Non-Goals

- Do not implement Loop In.
- Do not add provider discovery, gossip, or Fiber p2p liquidity messages.
- Do not replace the existing `CkbChainActor`, transaction tracing actor, or funding machinery.
- Do not add a separate liquidity wallet/input-selection subsystem unless existing funding APIs cannot express a required transaction.
- Do not mark chain operations successful without CKB confirmation callbacks.

## Recommended Approach

Use persisted transaction-plan records plus existing CKB actor funding, sending, and tracing.

`CkbLiquidityChainWatcher` becomes the production adapter for payout, claim, and refund. It does not own authoritative swap state. It builds transaction plans from persisted swap and quote terms, persists transaction identity, sends transactions through `CkbChainMessage::SendTx`, and registers confirmation tracers through `CkbChainMessage::CreateTxTracer`.

Recovery checks persisted transaction identity first. If a transaction identity exists, recovery watches that transaction. If no transaction identity exists, recovery rebuilds only when the current swap state makes the side effect safe. Missing identity in unsafe states fails closed and records recoverable context.

## Data Model

Add focused persisted chain transaction records rather than opaque JSON blobs.

Each record should contain:

- `swap_id`: local liquidity swap id.
- `role`: one of `payout`, `claim`, `refund`.
- `tx_hash`: CKB transaction hash.
- `outpoint`: optional output outpoint, required for provider payout records.
- `status`: local lifecycle such as `planned`, `broadcast`, `confirmed`, `rejected`.
- `created_at` and `updated_at` timestamps.
- optional `failure_reason` for recoverable tracing or broadcast errors.

The store API should support:

- insert transaction identity before broadcast;
- lookup by `swap_id` and role;
- update transaction status/failure reason;
- list active transaction records for recovery if needed.

Persisted schema types must live in `fiber-types` or otherwise satisfy `make check-migrate`.

The existing `LiquiditySwapRecord.onchain_outpoint` remains the convenient payout lock location. The transaction record is the durable broadcast/watch identity; `onchain_outpoint` is the payout cell identity used by claim and refund builders.

## Provider Payout Flow

Provider accept flow already persists the provider swap and accepted claimant/refund locks before chain side effects. The CKB integration extends the chain step:

1. Load accepted `LoopOutQuoteTerms` and provider `LiquiditySwapRecord`.
2. Build the liquidity-lock output using `build_loop_out_payout_output`.
3. Use existing CKB funding machinery to select inputs, pay fees, and create a signed payout transaction.
4. Derive the payout lock output outpoint from the transaction hash and output index.
5. Persist the payout transaction record and `LiquiditySwapRecord.onchain_outpoint` before `SendTx`.
6. Send through `CkbChainMessage::SendTx`.
7. Register a `CkbTxTracer` for the payout transaction.
8. On enough confirmations, send `LiquidityActorMessage::PayoutConfirmed(swap_id)`.

If transaction construction fails before a transaction identity exists, leave the swap in its current recoverable state and persist `failure_reason`. If `SendTx` reports duplicate or already-in-pool, treat it as success-equivalent, matching current `CkbChainActor` behavior.

## Client Claim Flow

The client claim path starts only after a valid preimage is persisted and the swap reaches `PaymentSettled`.

1. Validate `payment_preimage` against `payment_hash` using `LoopOutClaimPlan`.
2. Persist `ClaimPending` before claim broadcast intent.
3. Build a claim transaction spending the persisted payout outpoint with the preimage witness required by the liquidity-lock script.
4. Use existing funding machinery for any required fee inputs/change.
5. Persist a claim transaction record before `SendTx`.
6. Send through `CkbChainMessage::SendTx`.
7. Register a `CkbTxTracer` for the claim transaction.
8. On enough confirmations, send `LiquidityActorMessage::ClaimConfirmed(swap_id)`.

If a claim transaction record already exists, recovery watches that transaction rather than rebuilding. If `ClaimPending` exists without a claim transaction record because the node crashed after state persistence but before transaction identity persistence, recovery may rebuild and broadcast the deterministic claim transaction, then persist the transaction record before sending.

## Provider Refund Flow

Provider refund is available only after `refund_after_lock_time` and only for provider-side swaps whose payout lock remains unclaimed.

1. Confirm the persisted payout outpoint exists.
2. Confirm the swap is in a refund-eligible state and lock time is satisfied.
3. Build a refund transaction spending the payout lock through the refund path.
4. Use existing funding machinery for required fees/change.
5. Persist a refund transaction record before `SendTx`.
6. Send through `CkbChainMessage::SendTx`.
7. Register a `CkbTxTracer` for the refund transaction.
8. On enough confirmations, send `LiquidityActorMessage::RefundConfirmed(swap_id)`.

If refund identity exists, recovery watches it. If no payout outpoint exists, refund recovery fails closed and records recoverable context.

## Watchers And Confirmations

`CkbLiquidityChainWatcher` should create `CkbTxTracer` entries for payout, claim, and refund transactions. Each tracer callback maps CKB tracing results into liquidity actor messages:

- payout committed with enough confirmations -> `PayoutConfirmed`;
- claim committed with enough confirmations -> `ClaimConfirmed`;
- refund committed with enough confirmations -> `RefundConfirmed`;
- rejected/permanent send error -> persist transaction failure context and keep swap recoverable unless a safe terminal path exists.

Confirmation handlers must be idempotent. Duplicate callbacks should not crash the actor and should not produce invalid state transitions.

## Recovery Rules

Recovery must be idempotent.

- `PayoutPending` with payout tx record: watch payout tx.
- provider `PayoutPending` without tx record: rebuild payout only if accepted quote terms are present and no previous payout identity exists.
- client `PayoutPending` without tx record: passively watch for provider payout using persisted quote/payment terms if supported; never broadcast a provider payout.
- `PayoutLocked`: resume payment eligibility or provider payment watching as implemented by M3 runtime wiring.
- `PaymentInFlight`: reload/reconcile payment status.
- `PaymentSettled`: build or resume claim transaction using persisted valid preimage.
- `ClaimPending` with claim tx record: watch claim tx.
- `ClaimPending` without claim tx record: rebuild claim transaction and persist tx identity before send.
- `RefundPending` with refund tx record: watch refund tx.
- `RefundPending` without refund tx record and payout outpoint: build refund only when lock time is satisfied.
- missing payout identity in refund path: fail closed with recoverable failure context.

Repeated `ResumeNonTerminal` in the same actor lifetime must not schedule duplicate active watches or broadcasts. After restart, re-watching persisted transaction identities is acceptable.

## Error Handling

- Transaction construction failures happen before transaction record persistence and leave swap state recoverable.
- Store failures abort before CKB side effects.
- Broadcast failures after transaction record persistence keep the transaction record and rely on tracing or later recovery to determine final status.
- Duplicate/in-pool responses from `CkbChainActor::SendTx` are success-equivalent.
- Rejections reported by tracing are persisted as failure context. They do not automatically mark swaps terminal unless a safe terminal transition exists.
- Missing preimage or preimage/hash mismatch prevents claim construction.
- Missing payout outpoint prevents claim/refund construction.
- Confirmation timeout or unknown transaction status keeps swaps non-terminal.

## Testing Strategy

Required tests:

- Build deterministic payout output for CKB and UDT using accepted quote terms.
- Provider payout persists tx hash and outpoint before `SendTx`.
- Client claim persists tx hash before `SendTx` and never marks success before confirmation.
- Provider refund persists tx hash before `SendTx` and never marks refunded before confirmation.
- Watcher registers `CkbTxTracer` after successful or duplicate/in-pool send.
- Payout, claim, and refund tracer callbacks send the correct liquidity actor continuation messages.
- Recovery with existing payout/claim/refund tx record watches instead of rebuilding.
- `ClaimPending` without claim tx record rebuilds claim safely and persists tx identity before send.
- Missing payout identity in refund recovery fails closed.
- Construction failure before tx identity causes no send.
- Store failure before tx identity causes no send.
- Broadcast failure after tx identity leaves identity persisted and swap recoverable.
- Duplicate confirmation callbacks are idempotent.

Verification commands:

- `cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture`
- `cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture`
- `cargo test -p fnn --features rocksdb test_store_liquidity -- --nocapture`
- `cargo check -p fnn --features rocksdb`
- `make check-migrate`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`

## Open Constraints For Implementation

- If existing funding machinery cannot directly build claim/refund transactions, add the smallest liquidity-specific transaction builder that still uses existing signing, fee, and send/tracing paths.
- If a transaction identity field is missing from current store APIs, add focused persisted transaction records rather than extending `LiquiditySwapRecord` with multiple role-specific fields.
- If passive client payout watching cannot be implemented safely without scanning chain state by script args, keep that path fail-closed and document the limitation in the implementation plan.
