# Fiber Liquidity M3 Runtime Wiring Design

## Status

Approved direction for implementation planning.

## Context

M3 Manual Loop Out currently has durable domain types, quote validation, storage-backed RPC reads, deterministic orchestration helpers, and tests for the safety-critical ordering. Mutation RPCs intentionally return unavailable errors because there is not yet a spawned `LiquidityActor`, real Fiber payment adapter, or real CKB chain watcher.

This follow-up finishes M3 runtime wiring before starting M4 Loop In. The goal is to turn the tested orchestration boundary into a usable runtime path while preserving the M3 safety invariants: persist before side effects, pay only after confirmed payout, persist preimage before claim, and mark success only after claim confirmation or provider claim observation.

## Goals

- Add a real `LiquidityActor` implemented with Ractor.
- Wire liquidity mutation RPCs to the actor instead of returning unavailable errors.
- Add a real payment adapter over existing Fiber network/payment actor messages.
- Add a liquidity-specific chain watcher abstraction over existing CKB actor and transaction tracing.
- Support real manual Loop Out between a client FNN and provider FNN through RPC coordination.
- Resume non-terminal Loop Out orders after restart without duplicating economic actions.

## Non-Goals

- Do not implement Loop In; that remains M4.
- Do not add Fiber p2p messages, provider gossip, or provider discovery.
- Do not implement automated liquidity planning.
- Do not change the `liquidity-lock` script unless runtime integration exposes a concrete script bug.
- Do not expose fake success paths for mutation RPCs.

## Architecture

M3 runtime wiring adds one long-running actor boundary and two runtime adapters.

`LiquidityActor` owns mutation workflows and recovery. It receives typed commands from RPC handlers and startup wiring, reads/writes `LiquidityStore`, calls payment and chain adapters, and sends typed responses through `RpcReplyPort`s. Existing pure orchestration helpers remain useful, but actor handlers are the production entry points.

`LiquidityPaymentAdapter` wraps the existing network actor payment API. For Loop Out, it sends a payment using the quote `payment_hash`, gross payment budget, and routing fee cap, then rehydrates payment status when recovering `PaymentInFlight` orders. It must return the settled preimage before claim orchestration can continue.

`LiquidityChainWatcher` wraps existing CKB actor and transaction tracing APIs. It builds, broadcasts, and watches provider payout, client claim, and provider refund transactions. It persists the transaction identity before or atomically with broadcast intent, then resumes by watching known transactions instead of blindly rebroadcasting.

RPC handlers remain thin. They parse JSON-RPC parameters, call `LiquidityActor`, and return JSON DTOs. Read-only `get_swap` and `list_swaps` may continue reading `LiquidityStore` directly because they have no side effects.

## Actor Boundary

`LiquidityActorMessage` should become a runtime message enum with reply ports for mutation commands:

- `QuoteLoopOut(command, reply)` returns quote terms.
- `LoopOut(command, reply)` creates the client order and starts the Loop Out workflow.
- `ProviderQuoteLoopOut(command, reply)` validates provider asset capacity, reserves capacity, and returns quote terms.
- `ProviderAcceptLoopOut(command, reply)` persists provider order and starts payout lock broadcast/watch.
- `ResumeNonTerminal(reply)` scans store recovery queries and resumes all non-terminal Loop Out swaps.
- Internal continuation messages handle payout confirmed, payment settled, claim confirmed, refund eligible, and transaction watcher callbacks.

Actor state contains cloned store, network actor reference, CKB actor reference, chain watcher configuration, and provider runtime configuration. The actor must not store authoritative swap state only in memory; persisted `LiquiditySwapRecord` remains the source of truth.

## RPC Wiring

Mutation RPCs must call the actor:

- `quote_loop_out` calls the provider endpoint or actor path selected by the client request. For the first manual runtime path, direct provider RPC coordination is acceptable.
- `loop_out` calls the local actor to create the client order and start watching the provider payout.
- `provider_quote_loop_out` calls the local actor to validate assets and reserve quote capacity.
- `provider_accept_loop_out` calls the local actor to persist provider order and broadcast/watch payout.

Read RPCs remain store-backed:

- `get_swap`
- `list_swaps`

RPC methods must not mutate swap state directly. All mutation state transitions go through `LiquidityActor` and `LiquidityStore`.

## Payment Adapter

The payment adapter uses existing Fiber payment messages rather than introducing a second payment subsystem.

For Loop Out payment send:

- Build the payment request from persisted quote terms.
- Gross amount is `amount + provider_fee + routing_fee_limit` for budget construction.
- Set the payment hash to the provider-generated quote hash.
- Persist `PaymentInFlight` before sending.
- On success, persist the returned preimage before claim orchestration.
- On failure before a payout lock is confirmed, transition to `Failed` only when safe.
- On failure after payout lock confirmation, keep the swap non-terminal for recovery/refund logic.

