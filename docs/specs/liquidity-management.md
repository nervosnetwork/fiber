# Fiber Liquidity Management Protocol

## Status

Draft for M0 protocol review.

## Scope

This protocol defines Loop In and Loop Out style liquidity swaps between Fiber
channels and CKB on-chain assets. It supports CKB and provider-whitelisted UDTs.
Swap negotiation uses RPC in the first product milestones. Fiber p2p messages are
not changed by this protocol version.

## Roles

- Client FNN: requests a liquidity swap.
- Provider FNN: optionally enabled role that quotes and fulfills swaps.
- Fiber network: routes ordinary payments.
- CKB chain: hosts on-chain swap cells for CKB or UDT assets.

## Asset Registry

Providers maintain a local asset registry. A swap request must reference an
asset by `asset_id`; the client must not provide an arbitrary UDT type script in
the execution request.

Asset entries have these fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `asset_id` | string | Stable provider-local identifier used by quote and swap RPCs. |
| `kind` | `ckb` or `udt` | Asset family. |
| `udt_type_script` | nullable CKB script | Required when `kind = udt`; absent when `kind = ckb`. |
| `min_amount` | u128 decimal string | Smallest raw swap amount accepted by the provider. |
| `max_amount` | u128 decimal string | Largest raw swap amount accepted by the provider. |
| `available_capacity` | u128 decimal string | Provider-advertised capacity for this asset. |
| `base_fee` | u128 decimal string | Fixed provider fee charged in the swapped asset. |
| `proportional_fee_ppm` | u64 | Proportional provider fee in parts per million. |
| `enabled` | bool | Whether the provider currently quotes this asset. |

CKB amounts are raw shannons. UDT amounts are raw token units. Decimals and
display symbols are wallet/UI metadata and are not interpreted by this protocol.

For UDT swaps, CKB is still required for cell capacity and transaction fees. A
provider may reject a UDT quote if the client cannot satisfy the CKB capacity and
fee requirements.

## Quote Model

A quote is a provider commitment to execute a swap under fixed terms until
`expires_at`.

Common quote fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `quote_id` | Hash256 hex | Provider-generated identifier. |
| `swap_kind` | `loop_in` or `loop_out` | Direction requested by the client. |
| `asset_id` | string | Asset registry identifier. |
| `amount` | u128 decimal string | Raw amount the client wants to swap before provider fee. |
| `provider_fee` | u128 decimal string | Fee charged in the swapped asset. |
| `routing_fee_limit` | u128 decimal string | Maximum Fiber routing fee in the swapped asset. |
| `onchain_fee_estimate_ckb` | u64 decimal string | Estimated CKB transaction fee. |
| `capacity_requirement_ckb` | u64 decimal string | CKB capacity required for CKB or UDT cells. |
| `payment_hash` | Hash256 hex | Hash used by the Fiber payment and on-chain claim path. |
| `expires_at` | u64 milliseconds | Quote expiry time. |
| `refund_after_lock_time` | u64 | Chain lock time after which refund is valid. |

Quote validation rules:

- `asset_id` must exist and be enabled in the provider registry.
- `amount` must be between `min_amount` and `max_amount`.
- `amount + provider_fee + routing_fee_limit` must fit in u128.
- `expires_at` must be later than the current node time.
- For UDT assets, the quote must include the registry's exact `udt_type_script`.
- The provider must reserve quoted capacity until the quote expires or the swap
  reaches a terminal state.

## Loop Out Protocol

Loop Out moves Fiber channel balance to an on-chain CKB address or UDT receiver.

Sequence:

```mermaid
sequenceDiagram
    participant C as Client FNN
    participant P as Provider FNN
    participant F as Fiber Network
    participant L as CKB Chain

    C->>P: quote_loop_out(asset_id, amount, receiver)
    P-->>C: quote(payment_hash, fees, expiry, payout terms)
    C->>P: loop_out(quote_id)
    C->>F: pay provider invoice/payment_hash
    F-->>P: payment settled, preimage available to provider-side invoice logic
    P->>L: create payout transaction to client receiver
    L-->>P: payout confirmed
    P-->>C: swap settled
```

Safety rules:

- The provider must not broadcast the on-chain payout before the Fiber payment is
  settled or otherwise irreversibly claimable under the agreed hashlock flow.
- The client must not treat Loop Out as settled until the payout transaction is
  confirmed under the quote's confirmation policy.
- If the Fiber payment fails before settlement, both sides mark the swap failed
  and release reserved capacity.
- If the provider payment path settles but the payout transaction is not
  confirmed before the payout deadline, recovery must continue after restart and
  surface the order as non-terminal.

## Loop In Protocol

