# Fiber Liquidity M2 Store And Recovery Design

## Status

Approved design for M2 implementation planning.

## Context

M0 defined the liquidity-management protocol in
`docs/specs/liquidity-management.md`. M1 added the `liquidity-lock` CKB script in
`../fiber-scripts` and introduced Fiber-side liquidity domain types, JSON-RPC
DTOs, storage traits, and transaction output builders.

M2 makes liquidity swaps durable before M3/M4 execute complete Loop Out and Loop
In products. The implementation should add real persistence in `Store`, validate
state transitions, and provide restart recovery queries. It should not execute
Loop In or Loop Out recovery side effects yet; those depend on product-specific
payment and chain flows in later milestones.

## Goals

- Implement durable `LiquidityStore` persistence for swaps and provider assets.
- Enforce the liquidity state machine during state updates.
- Maintain state and asset indexes for recovery and history queries.
- Support paginated swap history queries.
- Preserve recovery fields during partial updates.
- Add store tests that prove durability and query behavior.

## Non-Goals

- Do not implement Loop Out quote/execution RPCs.
- Do not implement Loop In quote/execution RPCs.
- Do not wire startup recovery into node startup yet.
- Do not broadcast claim, refund, payout, or payment actions from M2 recovery.
- Do not change Fiber p2p messages.

## Recommended Approach

Use a store-first vertical slice.

Implement the M1 `LiquidityStore` trait for `Store` using the existing store
patterns in `crates/fiber-lib/src/store/store_impl/mod.rs`. M2 owns durable data,
indexes, transition validation, pagination, and tests. M3/M4 will consume the
same APIs to perform concrete Loop Out and Loop In side effects.

This avoids designing recovery action execution before the product flows exist,
while still giving those later milestones a restart-safe foundation.

## Architecture

The domain boundary remains `crates/fiber-lib/src/liquidity/store.rs`.

`Store` should implement the trait by adding liquidity variants to the existing
`KeyValue` enum and by using the same serialization and prefix-iteration helpers
as invoices, payment sessions, watchtower data, and CCH orders.

M2 should keep recovery as durable query capability:

- Non-terminal swaps can be listed by state.
- Swap history can be listed by asset and cursor.
- Complete records can be reloaded after restart.
- Recovery workers in M3/M4 can decide which payment or chain action to resume.

No new actor is required in M2. Prefer storage APIs and tests over runtime
wiring.

## Data Model

### Swap Orders

Use `LiquiditySwapRecord` as the persisted order model. It includes:

- `swap_id`
- `quote_id`
- `role`
- `swap_kind`
- `asset_id`
- `state`
- `payment_hash`
- `payment_preimage`
- `amount`
- `onchain_outpoint`
- `payout_deadline`
- `refund_after_lock_time`
- `expires_at`
- `created_at`
- `updated_at`
- `failure_reason`

`insert_liquidity_swap` must persist a full record before any external side
effect. If a record with the same `swap_id` already exists, the implementation
must return `LiquidityStoreError::Backend` and leave the existing record and
indexes unchanged.

`update_liquidity_swap_state` must load the current record, reject invalid state
transitions with `LiquidityStoreError::InvalidStateTransition`, update `state`,
`updated_at`, and transition reason, and maintain the state index atomically.

`update_liquidity_swap` must patch recovery fields learned after creation:

- `payment_preimage`
- `onchain_outpoint`
- `failure_reason`
- `updated_at`

`None` means preserve the existing value, not clear it. M2 does not need a clear
operation for these fields.

### Provider Assets

Persist `LiquidityAsset` entries by `asset_id`.

`upsert_liquidity_asset` must call `LiquidityAsset::validate()` before writing.
Invalid CKB/UDT script combinations or invalid amount ranges must not be stored.

`get_liquidity_asset` returns `Ok(None)` when absent. `list_liquidity_assets`
returns all configured entries in deterministic key order.

## Store Keys And Indexes

Add dedicated prefixes in `store_impl` for:

- `LIQUIDITY_SWAP_PREFIX + swap_id`
- `LIQUIDITY_SWAP_STATE_PREFIX + state_key + swap_id`
- `LIQUIDITY_SWAP_ASSET_PREFIX + asset_id + swap_id`
- `LIQUIDITY_ASSET_PREFIX + asset_id`

The primary swap key stores the full `LiquiditySwapRecord`.

The state index stores a lightweight value or empty marker and is used for
recovery scans by state. When state changes, delete the old state-index key and
insert the new one in the same batch as the primary record update.

The asset index stores a lightweight value or empty marker and supports history
queries by asset. It is inserted with the swap and normally does not change
because `asset_id` is immutable for a swap.

Index values must not duplicate full records. Load primary records by `swap_id`
from index entries.

## Query Semantics

`get_liquidity_swap` loads by primary key.

`list_liquidity_swaps` supports:

- no filter: scan primary swap keys;
- state filter: scan the state index;
- asset filter: scan the asset index;
- both state and asset: scan the narrower or simpler index and filter loaded
  records in memory;
- `limit`: cap returned records;
- `cursor`: exclusive cursor for the selected scan.

The cursor must be deterministic and opaque to API callers. M2 will encode the
last scanned storage key as lowercase hex and tests must cover cursor round
trips.

Pagination must not skip or duplicate entries when the returned `next_cursor` is
used in the next call.

## Error Handling

- Missing swap during update returns `LiquidityStoreError::SwapNotFound`.
- Missing asset during get returns `Ok(None)`.
- Invalid transition returns `LiquidityStoreError::InvalidStateTransition`.
- Backend serialization or storage failures map to `LiquidityStoreError::Backend`.
- Asset validation failures return a dedicated `LiquidityStoreError` variant that
  preserves the validation reason.

## Testing

Add tests under the existing store test module.

Required tests:

- insert and get a liquidity swap;
- valid state transition updates the record and state index;
- invalid state transition is rejected and leaves the record unchanged;
- list non-terminal swaps by state for recovery;
- list swaps by asset;
- list swaps with both state and asset filters;
- pagination returns stable next cursors without duplicates;
- partial update with `None` preserves existing preimage, outpoint, and failure
  reason;
- provider asset validation rejects invalid UDT/CKB entries;
- provider asset upsert, get, and list succeed;
- durable reload can read previously inserted liquidity swaps and assets using
  the existing store test pattern.

During development, focused package tests may be used. M2 completion requires the
affected store test target, liquidity-focused tests, and formatting to pass.

## Completion Criteria

- `LiquidityStore` is implemented for `Store`.
- Swap and asset records are durable across store reload.
- State and asset indexes remain consistent after transitions.
- Invalid transitions and invalid assets are rejected.
- Pagination behavior is covered by tests.
- No runtime recovery side effects are implemented before M3/M4.
