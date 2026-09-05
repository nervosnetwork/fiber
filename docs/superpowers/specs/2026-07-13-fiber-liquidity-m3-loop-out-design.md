# Fiber Liquidity M3 Manual Loop Out Design

## Status

Approved design for M3 implementation planning.

## Context

M0 defined the liquidity protocol in `docs/specs/liquidity-management.md`. M1
added the `liquidity-lock` script, Fiber-side liquidity types, JSON-RPC DTOs,
transaction output builders, and storage traits. M2 implemented durable
`LiquidityStore` persistence, state transition validation, recovery queries, and
provider asset registry persistence.

M3 ships the first complete manual liquidity product: Loop Out. Loop Out moves
Fiber channel balance to an on-chain CKB or UDT receiver. It must work between a
client FNN and a provider FNN using RPC coordination while Fiber p2p continues to
carry ordinary payments only.

## Goals

- Implement a real end-to-end Loop Out path between two FNN nodes.
- Add client RPCs for `quote_loop_out`, `loop_out`, `get_swap`, and
  `list_swaps`.
- Add the minimum provider RPC surface needed for Loop Out quote and acceptance.
- Integrate provider payout-lock construction and confirmation watching.
- Integrate client Fiber payment and client on-chain claim.
- Support CKB and whitelisted UDT assets.
- Persist every order before external side effects and resume non-terminal Loop
  Out orders after restart.

## Non-Goals

- Do not implement Loop In; that is M4.
- Do not add Fiber p2p messages, provider gossip, or provider discovery.
- Do not implement automated liquidity planning.
- Do not implement a multi-provider market.
- Do not expose provider mode as enabled by default.

## Recommended Approach

Use an end-to-end liquidity actor boundary.

RPC handlers should stay thin: parse JSON-RPC parameters, apply permissions, and
call the `LiquidityActor`. The actor owns Loop Out orchestration, state
transitions, persistence, payment calls, CKB calls, and recovery. This follows the
existing FNN pattern of keeping long-running workflows out of RPC handlers and
makes restart behavior testable in one place.

## Architecture

M3 adds a `LiquidityActor` runtime component in
`crates/fiber-lib/src/liquidity/`. The actor exposes one clear command boundary
for RPC methods and recovery startup.

The component depends on:

- `LiquidityStore` for durable orders and provider assets.
- Existing payment APIs for client Fiber payment execution and payment status
  rehydration.
- Existing CKB actor/client APIs for payout-lock broadcast, claim broadcast, and
  transaction confirmation watching.
- M1 transaction builders for liquidity-lock script args and output construction.

RPC modules should expose methods but not directly mutate swap state. All state
transitions go through the liquidity component and M2 store APIs.

## RPC Surface

### Client RPCs

`quote_loop_out` requests a provider quote for a target on-chain amount. Request
fields match the M0 contract: `provider`, `asset_id`, `amount`, `receiver`,
`max_provider_fee`, `max_routing_fee`, and `expires_after_seconds`.

`loop_out` accepts a provider quote. It persists the client-side order, starts
watching the provider payout lock, sends the Fiber payment after payout
confirmation, and claims the payout after payment settlement reveals the
preimage.

`get_swap` and `list_swaps` read M2 persisted orders. They do not trigger side
effects.

### Provider RPCs

M3 requires a provider-side counterpart so a client FNN can coordinate with a
provider FNN without Fiber p2p changes. The provider surface includes:

- `provider_quote_loop_out`: validate asset, amount, fee caps, and capacity;
  generate preimage/payment hash; reserve capacity until quote expiry; return
  quote terms.
- `provider_accept_loop_out`: accept a quote, persist the provider order, build
  and broadcast the payout lock, and return payout lock tracking data.

These methods are provider RPC methods with the names above in M3. They use
normal RPC transport so two FNN nodes can interoperate.

### Permissions

- Client quote/history reads require `read("liquidity")`.
- Client execution requires `write("liquidity")`.
- Provider quote/accept and asset administration require
  `write("liquidity_provider")` where mutation or capacity reservation occurs.

## Data Model

`LiquiditySwapRecord` remains the durable order record. M3 does not introduce a
parallel order model.

Loop Out runtime data must be derivable from persisted fields plus quote terms.
The persisted order must include enough information to resume:

- `quote_id`
- local `role`
- `swap_kind = LoopOut`
- `asset_id`
- quoted amount
- `payment_hash`
- `payment_preimage` once known
- payout-lock or claim `onchain_outpoint` once known
- `payout_deadline`
- `refund_after_lock_time`
- `expires_at`
- timestamps and failure reason

If implementation needs extra Loop Out fields that are not present in M2, add a
focused persisted extension rather than storing opaque JSON blobs.

## State Flow

### Client Role

The client flow is:

1. `Created`: local order record exists before side effects.
2. `Quoted`: provider quote accepted and persisted.
3. `PayoutPending`: client is waiting for provider payout-lock broadcast or
   confirmation.
4. `PayoutLocked`: payout lock confirmed under the quote confirmation policy.
5. `PaymentInFlight`: client Fiber payment has been sent.
6. `PaymentSettled`: payment settled and preimage is persisted.
7. `ClaimPending`: client claim transaction is broadcast.
8. `Success`: claim transaction confirmed.

The client must not send the Fiber payment before the payout lock is confirmed.
The client must not mark success before claim confirmation.