Loop In moves on-chain CKB or UDT into Fiber channel balance.

Sequence:

```mermaid
sequenceDiagram
    participant C as Client FNN
    participant P as Provider FNN
    participant F as Fiber Network
    participant L as CKB Chain

    C->>P: quote_loop_in(asset_id, amount, client invoice)
    P-->>C: quote(payment_hash, fees, lock terms)
    C->>L: lock CKB/UDT in swap cell
    L-->>P: swap cell confirmed
    P->>F: pay client invoice/payment_hash
    F-->>C: payment settled, preimage revealed
    P->>L: claim swap cell with preimage
    L-->>P: claim confirmed
```

Safety rules:

- The provider must not send the Fiber payment until the on-chain swap cell is
  confirmed under the quote's confirmation policy.
- The client must be able to refund the on-chain swap cell after
  `refund_after_lock_time` if the Fiber payment does not settle.
- The provider must only claim with a preimage whose hash matches `payment_hash`.
- Both sides must persist the swap before broadcasting any Fiber payment or CKB
  transaction.

## On-Chain Swap Cell Requirements

The M1 implementation must choose a concrete CKB script design that satisfies
the requirements in this section. The CKB contract/script implementation lives
in the sibling `../fiber-scripts` repository. This repo consumes that script via
Fiber-side types, config, transaction builders, chain watchers, and integration
tests.

Common requirements:

- Claim path: spender provides a preimage whose hash equals `payment_hash`.
- Refund path: original funder can spend after `refund_after_lock_time`.
- The lock must bind the intended claimant and refund identity.
- The transaction builder must reject cells whose asset does not match the quote.
- The watcher must detect lock, claim, refund, and expiry-relevant blocks.

CKB swap requirements:

- The swapped value is represented by cell capacity in shannons.
- The cell must reserve enough capacity to remain valid after claim or refund.
- The claim and refund builders must keep fee calculation separate from the raw
  swapped amount reported in the quote.

UDT swap requirements:

- The cell must use the exact `udt_type_script` from the provider asset registry.
- The UDT amount in cell data must equal the quote's raw amount after applying
  the direction-specific fee rule.
- The CKB capacity in the UDT cell is operational capacity, not swapped value.
- Claim and refund outputs must preserve the UDT type script and amount.

M1 test vectors must cover:

- Correct preimage claim succeeds.
- Wrong preimage claim fails.
- Early refund fails.
- Refund after lock time succeeds.
- UDT type script mismatch is rejected.
- UDT amount mismatch is rejected.
- CKB amount below quote is rejected.

## Swap Order State Machine

Shared states:

| State | Meaning |
| --- | --- |
| `Created` | Local order record exists before external side effects. |
| `Quoted` | Provider quote is accepted and capacity is reserved. |
| `OnchainLockPending` | A required on-chain lock or payout transaction is broadcast but not confirmed. |
| `OnchainLocked` | Required on-chain lock is confirmed. |
| `PaymentInFlight` | Fiber payment has been sent and is waiting for result. |
| `PaymentSettled` | Fiber payment settled and a valid preimage is known where required. |
| `ClaimPending` | Claim transaction is broadcast but not confirmed. |
| `RefundPending` | Refund transaction is broadcast but not confirmed. |
| `Success` | Swap completed successfully. |
| `Failed` | Swap failed before funds were locked in a way that requires refund. |
| `Refunded` | Swap failed and locked funds were returned through refund. |

Terminal states are `Success`, `Failed`, and `Refunded`.

Allowed transitions:

```mermaid
stateDiagram-v2
    [*] --> Created
    Created --> Quoted
    Quoted --> OnchainLockPending
    Quoted --> PaymentInFlight
    OnchainLockPending --> OnchainLocked
    OnchainLocked --> PaymentInFlight
    PaymentInFlight --> PaymentSettled
    PaymentInFlight --> Failed
    PaymentSettled --> ClaimPending
    PaymentSettled --> Success
    ClaimPending --> Success
    OnchainLockPending --> RefundPending
    OnchainLocked --> RefundPending
    PaymentInFlight --> RefundPending
    RefundPending --> Refunded
```

Every transition must record `updated_at` and an event reason. Invalid backward
transitions must be rejected during normal execution and recovery.

## Persistence And Recovery

Before any external side effect, the node must persist the order state that
explains the next side effect.

Persisted fields:

