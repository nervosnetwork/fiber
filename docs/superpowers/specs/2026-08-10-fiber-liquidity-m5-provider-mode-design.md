# Fiber Liquidity M5 Integrated Provider Mode Design

## Summary

M5 turns FNN into an optionally-enabled, safely-operable liquidity provider by adding:
native CKB observed-output validation, asset management RPCs, and a global provider
toggle. This slice intentionally prioritises the minimum needed for a real two-node
Loop In E2E test: provider must verify an external lock on-chain before paying.

The remaining M5 deliverables (concurrent order limits, risk budgets, metrics) are
deferred to a later M5 sub-slice.

## Goals

- Implement native `validate_observed_loop_in_lock` so `provider_accept_loop_in`
  is safe against unverified external lock txs.
- Expose `add`/`update`/`disable`/`list` asset management RPCs backed by durable
  store.
- Add a global `provider_mode` flag gating all provider-side quote/accept paths.
- Keep implementation consistent with existing CKB actor dispatch patterns.

## Non-Goals

- Do not add concurrent order limits, risk budgets, or metrics in this slice.
- Do not add Biscuit auth middleware for liquidity (deferred to M8).
- Do not change the liquidity-lock script or transaction builder layer.
- Do not wire automated planner or background capacity management.

## Architecture

```
┌─────────────────────┐
│  RPC (liquidity.rs) │── delegate ──▶ LiquidityActor
└──────────┬──────────┘                │
           │                           ├── quote_loop_in/out
           ▼                           ├── loop_in/out
   provider_accept_loop_in             ├── provider_accept_loop_out
   add/update/disable/list_asset       ├── provider_accept_loop_in  ← NEW
   get_provider_status                 ├── add/update/disable/list_asset  ← NEW
                                       ├── get_provider_status  ← NEW
                                       └── chain.validate_observed_loop_in_lock  ← FIXED
                                                    │
                                                    ▼
                                         CkbChainActor::GetLiveCell  ← NEW
                                                    │
                                                    ▼
                                              CKB RPC node
```

## 1. Native Observed-Output Validation

### 1.1 CkbChainActor: GetLiveCell

Add a new message to `CkbChainMessage`:

```rust
GetLiveCell(
    packed::OutPoint,
    RpcReplyPort<Result<Option<packed::CellOutput>, RpcError>>,
),
```

The handler queries the configured CKB RPC node for the cell at the given outpoint
and returns `None` if the cell is not found or already spent.

### 1.2 CkbLiquidityChainWatcher: validate_observed_loop_in_lock

Replace the current hardcoded `Err(...)` in `chain.rs:1144-1153` with a full
verification that calls `GetLiveCell` and then checks every field:

```
validate_observed_loop_in_lock(quote, outpoint):
  1. Call CkbChainActor::GetLiveCell(outpoint)
  2. Return error if cell not found or already spent
  3. Verify cell.lock.code_hash and hash_type match the liquidity-lock artifact
  4. Parse lock script args (152 bytes):
     [0..32)   payment_hash          → must equal quote.payment_hash
     [32..64)  claimant_lock_hash    → must equal blake2b_256(quote.claimant_lock)
     [64..96)  refund_lock_hash      → must equal blake2b_256(quote.refund_lock)
     [96..104) refund_after_lock_time→ must equal quote.refund_after_lock_time
     [104..120) amount               → must equal amount + provider_fee in LE u128
     [120..152) asset_type_hash      → [0;32] for CKB, type_hash(udt_script) for UDT
  5. If UDT asset, verify:
     a. cell.type_script matches quote.asset.udt_type_script
     b. cell output data encodes the same amount (u128 LE)
  6. Return Ok(()) on success, detailed field-specific error on mismatch
```

Error messages include the expected and actual values where useful, e.g.
`"observed lock payment_hash mismatch: expected {x}, got {y}"`.

### 1.3 Impact

After this change `provider_accept_loop_in` passes through to
`watch_loop_in_lock` → `handle_loop_in_lock_confirmed` →
`start_provider_loop_in_payment` → `claim_provider_loop_in`, completing the
provider Loop In E2E path.

## 2. Asset Management RPCs

### 2.1 Methods

All methods delegate to `LiquidityActor`. JSON types already exist in
`fiber-json-types/src/liquidity.rs`.

