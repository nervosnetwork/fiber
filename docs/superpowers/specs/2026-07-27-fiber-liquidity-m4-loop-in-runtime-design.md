# Fiber Liquidity M4 Loop In Runtime Design

## Summary

M4 adds the first manual Loop In runtime slice. Loop In moves a user's on-chain
CKB or UDT into Fiber channel balance: the client locks on-chain funds, the
provider pays the client's Fiber invoice after the lock is confirmed, and the
provider claims the on-chain lock with the revealed preimage.

This slice intentionally mirrors M3 Loop Out's safety model while keeping scope
small: persist before side effects, keep chain transactions durable, make
recovery idempotent, and avoid provider discovery or automated planning.

## Goals

- Add runtime handling for `quote_loop_in` and `loop_in` RPC flows.
- Persist Loop In swaps and quotes before chain or payment side effects.
- Add explicit recovery actions for Loop In non-terminal states.
- Reuse the liquidity-lock script for client lock, provider claim, and client
  refund paths.
- Reuse M3's chain transaction record lifecycle: `Planned`, `Broadcast`,
  `Confirmed`, `Rejected`.
- Preserve CKB and UDT asset rules from `docs/specs/liquidity-management.md`.

## Non-Goals

- Do not add provider discovery or new Fiber p2p messages.
- Do not add automated liquidity planning or rebalancing.
- Do not redesign the liquidity-lock script.
- Do not add a separate wallet abstraction beyond the existing CKB actor
  boundary.
- Do not implement background strategy selection; Loop In remains manually
  requested through RPC.

## Existing Model Reuse

M4 should reuse the M3 shape instead of creating parallel concepts:

- `LiquiditySwapKind::LoopIn` identifies direction.
- `LiquiditySwapRole::{Client, Provider}` identifies local responsibility.
- `LiquidityChainTxRole` remains the durable tx identity key. For Loop In:
  `Payout` represents the client on-chain lock, `Claim` represents the provider
  claim, and `Refund` represents the client refund.
- `Contract::LiquidityLock` and `CkbLiquidityChainWatcher` provide the script
  artifact, deps, send, and trace boundary.
- `LoopOutPaymentAdapter` should not be reused by name for Loop In long-term;
  either generalize it to a direction-neutral payment adapter or add a small
  Loop In payment method that still delegates to the existing payment actor.

## Client Flow

1. The client creates or supplies a Fiber invoice whose payment hash is the Loop
   In `payment_hash`.
2. The client requests `quote_loop_in` from the provider with `asset_id`,
   `amount`, invoice, fee limits, and expiry.
3. The provider returns terms including `provider_fee`, routing fee estimate,
   `refund_after_lock_time`, lock scripts, and expiry.
4. The client executes `loop_in(quote_id, funding_tx | wallet funding params)`.
5. The client persists the swap as `LoopIn` before any chain side effect.
6. The client broadcasts a liquidity-lock transaction for gross on-chain amount
   `amount + provider_fee`.
7. After the lock transaction confirms, the client moves to `OnchainLocked` and
   waits for the Fiber payment to settle.
8. If the Fiber payment settles, the client reaches `Success` after observing the
   provider claim or a confirmed payment outcome.
9. If settlement does not happen before the refund path is available, the client
   broadcasts a refund and reaches `Refunded` after confirmation.

## Provider Flow

1. The provider handles `quote_loop_in` using its asset registry and fee policy.
2. After observing a matching confirmed client on-chain lock, the provider sends
   the Fiber payment to the client's invoice/payment hash.
3. Payment settlement reveals the preimage to the provider.
4. The provider persists the preimage before broadcasting any claim transaction.
5. The provider claims the client lock using the liquidity-lock claim witness.
6. After claim confirmation, the provider marks the swap `Success`.

## Chain Transactions

### Client Lock

The client lock transaction creates a liquidity-lock output with:

- `payment_hash` from the client invoice.
- `claimant_lock` bound to the provider claim identity.
- `refund_lock` bound to the client refund identity.
- `refund_after_lock_time` from the quote.
- gross locked amount `amount + provider_fee`, represented as CKB value for CKB
  swaps and as UDT cell data for UDT swaps, as specified by the canonical
  protocol.
