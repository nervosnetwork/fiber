# Fiber Liquidity CKB Loop Out Hardening Design

## Status

Approved design addressing final-review findings in the cross-node CKB Loop Out
flow.

## Context

The public-RPC CKB Loop Out happy path and provider restart path pass, but final
review identified five correctness gaps:

1. the client confirms only the provider-supplied transaction hash before paying;
2. the routing fee limit is transferred as principal instead of remaining a cap;
3. execution-time fee caps are ignored;
4. `InvoiceAlreadyExists` is accepted without validating equivalence;
5. outpoint tracer registration conflicts are reported asynchronously after the
   liquidity actor has marked the watcher active.

UDT payment propagation is a separate, already-approved item in the original
in-process E2E Task 6 and remains out of scope for this CKB hardening phase.

## Goals

- Never send the Fiber payment until the exact payout cell is confirmed and
  validated against the imported quote.
- Charge provider principal and routing fees according to the approved M3 fee
  model.
- Enforce execution-time fee caps before persistence or external side effects.
- Accept existing provider invoices only when their observable terms are
  equivalent.
- Mark a provider claim watcher active only after generic tracer registration is
  acknowledged.

## Non-Goals

- Do not add UDT payment propagation or UDT happy-path E2E here.
- Do not change the liquidity-lock contract.
- Do not redesign quote transport or add provider-supplied chain proofs.
- Do not add refund terminal-state policy.

## 1. Payout Cell Validation

The payout outpoint returned by `provider_accept_loop_out` is untrusted input.
The client persists it for recovery, but transaction confirmation alone is not
payment authorization.

After the payout transaction reaches the configured confirmation depth, the
client chain watcher fetches the exact live cell at the persisted outpoint. It
validates before sending `PayoutConfirmed`:

- the output index exists and the cell is live;
- lock code hash and hash type match the configured liquidity-lock artifact;
- lock args are exactly 152 bytes;
- payment hash matches the imported quote;
- claimant and refund lock hashes match the quote scripts;
- refund lock time matches the quote;
- encoded asset type hash matches CKB or the exact UDT script;
- encoded on-chain amount matches the direction-specific amount;
- output capacity satisfies both the asset amount and capacity requirement;
- CKB outputs have no type script;
- UDT outputs have the exact type script and 16-byte little-endian amount data.

Direction-specific amount rules:

- Loop Out payout: `quote.amount`;
- Loop In client lock: `quote.amount + quote.provider_fee`.

Refactor the existing Loop In observed-lock validation into shared pure/live-cell
validation with an explicit expected amount. Loop In behavior remains unchanged.

A committed but missing or invalid payout must never trigger the Fiber payment.
The actor records a descriptive payout-validation failure while preserving the
fact that the transaction itself committed. It must not label the committed
transaction as CKB-rejected.

## 2. Payment Principal and Routing Budget

The approved M3 model is:

```text
provider principal = amount + provider_fee
sender total cost  = provider principal + actual_routing_fee
actual_routing_fee <= routing_fee_limit
```

`LoopOutPaymentRequest.amount` becomes provider principal only. The existing
`max_fee_amount` remains the routing fee cap passed to `SendPaymentCommand`.

The provider invoice amount is the same principal. It must not include
`routing_fee_limit`.

Keep a checked budget helper for overflow validation:

```text
amount + provider_fee + routing_fee_limit
```

but do not use that budget as payment principal or invoice amount.

For the current direct-channel E2E quote (`1000 + 1`, routing cap `100`), the
settled channel delta is `1001`, not `1101`.

## 3. Execution-Time Fee Caps

Before `create_client_loop_out`, outpoint persistence, chain transaction record
insertion, or watcher registration, `handle_loop_out` validates:

- `quote.provider_fee <= params.max_provider_fee`;
- `quote.routing_fee_limit <= params.max_routing_fee`.

Cap violations return the existing structured provider-fee/routing-fee errors.
No swap, transaction identity, watcher, invoice, or payment side effect occurs.

