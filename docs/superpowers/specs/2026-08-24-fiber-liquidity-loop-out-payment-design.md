# Fiber Liquidity Cross-Node Loop Out Payment Design

## Status

Approved design closing the cross-node Loop Out payment gaps discovered while
implementing the in-process E2E happy path (plan
`2026-08-12-fiber-liquidity-in-process-e2e.md`, Task 4).

## Context

The cross-node negotiation phase delivered quote envelopes, import,
idempotent provider acceptance, provider mode, live-cell data, and durable
signed transactions, but the actual cross-node Loop Out payment path was never
wired end to end. The RED E2E test exposed three gaps, all confirmed against the
approved M3 design (`2026-07-13-fiber-liquidity-m3-loop-out-design.md`):

1. The client never learns the payout transaction identity, so it cannot watch
   the payout lock. `provider_accept_loop_out` returns only `LiquiditySwapResponse`
   (no outpoint), but the client's `loop_out` → `watch_payout_lock` requires the
   payout outpoint. M3 requires the provider accept to "return payout lock
   tracking data".

2. The provider never generates a real preimage. `provider_quote_loop_out` sets
   `payment_hash = loop_out_quote_hash(...)`, a deterministic hash of quote
   params, instead of `H(preimage)`. The claim path
   (`LoopOutClaimPlan::validate_payment_preimage`) requires
   `H(preimage) == payment_hash`, so a real claim is impossible. M3 requires "the
   provider generates the 32-byte preimage and `payment_hash`".

3. The provider has no incoming-payment observation, so it stalls in
   `PayoutLocked` forever and never reaches `PaymentSettled` / observes the
   client claim to reach `Success`.

The fix is the mirror image of the existing Loop In client-invoice pattern: the
provider generates a preimage and registers an invoice; the client pays it and
learns the preimage; the client claims the payout with the preimage.

## Goals

- Make the cross-node CKB and UDT Loop Out happy path reach `Success` on both
  nodes through public RPC only.
- Preserve the existing invariant: the client must not send the Fiber payment
  before the payout lock is confirmed, and must not mark success before claim
  confirmation.
- Preserve the preimage-secrecy invariant: the provider holds the preimage and
  reveals it only through payment settlement; the client can only claim after
  paying.
- Keep Loop In behavior unchanged.

## Non-Goals

- No change to the liquidity-lock contract or `fiber-scripts`.
- No p2p message additions; coordination remains over public RPC.
- No provider discovery, reputation, or multi-provider market work.
- No change to Loop In quote/accept/execute semantics.

## Design

### 1. Provider preimage and invoice registration

`provider_quote_loop_out` changes:

- Generate a fresh 32-byte preimage from the system CSPRNG.
- Set `payment_hash = blake2b_256(preimage)` (i.e. `HashAlgorithm::CkbHash.hash`),
  matching `LoopOutClaimPlan::validate_payment_preimage` and the pinned contract.
- Persist the preimage provider-side, associated with the quote id, so it
  survives restart and can settle an incoming payment during recovery.

The preimage is provider-secret and must never appear in the
`LiquidityQuoteEnvelope` returned to the client. `liquidity_quote_envelope_from_terms`
must not serialize it. The client's imported terms therefore carry no preimage
and learn it only from payment settlement.

`provider_accept_loop_out` changes:

- After reserving and broadcasting the payout lock, register the provider
  invoice + preimage with the network actor so the incoming Fiber payment is
  held and settled, revealing the preimage to the client.

The `LoopOutPaymentAdapter` trait gains a provider-side method (e.g.
`register_provider_invoice(payment_hash, preimage, amount)`), implemented by
`NetworkLoopOutPaymentAdapter` via `NetworkActorCommand::AddInvoice` (which
persists the invoice + preimage). The invoice must use the same `payment_hash`
and the quote amount (gross amount including provider fee, in the quoted asset
currency) so the incoming payment is recognized. Registration is idempotent:
re-registering the same hash/preimage/amount must not create duplicate state or
fail.

### 2. Payout outpoint propagation

- `LiquiditySwapResponse` gains `payout_outpoint: Option<OutPoint>`
  (serde-default, backward compatible).
- `handle_provider_accept_loop_out` populates it from the reserved
  `onchain_outpoint` (the outpoint produced by `reserve_payout_lock_outpoint`).
- `LoopOutParams` gains the matching outpoint field.
- `handle_loop_out` / `create_client_loop_out` set `onchain_outpoint` on the
  client swap record before `watch_payout_lock`, so the client can observe the
  payout confirmation through its own chain watcher.

### 3. Provider incoming-payment observation and state flow

After the provider payout is confirmed (`PayoutLocked`), the provider:

1. transitions to `PaymentInFlight` (waiting for the incoming payment);
2. polls the incoming payment status via `LoopOutPaymentAdapter::reload_loop_out_payment`
   (which reuses `GetPayment`) until `Settled`, then transitions to `PaymentSettled`;
3. continues watching for the client claim (existing `mark_provider_claim_observed`)
   and transitions to `Success`.

This mirrors the client Loop Out side, except the provider observes the incoming
direction rather than sending.

### 4. Quote id uniqueness

The quote id must remain unique per quote. Because the preimage is now
fresh-random, the quote id must not be a deterministic function of quote params
(it would collide on re-quote and violate import idempotency). Use a fresh
random quote id (or derive it from the preimage). Import idempotency stays:
"same quote id + identical terms → success; same quote id + different terms →
fail, never overwrite".

### 5. Data model

- Internal `LoopOutQuoteTerms` gains `payment_preimage: Option<Hash256>`
  (provider-local). It is omitted from the envelope.
- The internal swap record already anticipates `payment_preimage once known`
  (M3 design); the provider persists it so recovery can settle the incoming
  payment.

### 6. Recovery

- Provider swaps in `PayoutLocked` or `PaymentInFlight` resume watching the
  payout and observing the incoming payment on restart; the persisted preimage
  is re-registered with the network actor (idempotent) so the incoming payment
  can still settle.
- Client swaps in `PayoutPending` resume watching the payout from the persisted
  `onchain_outpoint`.

## State Flow (Provider Loop Out)

```text
Created -> Quoted -> PayoutPending -> PayoutLocked -> PaymentInFlight
    -> PaymentSettled -> (observe client claim) -> Success

PaymentInFlight -> RefundPending -> Refunded   (if payment never settles)
```

## Testing

- Unit tests for preimage generation and `payment_hash = blake2b_256(preimage)`.
- Unit tests for envelope serialization excluding the preimage.
- Unit tests for provider invoice registration idempotency and incoming-payment
  observation state transitions.
- The RED E2E test `liquidity_ckb_loop_out_e2e` turns GREEN: payout held pending
  → no payment, unchanged balances; commit → payment settlement, claim record via
  `list_liquidity_chain_transactions`, claim confirmation, `Success` on both nodes.
- Regression: existing Loop Out/Loop In actor, chain, quote, and RPC tests remain
  green; `make clippy`, `make check-migrate`, and RPC doc regeneration pass.

## Implementation Order

1. Preimage generation + payment hash + persistence + quote id uniqueness.
2. Provider invoice registration + incoming-payment observation + state flow.
3. Payout outpoint propagation (accept response + `LoopOutParams` + client watch).
4. Recovery wiring for provider incoming-payment and client payout watch.
5. Turn the E2E happy path GREEN, then run the full verification set.