For recovery:

- `PaymentInFlight` reloads payment status from the existing payment store/API.
- If settled, persist preimage and continue to claim.
- If still in flight, keep watching or schedule retry through existing payment semantics.
- If failed and payout is already locked, remain non-terminal until refund or operator intervention.

## Chain Watcher

`LiquidityChainWatcher` is a liquidity-specific abstraction over existing CKB actor and transaction tracing. It should expose operations in liquidity terms:

- `reserve_payout_lock_outpoint` or equivalent transaction identity planning.
- `broadcast_payout_lock`.
- `watch_payout_lock`.
- `broadcast_claim` with `LoopOutClaimRequest` containing persisted preimage.
- `watch_claim`.
- `broadcast_refund`.
- `watch_refund`.

The watcher must be idempotent. If a persisted record already has an outpoint or transaction identity, recovery watches that transaction. It must not create a second payout, claim, or refund for the same swap unless the previous transaction is known to have failed permanently and rebuilding is safe.

For CKB, payout capacity represents the swapped value. For UDT, the output carries the exact whitelisted type script and raw token amount; CKB capacity is operational capacity.

## Persistence And State Flow

The existing `LiquiditySwapRecord` remains the durable order model. If runtime integration requires additional transaction identity fields beyond `onchain_outpoint`, add focused fields rather than opaque JSON blobs.

Client Loop Out runtime flow:

1. Persist `Created` and accepted quote fields.
2. Persist `PayoutPending` before watching provider payout.
3. Persist `PayoutLocked` after payout confirmation.
4. Validate payment request, persist `PaymentInFlight`, then send Fiber payment.
5. Persist `payment_preimage`, transition to `PaymentSettled`.
6. Persist `ClaimPending`, broadcast deterministic claim using persisted preimage.
7. Watch claim confirmation.
8. Transition `ClaimPending -> Success` only after confirmation.

Provider Loop Out runtime flow:

1. Persist quote reservation and provider order before side effects.
2. Persist `PayoutPending` and payout transaction identity before broadcast intent.
3. Broadcast payout and watch confirmation.
4. Persist `PayoutLocked` after confirmation.
5. Observe incoming payment settlement or claim evidence.
6. Transition through `PaymentSettled -> ClaimPending -> Success` only after the client claim is observed or confirmed enough that provider refund is no longer required.
7. If payment does not settle, keep the order non-terminal until `refund_after_lock_time`, then refund and watch confirmation.

## Recovery

On startup, `LiquidityActor` scans non-terminal Loop Out orders and schedules recovery actions:

- `PayoutPending`: watch known payout transaction; if no transaction identity exists, rebuild only when safe.
- `PayoutLocked`: resume payment eligibility for client or payment watching for provider.
- `PaymentInFlight`: reload payment status.
- `PaymentSettled`: broadcast or resume claim using persisted preimage.
- `ClaimPending`: watch known claim transaction; rebroadcast only if the previous broadcast did not produce a transaction identity and deterministic rebuild is safe.
- `RefundPending`: watch or resume provider refund.

Recovery must be idempotent. Repeated actor restarts should not create duplicate economic actions.

## Error Handling

- Expired quotes reject before order creation.
- Missing/disabled assets reject provider quote before capacity reservation.
- Store errors stop orchestration and surface through RPC without hiding partial state.
- Payment construction errors happen before `PaymentInFlight`.
- Chain broadcast failures leave the persisted state recoverable.
- Missing preimage prevents claim broadcast.
- Confirmation timeouts keep swaps non-terminal unless a safe terminal path is known.

## Testing

Required tests:

- Actor mutation RPC calls delegate to `LiquidityActor` and do not mutate store directly.
- `LiquidityActor` persists state before every payment/chain side effect.
- Real payment adapter sends through existing network actor messages in deterministic tests.
- Payment recovery reloads settled preimage and resumes claim.
- Chain watcher persists transaction identity before broadcast intent.
- Payout confirmation gates client payment.
- Claim confirmation gates client success.
- Provider claim observation gates provider success.
- `ClaimPending` recovery watches or safely retries without duplicate economic action.
- Provider refund path after `refund_after_lock_time` is idempotent.
- Restart recovery resumes all M3 non-terminal states.
- RPC mutation methods no longer return unavailable errors once actor wiring is complete.

## Completion Criteria

- Two FNN nodes can execute manual Loop Out through RPC coordination in an integration test or deterministic runtime harness.
- Mutation RPCs delegate to `LiquidityActor` and no longer return the M3 placeholder unavailable errors.
- The actor uses real Fiber payment APIs and a real CKB watcher abstraction backed by existing actors.
- Success is never marked before claim confirmation or provider claim observation.
- Recovery is idempotent for payout, payment, claim, and refund states.
- Existing focused checks pass with `--features rocksdb`.