Import-time caps remain independently enforced. Execution-time caps protect an
operator that intentionally tightens policy after import.

## 4. Existing Invoice Equivalence

Provider invoice registration remains idempotent only for equivalent terms.

When `AddInvoice` returns `InvoiceAlreadyExists`, the payment adapter calls
`GetInvoice(payment_hash)` and validates:

- payment hash;
- amount;
- node currency;
- exact optional UDT type script;
- hash algorithm (`CkbHash` unless explicitly specified identically);
- compatible status.

Compatible statuses are `Open`, `Received`, and `Paid`. `Cancelled`, `Expired`,
missing, or metadata mismatch returns a descriptive conflict error.

The requested preimage is independently validated to hash to `payment_hash`
before registration. Because the network API does not expose stored preimages,
matching payment hash is the cryptographic equivalence check for the preimage.

Invoice timestamps, signatures, and unrelated descriptive attributes are not
part of equivalence.

## 5. Tracer Registration Acknowledgement

Separate registration success from eventual spend observation.

`CkbOutPointSpendTracer` gains a one-shot registration acknowledgement:

```rust
pub registration: RpcReplyPort<Result<(), String>>
```

The production and mock tracing actors send:

- `Ok(())` only after the tracer group/callback is installed or an identical
  registration joins it;
- `Err(reason)` for conflicting lock script or confirmation metadata.

`CkbLiquidityChainWatcher::watch_provider_claim` sends the chain message and
awaits acknowledgement before returning. `LiquidityActorState` inserts the swap
into `watched_claim_swaps` only after that successful return.

Spend discovery remains a separate callback. Retryable indexer errors stay
inside the tracer and do not fail registration.

Mock registration preserves history-first and subscribe/store-then-recheck race
semantics. A historical spend may be delivered immediately after registration,
but acknowledgement must still be resolved exactly once.

## Error Handling

- Invalid payout cell: persist validation failure; do not pay.
- Execution cap violation: return before side effects.
- Conflicting invoice: return payment-registration conflict; do not treat as
  healed idempotence.
- Tracer metadata conflict: fail watcher registration; do not set in-memory
  deduplication guard.
- Retryable chain/indexer error after successful registration: retain tracer and
  retry.

## Testing

### Payout validation

- valid CKB payout confirms and authorizes payment;
- committed transaction with missing output index does not pay;
- wrong lock contract, payment hash, claimant/refund hashes, refund time, amount,
  capacity, or unexpected type script does not pay;
- UDT validation helpers retain exact type/data checks for later Task 6;
- restart revalidates before payment if confirmation occurred during downtime.

### Fees and caps

- direct payment principal is `amount + provider_fee`;
- routing limit is only `max_fee_amount`;
- provider invoice amount matches principal;
- checked total budget overflow remains rejected;
- lower execution provider/routing caps reject before store/chain/payment events.

### Invoice idempotence

- identical Open, Received, and Paid invoices succeed;
- amount, currency, UDT script, hash algorithm, or terminal-status conflict fails.

### Registration acknowledgement

- identical registration acknowledges success and both callbacks remain active;
- conflict acknowledges error and original remains active;
- mock history/race paths acknowledge exactly once;
- liquidity watcher guard is not set when registration fails.

### Integration

Update both CKB Loop Out E2Es to assert a direct-channel balance delta of
`amount + provider_fee`. Add an adversarial committed payout transaction with an
invalid/missing quoted output and assert no offered TLC, no payment record, and
no balance movement.

## Completion Criteria

- Malicious provider outpoint cannot trigger payment without exact live-cell
  validation.
- CKB direct-channel payment transfers principal `amount + provider_fee`.
- Execution cap reductions fail before side effects.
- Conflicting existing invoice terms fail closed.
- Claim watcher guard is set only after acknowledged tracer registration.
- Existing CKB happy/restart E2Es and full liquidity checks pass.
- UDT propagation remains explicitly tracked by original Task 6.