### Provider Role

The provider flow is:

1. `Created`: quote/order record exists before side effects.
2. `Quoted`: capacity is reserved for an unexpired quote.
3. `PayoutPending`: provider payout-lock transaction is broadcast.
4. `PayoutLocked`: payout lock confirmed.
5. `PaymentInFlight`: provider is waiting for or observing the incoming Fiber
   payment.
6. `PaymentSettled`: payment settled or settlement evidence is known.
7. `Success`: provider observes the client claim or otherwise confirms the swap
   cannot require provider refund.

If the Fiber payment does not settle, the provider keeps the order non-terminal
until `refund_after_lock_time` permits refund. The refund path is
`PayoutLocked -> RefundPending -> Refunded`.

## Quote Validation

Provider quote validation must enforce:

- `asset_id` exists and is enabled.
- `amount` is between `min_amount` and `max_amount`.
- available capacity covers the quoted amount and provider fee semantics.
- `amount + provider_fee + routing_fee_limit` fits in `u128` for Loop Out Fiber
  payment budgeting.
- quote expiry is in the future.
- UDT quotes use exactly the registry UDT type script.
- provider reserves quoted capacity until expiry or terminal state.

The provider generates the 32-byte preimage and `payment_hash` for Loop Out. The
client treats the hash as quote-bound and uses it for the Fiber payment and claim
path.

## Chain Integration

The provider builds a liquidity-lock payout cell with:

- `payment_hash` from the quote preimage.
- client receiver/claimant lock hash.
- provider refund lock hash.
- `refund_after_lock_time`.
- quote asset amount and asset type hash.

For CKB, the payout output capacity must cover the quoted amount and any required
operational capacity. Chain fees remain separate from the swapped amount.

For UDT, the payout output must carry the exact whitelisted UDT type script and
raw token amount. Cell capacity is operational capacity, not swapped value.

The client claim transaction spends the payout lock with the 32-byte preimage
revealed by Fiber payment settlement. Claim construction must preserve CKB/UDT
asset terms enforced by the `liquidity-lock` script.

## Payment Integration

The client pays the provider through Fiber using the quote `payment_hash`.

For Loop Out, the gross Fiber payment budget is:

`amount + provider_fee + actual_routing_fee`, capped by `routing_fee_limit`.

The client persists `PaymentInFlight` before sending payment. On successful
settlement, it persists `payment_preimage` before building or broadcasting the
claim transaction. If payment fails before settlement while no payout lock is
confirmed, the order transitions to `Failed`. If a payout lock is confirmed or
claim/refund tracking is still required, the order remains non-terminal and
recovery continues.

## Recovery

On startup, the liquidity component scans non-terminal Loop Out orders from
`LiquidityStore`.

Recovery actions:

- `PayoutPending`: resume provider payout-lock broadcast/confirmation watching.
- `PayoutLocked`: resume client payment eligibility or provider payment watching.
- `PaymentInFlight`: reload payment status before retrying or transitioning.
- `PaymentSettled`: resume client claim construction/broadcast.
- `ClaimPending`: resume claim confirmation watching.
- Provider unpaid payout locks: wait until `refund_after_lock_time`, then build,
  broadcast, and confirm the provider refund transaction.

Recovery must be idempotent. If a transaction was already broadcast, recovery
must detect it by outpoint/hash and continue watching instead of creating a
second economic claim.

## Error Handling

- Expired quotes are rejected before order creation.
- Disabled or missing assets reject provider quotes.
- Fee cap violations reject quote acceptance or payment execution before side
  effects.
- Missing payout confirmation before `payout_deadline` leaves the order
  non-terminal for recovery; it does not become success or failed automatically.
- Wrong or missing preimage prevents claim and keeps the order non-terminal until
  recovery or operator intervention.
- Store errors stop orchestration and surface through RPC without hiding partial
  state.

## Testing

Required tests:

- Quote validation success for CKB and UDT assets.
- Quote validation rejects disabled/missing asset, out-of-range amount, expired
  quote, fee overflow, and UDT type mismatch.
- Client state machine refuses to pay before payout confirmation.
- Client persists `PaymentInFlight` before sending payment.
- Client persists `payment_preimage` before claim broadcast.
- Provider builds payout lock with claimant/refund identities and exact asset
  terms.
- Loop Out happy path covers quote, accept, payout lock, Fiber payment, claim, and
  success transition.
- Payment failure before settlement transitions to `Failed` only when no payout
  lock is confirmed; confirmed payout locks remain non-terminal for recovery.
- Restart recovery resumes `PayoutPending`, `PayoutLocked`, `PaymentInFlight`,
  `PaymentSettled`, and `ClaimPending` without duplicate economic actions.
- `get_swap` and `list_swaps` return persisted M2 records.

Integration tests use existing actor/test utilities with deterministic chain and
payment adapters. At least one end-to-end test must exercise the real
`LiquidityActor` boundary and M2 persistence.

## Completion Criteria

- Two FNN nodes can complete a manual Loop Out using RPC coordination.
- Client does not pay before confirmed payout lock.
- Provider payout lock uses the M1 liquidity-lock script and supports CKB/UDT
  assets.
- Client claim uses the payment-settlement preimage.
- M2 store records are updated through the specified state flow.
- Non-terminal Loop Out orders are discoverable and resumable after restart.
- No Fiber p2p messages are added.