- UDT type script and UDT deps when the asset is UDT.

The lock tx must be persisted as `LiquidityChainTxRole::Payout` before `SendTx`
and marked `Broadcast` only after successful submission.

### Provider Claim

The provider claim transaction spends the confirmed client lock using:

- claim witness `WitnessArgs::default().as_bytes() || 0x01 || preimage`.
- output lock controlled by the provider.
- UDT type/data preserved for UDT assets.
- liquidity-lock deps and UDT deps.

The claim tx uses `LiquidityChainTxRole::Claim` and follows the M3 status
ordering and retry rules.

### Client Refund

The client refund transaction spends the lock after `refund_after_lock_time`
using:

- refund witness `WitnessArgs::default().as_bytes() || 0x02`.
- input `since == refund_after_lock_time`.
- output lock controlled by the client.
- UDT type/data preserved for UDT assets.

The refund tx uses `LiquidityChainTxRole::Refund`. Late rejections must not
downgrade confirmed records.

## Recovery

Recovery should be explicit by state and role.

Client-side non-terminal recovery:

- `OnchainLockPending`: watch an existing broadcast lock tx or rebroadcast a
  retryable lock tx.
- `OnchainLocked`: wait for payment settlement or refund eligibility.
- `PaymentInFlight`: reload/reconcile the payment if the client initiated any
  local payment-observation task.
- `RefundPending`: watch or retry the refund tx.

Provider-side non-terminal recovery:

- `OnchainLockPending`: watch the client lock if the provider has a persisted tx
  identity or observed outpoint.
- `OnchainLocked`: send or resume the Fiber payment.
- `PaymentInFlight`: reload/reconcile payment status.
- `PaymentSettled`: broadcast or retry claim using the persisted preimage.
- `ClaimPending`: watch or retry the claim tx.

Recovery must remain idempotent. Broadcast/confirmed tx records should not be
rebroadcast; planned/rejected records may be retried only when the rebuilt tx hash
matches the persisted tx hash or the old record is explicitly replaced by a safe
new plan.

## RPC Scope

M4 exposes the existing JSON types as runtime paths:

- `quote_loop_in`: returns a quote with on-chain lock terms.
- `loop_in`: accepts a quote and starts the manual Loop In execution.
- Existing `get_swap` and `list_swaps` should include Loop In records without a
  separate response shape.

The first slice may keep `funding_tx` semantics narrow: accept a client-provided
funding transaction identity or use the existing CKB actor funding path, but it
must not silently accept an unsigned or untraceable transaction.

## Error Handling

- Missing liquidity-lock script artifacts must fail closed, as in M3.
- Fee overflow and gross amount overflow must return structured liquidity errors.
- UDT type mismatches must fail before persistence or side effects when possible.
- Send failures mark the chain tx `Rejected` with a reason and preserve retry.
- Tracer rejection marks the chain tx `Rejected` unless it is already
  `Confirmed`.
- Terminal swap states must not be downgraded by late callbacks.

## Testing

The implementation plan should use TDD and add focused coverage for:

- Loop In quote validation and gross amount calculation.
- Client lock transaction builder for CKB and UDT.
- Provider claim transaction builder and preimage validation.
- Client refund transaction builder with exact `since`.
- Persist-before-side-effect ordering for lock, claim, and refund.
- Recovery action mapping for Loop In states.
- Rejected tracer retry and confirmed-status monotonicity.
- RPC fail-closed behavior when liquidity runtime wiring is unavailable.

Targeted verification should include:

- `cargo test -p fnn --features rocksdb liquidity::tx -- --nocapture`
- `cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture`
- `cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture`
- `cargo test -p fnn --features rocksdb liquidity::quote -- --nocapture`
- `cargo check -p fnn --features rocksdb`
- `cargo fmt --all -- --check`

## Open Implementation Decisions

- Whether to generalize payment adapter naming now or keep a minimal Loop In
  wrapper around existing payment actor calls.
- Whether first-slice `loop_in` accepts only existing wallet funding parameters
  or also supports externally supplied funding transaction identities.
- Whether provider-side lock observation starts from a client-submitted outpoint
  or from a chain scan keyed by liquidity-lock args.

These decisions should be resolved in the M4 implementation plan before code is
changed.