| Field | Meaning |
| --- | --- |
| `swap_id` | Local unique identifier. |
| `quote_id` | Provider quote identifier. |
| `role` | `client` or `provider`. |
| `swap_kind` | `loop_in` or `loop_out`. |
| `asset_id` | Asset registry identifier. |
| `amount` | Raw swapped asset amount. |
| `payment_hash` | Hash used by Fiber payment and claim path. |
| `payment_preimage` | Known after settlement or claim observation. |
| `state` | Current state-machine state. |
| `onchain_outpoint` | Swap, payout, claim, or refund outpoint when known. |
| `refund_after_lock_time` | Lock time that enables refund. |
| `expires_at` | Quote or order expiry. |
| `created_at` | Creation timestamp in milliseconds. |
| `updated_at` | Last state change timestamp in milliseconds. |
| `failure_reason` | Human-readable terminal failure reason. |

Startup recovery rules:

- Terminal orders are not retried.
- `OnchainLockPending`, `ClaimPending`, and `RefundPending` resume chain watching.
- `OnchainLocked` resumes the next Fiber payment or refund eligibility check.
- `PaymentInFlight` reloads payment status from the payment store before retrying.
- Refund is attempted only after `refund_after_lock_time` is reached.
- Recovery must be idempotent when the same transaction was already broadcast.

## RPC Contract

Initial client RPCs:

### `quote_loop_out`

Request fields: `provider`, `asset_id`, `amount`, `receiver`, `max_provider_fee`,
`max_routing_fee`, `expires_after_seconds`.

Response fields: quote fields from the Quote Model plus provider routing details.

### `loop_out`

Request fields: `quote_id`, `max_provider_fee`, `max_routing_fee`.

Response fields: `swap_id`, `state`, `payment_hash`, `created_at`.

### `quote_loop_in`

Request fields: `provider`, `asset_id`, `amount`, `client_invoice`,
`max_provider_fee`, `max_routing_fee`, `expires_after_seconds`.

Response fields: quote fields from the Quote Model plus on-chain lock terms.

### `loop_in`

Request fields: `quote_id`, `funding_tx` or wallet funding parameters.

Response fields: `swap_id`, `state`, `payment_hash`, `created_at`.

### `get_swap`

Request fields: `swap_id`.

Response fields: persisted swap order fields.

### `list_swaps`

Request fields: optional `state`, optional `asset_id`, optional `limit`, optional
`cursor`.

Response fields: `swaps`, optional `next_cursor`.

Provider administration RPCs:

- `list_liquidity_assets`
- `add_liquidity_asset`
- `update_liquidity_asset`
- `disable_liquidity_asset`
- `get_liquidity_provider_status`

Biscuit permissions:

- Swap history reads require `read("liquidity")`.
- Client swap execution requires `write("liquidity")`.
- Provider asset and risk configuration requires `write("liquidity_provider")`.

## P2P Compatibility

This protocol version does not add Fiber p2p messages. Quote negotiation uses
RPC between client and provider FNNs. Fiber p2p only sees ordinary payments and
TLC forwarding.

Future multi-provider market work may add provider discovery, quote gossip,
capacity advertisement, liquidity feature bits, and anti-spam rules. Those
messages are outside M0-M8 and must not block the first manual Loop Out product.

## Security Invariants

- A provider cannot claim a client's on-chain Loop In funds without revealing the
  preimage that settles the client's Fiber invoice.
- A client cannot receive a Loop In Fiber payment and prevent a provider from
  claiming the corresponding on-chain swap cell before refund expiry.
- A provider cannot complete Loop Out by receiving Fiber funds without either
  completing the on-chain payout or leaving a recoverable non-terminal order.
- A client cannot execute a swap for a UDT outside the provider whitelist.
- Chain fees and CKB capacity are accounted separately from the swapped asset.
- Restart recovery never creates a second economic claim for the same order.
- Fiber p2p routers do not need to understand liquidity swaps.

## M1 Implementation Inputs

M1 may start when this spec has maintainer approval for:

- Asset registry field names and validation rules.
- Quote field names and fee semantics.
- Loop In and Loop Out sequence diagrams.
- On-chain swap cell requirements.
- State names and allowed transitions.
- Persistence fields and recovery rules.
- RPC method names and permission categories.
- No-p2p-change boundary for M0-M8.
- Cross-repo boundary: CKB scripts in `../fiber-scripts`, Fiber integration in
  this repository.

The M1 plan should be split into two coordinated workstreams. In `../fiber-scripts`,
it should add the CKB script contract and script-level tests. In this repository,
it should create Rust types in `crates/fiber-types/src/liquidity.rs`, JSON-RPC
types in `crates/fiber-json-types/src/liquidity.rs`, a new
`crates/fiber-lib/src/liquidity/` module, storage traits under the same module,
transaction builders that consume the script artifacts, and integration tests.
M1 must start with tests for asset validation, state transitions, and script test
vectors before adding transaction builders.