| RPC method | Input | Output |
|---|---|---|
| `add_liquidity_asset` | `AddLiquidityAssetParams` | `LiquidityAssetInfo` |
| `update_liquidity_asset` | `UpdateLiquidityAssetParams` | `LiquidityAssetInfo` |
| `disable_liquidity_asset` | `{ asset_id: String }` | `LiquidityAssetInfo` |
| `list_liquidity_assets` | `{}` | `ListLiquidityAssetsResponse` |
| `get_liquidity_provider_status` | `{}` | `LiquidityProviderStatus` |

### 2.2 Actor messages and handlers

```rust
enum LiquidityActorMessage {
    // ... existing variants ...
    AddLiquidityAsset(AddLiquidityAssetParams, RpcReplyPort<...>),
    UpdateLiquidityAsset(UpdateLiquidityAssetParams, RpcReplyPort<...>),
    DisableLiquidityAsset(String, RpcReplyPort<...>),
    ListLiquidityAssets(RpcReplyPort<...>),
    GetLiquidityProviderStatus(RpcReplyPort<...>),
}
```

Handlers delegate to `LiquidityStore`:

- `add`: validate (asset_id unique, kind = ckb|udt, udt requires type_script,
  min_amount <= max_amount), then `upsert_liquidity_asset`.
- `update`: validate existing asset_id, same field checks, then `upsert_liquidity_asset`.
- `disable`: `get_liquidity_asset`, set `enabled = false`, `upsert_liquidity_asset`.
- `list`: `list_liquidity_assets()`.
- `status`: read `provider_mode` flag + count non-terminal provider swaps +
  per-asset enabled + capacity.

### 2.3 Auth

These RPCs require `liquidity_provider` write capability. The actual Biscuit
middleware is deferred to M8; a placeholder gate ensures we don't accidentally
expose them unauthenticated in production.

## 3. Global Provider Toggle

### 3.1 Persistence

Add a `provider_mode: bool` flag to `LiquidityStore`:

- Persisted to RocksDB under a dedicated key.
- Defaults to `false` on first start (no existing key).
- `set_provider_mode(bool)` updates the flag.

### 3.2 Gating

Every provider-side handler checks `provider_mode` before proceeding:

- `handle_provider_quote_loop_out`
- `handle_provider_accept_loop_out`
- `handle_provider_accept_loop_in`

If `provider_mode == false`, return `"provider mode is disabled"` error.

Per-asset `enabled` check remains separate: a provider may be globally enabled
but a specific asset may be disabled.

### 3.3 get_liquidity_provider_status response

```json
{
  "provider_mode": true,
  "active_swaps": 3,
  "assets": [
    {
      "asset_id": "ckb",
      "enabled": true,
      "available_capacity": 100000000000,
      "active_swaps": 2
    }
  ]
}
```

## 4. E2E Test Strategy (After M5 Implementation)

After M5 implementation completes, a comprehensive E2E test suite will be written
in a separate phase. The test plan is:

### 4.1 Loop Out E2E

- Two Fiber nodes (client + provider), real CKB dev chain, deployed liquidity-lock.
- Client RPC `quote_loop_out` → `loop_out`.
- Provider `provider_accept_loop_out` → payout lock on-chain → payment settled →
  client claim → both sides `Success`.

### 4.2 Loop In E2E

- Client `quote_loop_in` → on-chain lock → `loop_in`.
- Provider `provider_accept_loop_in` → observed-output validation → payment settled
  → provider claim → both sides `Success`.

### 4.3 Recovery E2E

- Crash-and-restart at various non-terminal states for both directions.
- Verify recovery resumes correctly without orphaned or duplicate state.

### 4.4 Error and Edge Case E2E

- Wrong preimage, early refund blocked, late refund after lock time.
- UDT type mismatch, insufficient capacity, expired quote.
- Provider disabled mid-flow, concurrent accepts.

## 5. Implementation Order

Recommended task sequence:

1. **Task 1:** CkbChainActor `GetLiveCell` + native `validate_observed_loop_in_lock`
2. **Task 2:** Asset management RPC wiring (add/update/disable/list + actor messages)
3. **Task 3:** Global provider toggle (persistence + gating + `get_liquidity_provider_status`)
4. **Task 4:** Final review and E2E test suite (separate phase)
