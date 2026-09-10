# Fiber Liquidity M2 Store Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement durable liquidity swap and asset persistence in `Store`, with validated transitions, recovery/history queries, pagination, and tests.

**Architecture:** Keep `crates/fiber-lib/src/liquidity/store.rs` as the domain boundary and implement it for `Store` in `crates/fiber-lib/src/store/store_impl/mod.rs`. Add primary swap/asset keys plus state and asset indexes; recovery remains query-only for M2.

**Tech Stack:** Rust, Cargo, bincode serialization, `fiber_store::StorageBackend`, existing `StoreKeyValue`/batch patterns, cargo-nextest/cargo test.

---

## File Structure

- Modify `crates/fiber-lib/src/liquidity/store.rs`: derive `Serialize`/`Deserialize` for persisted liquidity structs/enums and add `InvalidAsset` error variant.
- Modify `crates/fiber-types/src/schema.rs`: reserve liquidity store key prefixes.
- Modify `crates/fiber-lib/src/store/store_impl/mod.rs`: add `KeyValue` variants, key/value serialization, helper parsers, and `impl LiquidityStore for Store`.
- Modify `crates/fiber-lib/src/store/tests/store.rs`: add liquidity store tests and small test builders.
- Run formatting and focused tests after each task.

---

### Task 1: Make Liquidity Store Types Persistable

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/store.rs`

- [ ] **Step 1: Write the failing serialization test**

Add this test module block to the end of `crates/fiber-lib/src/liquidity/store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liquidity_swap_record_round_trips_through_bincode() {
        let record = LiquiditySwapRecord {
            swap_id: [1u8; 32].into(),
            quote_id: [2u8; 32].into(),
            role: LiquiditySwapRole::Client,
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: "ckb".to_string(),
            state: LiquiditySwapState::Created,
            payment_hash: [3u8; 32].into(),
            payment_preimage: Some([4u8; 32].into()),
            amount: 1000,
            onchain_outpoint: None,
            payout_deadline: Some(2000),
            refund_after_lock_time: 3000,
            expires_at: 4000,
            failure_reason: Some("failed".to_string()),
            created_at: 5000,
            updated_at: 6000,
        };

        let bytes = bincode::serialize(&record).expect("serialize record");
        let decoded: LiquiditySwapRecord =
            bincode::deserialize(&bytes).expect("deserialize record");

        assert_eq!(decoded, record);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb liquidity_swap_record_round_trips_through_bincode -- --nocapture`

Expected: FAIL because `LiquiditySwapRecord`, `LiquiditySwapRole`, and `LiquiditySwapKind` do not implement serde traits.

- [ ] **Step 3: Add serde derives and asset validation error**

In `crates/fiber-lib/src/liquidity/store.rs`, update imports and derive attributes exactly as follows while keeping existing struct fields unchanged:

```rust
use fiber_types::{Hash256, LiquidityAsset, LiquidityAssetError, LiquiditySwapState};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum LiquiditySwapRole {
    Client,
    Provider,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum LiquiditySwapKind {
    LoopOut,
    LoopIn,
}

#[derive(Debug, Error)]
pub enum LiquidityStoreError {
    #[error("liquidity swap not found: {0:?}")]
    SwapNotFound(Hash256),
    #[error("liquidity asset not found: {0}")]
    AssetNotFound(String),
    #[error("invalid liquidity state transition from {from:?} to {to:?}")]
    InvalidStateTransition {
        from: LiquiditySwapState,
        to: LiquiditySwapState,
    },
    #[error("invalid liquidity asset: {0}")]
    InvalidAsset(#[from] LiquidityAssetError),
    #[error("liquidity store backend error: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct LiquiditySwapRecord {
    pub swap_id: Hash256,
    pub quote_id: Hash256,
    pub role: LiquiditySwapRole,
    pub swap_kind: LiquiditySwapKind,
    pub asset_id: String,
    pub state: LiquiditySwapState,
    pub payment_hash: Hash256,
    pub payment_preimage: Option<Hash256>,
    pub amount: u128,
    pub onchain_outpoint: Option<ckb_types::packed::OutPoint>,
    pub payout_deadline: Option<u64>,
    pub refund_after_lock_time: u64,
    pub expires_at: u64,
    pub failure_reason: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct LiquiditySwapFilter {
    pub state: Option<LiquiditySwapState>,
    pub asset_id: Option<String>,
    pub limit: Option<u64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct LiquiditySwapPage {
    pub swaps: Vec<LiquiditySwapRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct LiquidityStateTransition {
    pub state: LiquiditySwapState,
    pub updated_at: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct LiquiditySwapUpdate {
    pub payment_preimage: Option<Hash256>,
    pub onchain_outpoint: Option<ckb_types::packed::OutPoint>,
    pub failure_reason: Option<String>,
    pub updated_at: u64,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p fnn --features rocksdb liquidity_swap_record_round_trips_through_bincode -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/store.rs
git commit -m "feat: make liquidity store records persistable"
```

---

### Task 2: Add Store Keys And Insert/Get Swap Persistence

**Files:**
- Modify: `crates/fiber-types/src/schema.rs`
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs`
- Modify: `crates/fiber-lib/src/store/tests/store.rs`

- [ ] **Step 1: Write the failing insert/get test**

In `crates/fiber-lib/src/store/tests/store.rs`, add imports near existing imports:

```rust
use crate::liquidity::store::{
    LiquidityStore, LiquiditySwapKind, LiquiditySwapRecord, LiquiditySwapRole,
};
use fiber_types::{LiquiditySwapState, LiquidityAsset, LiquidityAssetKind};
```

Add helper and test near `test_store_invoice`:

```rust
fn mock_liquidity_swap(seed: u8, state: LiquiditySwapState, asset_id: &str) -> LiquiditySwapRecord {
    LiquiditySwapRecord {
        swap_id: [seed; 32].into(),
        quote_id: [seed.wrapping_add(1); 32].into(),
        role: LiquiditySwapRole::Client,
        swap_kind: LiquiditySwapKind::LoopOut,
        asset_id: asset_id.to_string(),
        state,
        payment_hash: [seed.wrapping_add(2); 32].into(),
        payment_preimage: None,
        amount: u128::from(seed) + 1000,
        onchain_outpoint: None,
        payout_deadline: Some(10_000 + u64::from(seed)),
        refund_after_lock_time: 20_000 + u64::from(seed),
        expires_at: 30_000 + u64::from(seed),
        failure_reason: None,
        created_at: 40_000 + u64::from(seed),
        updated_at: 50_000 + u64::from(seed),
    }
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_swap_insert_get() {
    let (store, _dir) = generate_store();
    let swap = mock_liquidity_swap(1, LiquiditySwapState::Created, "ckb");

    store.insert_liquidity_swap(swap.clone()).unwrap();

    assert_eq!(store.get_liquidity_swap(&swap.swap_id).unwrap(), Some(swap));
    assert_eq!(store.get_liquidity_swap(&[99u8; 32].into()).unwrap(), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb test_store_liquidity_swap_insert_get -- --nocapture`

Expected: FAIL because `Store` does not implement `LiquidityStore`.

- [ ] **Step 3: Add key prefixes and `KeyValue` variants**

In `crates/fiber-types/src/schema.rs`, add these constants after `CCH_ORDER_PREFIX`:

```rust
pub const LIQUIDITY_SWAP_PREFIX: u8 = 240;
pub const LIQUIDITY_SWAP_STATE_PREFIX: u8 = 241;
pub const LIQUIDITY_SWAP_ASSET_PREFIX: u8 = 242;
pub const LIQUIDITY_ASSET_PREFIX: u8 = 243;
```

In `crates/fiber-lib/src/store/store_impl/mod.rs`, add imports near the crate imports:

```rust
use crate::liquidity::store::{
    LiquidityStateTransition, LiquidityStore, LiquidityStoreError, LiquiditySwapFilter,
    LiquiditySwapPage, LiquiditySwapRecord, LiquiditySwapUpdate,
};
use fiber_types::{LiquidityAsset, LiquiditySwapState};
```

Add variants to `pub enum KeyValue`:

```rust
LiquiditySwap(Hash256, LiquiditySwapRecord),
LiquiditySwapStateIndex((LiquiditySwapState, Hash256)),
LiquiditySwapAssetIndex((String, Hash256)),
LiquidityAsset(String, LiquidityAsset),
```

Extend `StoreKeyValue for KeyValue::key`:

```rust
KeyValue::LiquiditySwap(swap_id, _) => [&[LIQUIDITY_SWAP_PREFIX], swap_id.as_ref()].concat(),
KeyValue::LiquiditySwapStateIndex((state, swap_id)) => [
    &[LIQUIDITY_SWAP_STATE_PREFIX],
    &[liquidity_state_key(*state)],
    swap_id.as_ref(),
]
.concat(),
KeyValue::LiquiditySwapAssetIndex((asset_id, swap_id)) => [
    &[LIQUIDITY_SWAP_ASSET_PREFIX],
    asset_id.as_bytes(),
    &[0],
    swap_id.as_ref(),
]
.concat(),
KeyValue::LiquidityAsset(asset_id, _) => {
    [&[LIQUIDITY_ASSET_PREFIX], asset_id.as_bytes()].concat()
}
```

Extend `StoreKeyValue for KeyValue::value`:

```rust
KeyValue::LiquiditySwap(_, swap) => serialize_to_vec(swap, "LiquiditySwapRecord"),
KeyValue::LiquiditySwapStateIndex(_) => Vec::new(),
KeyValue::LiquiditySwapAssetIndex(_) => Vec::new(),
KeyValue::LiquidityAsset(_, asset) => serialize_to_vec(asset, "LiquidityAsset"),
```

Add helper near other helper functions:

```rust
fn liquidity_state_key(state: LiquiditySwapState) -> u8 {
    match state {
        LiquiditySwapState::Created => 0,
        LiquiditySwapState::Quoted => 1,
        LiquiditySwapState::OnchainLockPending => 2,
        LiquiditySwapState::OnchainLocked => 3,
        LiquiditySwapState::PayoutPending => 4,
        LiquiditySwapState::PayoutLocked => 5,
        LiquiditySwapState::PaymentInFlight => 6,
        LiquiditySwapState::PaymentSettled => 7,
        LiquiditySwapState::ClaimPending => 8,
        LiquiditySwapState::RefundPending => 9,
        LiquiditySwapState::Success => 10,
        LiquiditySwapState::Failed => 11,
        LiquiditySwapState::Refunded => 12,
    }
}
```

- [ ] **Step 4: Add insert/get implementation**

Add this `impl` near other store trait impls:

```rust
impl LiquidityStore for Store {
    fn insert_liquidity_swap(&self, swap: LiquiditySwapRecord) -> Result<(), LiquidityStoreError> {
        if self.get_liquidity_swap(&swap.swap_id)?.is_some() {
            return Err(LiquidityStoreError::Backend(format!(
                "liquidity swap already exists: {:?}",
                swap.swap_id
            )));
        }

        let mut batch = self.batch();
        let primary = KeyValue::LiquiditySwap(swap.swap_id, swap.clone());
        let state_index = KeyValue::LiquiditySwapStateIndex((swap.state, swap.swap_id));
        let asset_index = KeyValue::LiquiditySwapAssetIndex((swap.asset_id.clone(), swap.swap_id));
        batch.put(primary.key(), primary.value());
        batch.put(state_index.key(), state_index.value());
        batch.put(asset_index.key(), asset_index.value());
        batch.commit();
        Ok(())
    }

    fn get_liquidity_swap(
        &self,
        swap_id: &Hash256,
    ) -> Result<Option<LiquiditySwapRecord>, LiquidityStoreError> {
        let key = [&[LIQUIDITY_SWAP_PREFIX], swap_id.as_ref()].concat();
        Ok(self
            .get(key)
            .map(|value| deserialize_from(value.as_ref(), "LiquiditySwapRecord")))
    }

    fn list_liquidity_swaps(
        &self,
        _filter: LiquiditySwapFilter,
    ) -> Result<LiquiditySwapPage, LiquidityStoreError> {
        Ok(LiquiditySwapPage::default())
    }

    fn update_liquidity_swap_state(
        &self,
        swap_id: &Hash256,
        _transition: LiquidityStateTransition,
    ) -> Result<(), LiquidityStoreError> {
        Err(LiquidityStoreError::SwapNotFound(*swap_id))
    }

    fn update_liquidity_swap(
        &self,
        swap_id: &Hash256,
        _update: LiquiditySwapUpdate,
    ) -> Result<(), LiquidityStoreError> {
        Err(LiquidityStoreError::SwapNotFound(*swap_id))
    }

    fn upsert_liquidity_asset(&self, _asset: LiquidityAsset) -> Result<(), LiquidityStoreError> {
        Err(LiquidityStoreError::Backend(
            "liquidity asset persistence unavailable in current task".to_string(),
        ))
    }

    fn get_liquidity_asset(
        &self,
        _asset_id: &str,
    ) -> Result<Option<LiquidityAsset>, LiquidityStoreError> {
        Ok(None)
    }

    fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError> {
        Ok(Vec::new())
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p fnn --features rocksdb test_store_liquidity_swap_insert_get -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-types/src/schema.rs crates/fiber-lib/src/store/store_impl/mod.rs crates/fiber-lib/src/store/tests/store.rs
git commit -m "feat: persist liquidity swaps"
```

---

### Task 3: Add State Transition Updates And State Index Queries

**Files:**
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs`
- Modify: `crates/fiber-lib/src/store/tests/store.rs`

- [ ] **Step 1: Write failing transition and state-list tests**

Add imports in `store.rs` tests if not already present:

```rust
use crate::liquidity::store::{LiquidityStateTransition, LiquiditySwapFilter};
```

Add tests:

```rust
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_swap_valid_transition_updates_state_index() {
    let (store, _dir) = generate_store();
    let swap = mock_liquidity_swap(2, LiquiditySwapState::Created, "ckb");
    store.insert_liquidity_swap(swap.clone()).unwrap();

    store
        .update_liquidity_swap_state(
            &swap.swap_id,
            LiquidityStateTransition {
                state: LiquiditySwapState::Quoted,
                updated_at: 99,
                reason: Some("quote accepted".to_string()),
            },
        )
        .unwrap();

    let updated = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
    assert_eq!(updated.state, LiquiditySwapState::Quoted);
    assert_eq!(updated.updated_at, 99);
    assert_eq!(updated.failure_reason, Some("quote accepted".to_string()));

    let created = store
        .list_liquidity_swaps(LiquiditySwapFilter {
            state: Some(LiquiditySwapState::Created),
            ..Default::default()
        })
        .unwrap();
    let quoted = store
        .list_liquidity_swaps(LiquiditySwapFilter {
            state: Some(LiquiditySwapState::Quoted),
            ..Default::default()
        })
        .unwrap();

    assert!(created.swaps.is_empty());
    assert_eq!(quoted.swaps, vec![updated]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_swap_invalid_transition_is_rejected() {
    let (store, _dir) = generate_store();
    let swap = mock_liquidity_swap(3, LiquiditySwapState::PaymentSettled, "ckb");
    store.insert_liquidity_swap(swap.clone()).unwrap();

    let result = store.update_liquidity_swap_state(
        &swap.swap_id,
        LiquidityStateTransition {
            state: LiquiditySwapState::Success,
            updated_at: 99,
            reason: Some("skip claim".to_string()),
        },
    );

    assert!(matches!(
        result,
        Err(crate::liquidity::store::LiquidityStoreError::InvalidStateTransition { .. })
    ));
    assert_eq!(store.get_liquidity_swap(&swap.swap_id).unwrap(), Some(swap));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb 'test_store_liquidity_swap_' -- --nocapture`

Expected: FAIL because the current transition/list stubs do not update records or read indexes.

- [ ] **Step 3: Add state index helpers and implementation**

In `store_impl/mod.rs`, add helpers:

```rust
impl Store {
    fn liquidity_swap_key(swap_id: &Hash256) -> Vec<u8> {
        [&[LIQUIDITY_SWAP_PREFIX], swap_id.as_ref()].concat()
    }

    fn liquidity_swap_state_index_key(state: LiquiditySwapState, swap_id: &Hash256) -> Vec<u8> {
        [&[LIQUIDITY_SWAP_STATE_PREFIX], &[liquidity_state_key(state)], swap_id.as_ref()].concat()
    }

    fn parse_liquidity_swap_id_from_index(key: &[u8]) -> Option<Hash256> {
        let bytes: [u8; 32] = key.get(key.len().checked_sub(32)?)?.try_into().ok()?;
        Some(bytes.into())
    }
}
```

Replace `get_liquidity_swap` key construction with `Self::liquidity_swap_key(swap_id)`.

Implement state transition:

```rust
fn update_liquidity_swap_state(
    &self,
    swap_id: &Hash256,
    transition: LiquidityStateTransition,
) -> Result<(), LiquidityStoreError> {
    let mut swap = self
        .get_liquidity_swap(swap_id)?
        .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;
    if !swap.state.can_transition_to(transition.state) {
        return Err(LiquidityStoreError::InvalidStateTransition {
            from: swap.state,
            to: transition.state,
        });
    }

    let old_state = swap.state;
    swap.state = transition.state;
    swap.updated_at = transition.updated_at;
    if let Some(reason) = transition.reason {
        swap.failure_reason = Some(reason);
    }

    let mut batch = self.batch();
    batch.delete(Self::liquidity_swap_state_index_key(old_state, swap_id));
    let primary = KeyValue::LiquiditySwap(*swap_id, swap.clone());
    let state_index = KeyValue::LiquiditySwapStateIndex((swap.state, *swap_id));
    batch.put(primary.key(), primary.value());
    batch.put(state_index.key(), state_index.value());
    batch.commit();
    Ok(())
}
```

Implement state-filtered `list_liquidity_swaps` minimally:

```rust
fn list_liquidity_swaps(
    &self,
    filter: LiquiditySwapFilter,
) -> Result<LiquiditySwapPage, LiquidityStoreError> {
    let limit = filter.limit.unwrap_or(0) as usize;
    let prefix = if let Some(state) = filter.state {
        vec![LIQUIDITY_SWAP_STATE_PREFIX, liquidity_state_key(state)]
    } else {
        vec![LIQUIDITY_SWAP_PREFIX]
    };
    let rows = self.collect_by_prefix_with(&prefix, PrefixIterOptions::new().limit(limit));
    let swaps = rows
        .into_iter()
        .filter_map(|kv| {
            if prefix[0] == LIQUIDITY_SWAP_PREFIX {
                Some(deserialize_from(kv.value.as_ref(), "LiquiditySwapRecord"))
            } else {
                Self::parse_liquidity_swap_id_from_index(&kv.key)
                    .and_then(|swap_id| self.get_liquidity_swap(&swap_id).ok().flatten())
            }
        })
        .collect();

    Ok(LiquiditySwapPage {
        swaps,
        next_cursor: None,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p fnn --features rocksdb 'test_store_liquidity_swap_' -- --nocapture`

Expected: PASS for insert/get, valid transition, invalid transition.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/store/store_impl/mod.rs crates/fiber-lib/src/store/tests/store.rs
git commit -m "feat: validate liquidity swap transitions"
```

---

### Task 4: Add Recovery Field Partial Updates

**Files:**
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs`
- Modify: `crates/fiber-lib/src/store/tests/store.rs`

- [ ] **Step 1: Write failing partial update test**

Add imports:

```rust
use crate::liquidity::store::LiquiditySwapUpdate;
```

Add test:

```rust
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_swap_update_preserves_none_fields() {
    let (store, _dir) = generate_store();
    let mut swap = mock_liquidity_swap(4, LiquiditySwapState::PaymentSettled, "ckb");
    swap.payment_preimage = Some([11u8; 32].into());
    swap.failure_reason = Some("existing".to_string());
    store.insert_liquidity_swap(swap.clone()).unwrap();

    store
        .update_liquidity_swap(
            &swap.swap_id,
            LiquiditySwapUpdate {
                payment_preimage: None,
                onchain_outpoint: None,
                failure_reason: None,
                updated_at: 77,
            },
        )
        .unwrap();

    let updated = store.get_liquidity_swap(&swap.swap_id).unwrap().unwrap();
    assert_eq!(updated.payment_preimage, swap.payment_preimage);
    assert_eq!(updated.failure_reason, swap.failure_reason);
    assert_eq!(updated.onchain_outpoint, None);
    assert_eq!(updated.updated_at, 77);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb test_store_liquidity_swap_update_preserves_none_fields -- --nocapture`

Expected: FAIL because the current `update_liquidity_swap` stub returns `SwapNotFound`.

- [ ] **Step 3: Implement partial update**

Replace the `update_liquidity_swap` body:

```rust
fn update_liquidity_swap(
    &self,
    swap_id: &Hash256,
    update: LiquiditySwapUpdate,
) -> Result<(), LiquidityStoreError> {
    let mut swap = self
        .get_liquidity_swap(swap_id)?
        .ok_or(LiquidityStoreError::SwapNotFound(*swap_id))?;

    if update.payment_preimage.is_some() {
        swap.payment_preimage = update.payment_preimage;
    }
    if update.onchain_outpoint.is_some() {
        swap.onchain_outpoint = update.onchain_outpoint;
    }
    if update.failure_reason.is_some() {
        swap.failure_reason = update.failure_reason;
    }
    swap.updated_at = update.updated_at;

    let kv = KeyValue::LiquiditySwap(*swap_id, swap);
    let mut batch = self.batch();
    batch.put(kv.key(), kv.value());
    batch.commit();
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p fnn --features rocksdb test_store_liquidity_swap_update_preserves_none_fields -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/store/store_impl/mod.rs crates/fiber-lib/src/store/tests/store.rs
git commit -m "feat: update liquidity recovery fields"
```

---

### Task 5: Add Asset Filtering And Pagination

**Files:**
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs`
- Modify: `crates/fiber-lib/src/store/tests/store.rs`

- [ ] **Step 1: Write failing asset and pagination tests**

Add tests:

```rust
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_swaps_filter_by_asset_and_state() {
    let (store, _dir) = generate_store();
    let ckb = mock_liquidity_swap(5, LiquiditySwapState::Quoted, "ckb");
    let udt_quoted = mock_liquidity_swap(6, LiquiditySwapState::Quoted, "udt");
    let udt_created = mock_liquidity_swap(7, LiquiditySwapState::Created, "udt");
    store.insert_liquidity_swap(ckb).unwrap();
    store.insert_liquidity_swap(udt_quoted.clone()).unwrap();
    store.insert_liquidity_swap(udt_created).unwrap();

    let page = store
        .list_liquidity_swaps(LiquiditySwapFilter {
            state: Some(LiquiditySwapState::Quoted),
            asset_id: Some("udt".to_string()),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(page.swaps, vec![udt_quoted]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_swaps_paginate_without_duplicates() {
    let (store, _dir) = generate_store();
    let first = mock_liquidity_swap(8, LiquiditySwapState::Created, "ckb");
    let second = mock_liquidity_swap(9, LiquiditySwapState::Created, "ckb");
    store.insert_liquidity_swap(first.clone()).unwrap();
    store.insert_liquidity_swap(second.clone()).unwrap();

    let page1 = store
        .list_liquidity_swaps(LiquiditySwapFilter {
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    let cursor = page1.next_cursor.clone().expect("next cursor");
    let page2 = store
        .list_liquidity_swaps(LiquiditySwapFilter {
            limit: Some(1),
            cursor: Some(cursor),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(page1.swaps.len(), 1);
    assert_eq!(page2.swaps.len(), 1);
    assert_ne!(page1.swaps[0].swap_id, page2.swaps[0].swap_id);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb 'test_store_liquidity_swaps_' -- --nocapture`

Expected: FAIL because asset filtering and cursors are incomplete.

- [ ] **Step 3: Add asset index and cursor helpers**

Add helpers in `impl Store`:

```rust
fn liquidity_swap_asset_index_prefix(asset_id: &str) -> Vec<u8> {
    [&[LIQUIDITY_SWAP_ASSET_PREFIX], asset_id.as_bytes(), &[0]].concat()
}

fn cursor_to_key(cursor: &Option<String>) -> Result<Option<Vec<u8>>, LiquidityStoreError> {
    cursor
        .as_ref()
        .map(|cursor| hex::decode(cursor).map_err(|e| LiquidityStoreError::Backend(e.to_string())))
        .transpose()
}

fn key_to_cursor(key: &[u8]) -> String {
    hex::encode(key)
}
```

`hex` is already available in `crates/fiber-lib/Cargo.toml`; do not add a new dependency.

- [ ] **Step 4: Complete `list_liquidity_swaps`**

Replace the current implementation with:

```rust
fn list_liquidity_swaps(
    &self,
    filter: LiquiditySwapFilter,
) -> Result<LiquiditySwapPage, LiquidityStoreError> {
    let limit = filter.limit.unwrap_or(0) as usize;
    let start_key = Self::cursor_to_key(&filter.cursor)?;
    let prefix = match (&filter.state, &filter.asset_id) {
        (Some(state), _) => vec![LIQUIDITY_SWAP_STATE_PREFIX, liquidity_state_key(*state)],
        (None, Some(asset_id)) => Self::liquidity_swap_asset_index_prefix(asset_id),
        (None, None) => vec![LIQUIDITY_SWAP_PREFIX],
    };

    let mut options = PrefixIterOptions::new();
    if let Some(start_key) = start_key.as_deref() {
        options = options.start_key(start_key).start_key_exclusive();
    }
    if limit > 0 {
        options = options.limit(limit + 1);
    }

    let rows = self.collect_by_prefix_with(&prefix, options);
    let has_more = limit > 0 && rows.len() > limit;
    let rows: Vec<_> = rows.into_iter().take(if limit > 0 { limit } else { usize::MAX }).collect();
    let next_cursor = if has_more {
        rows.last().map(|kv| Self::key_to_cursor(&kv.key))
    } else {
        None
    };

    let mut swaps = Vec::new();
    for kv in rows {
        let swap = if prefix[0] == LIQUIDITY_SWAP_PREFIX {
            deserialize_from(kv.value.as_ref(), "LiquiditySwapRecord")
        } else {
            let Some(swap_id) = Self::parse_liquidity_swap_id_from_index(&kv.key) else {
                continue;
            };
            let Some(swap) = self.get_liquidity_swap(&swap_id)? else {
                continue;
            };
            swap
        };
        if filter.asset_id.as_ref().is_some_and(|asset_id| &swap.asset_id != asset_id) {
            continue;
        }
        if filter.state.is_some_and(|state| swap.state != state) {
            continue;
        }
        swaps.push(swap);
    }

    Ok(LiquiditySwapPage { swaps, next_cursor })
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p fnn --features rocksdb 'test_store_liquidity_swaps_' -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-lib/src/store/store_impl/mod.rs crates/fiber-lib/src/store/tests/store.rs crates/fiber-lib/Cargo.toml
git commit -m "feat: query liquidity swap history"
```

---

### Task 6: Add Provider Asset Persistence

**Files:**
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs`
- Modify: `crates/fiber-lib/src/store/tests/store.rs`

- [ ] **Step 1: Write failing asset persistence tests**

Add helper and tests:

```rust
fn mock_liquidity_asset(asset_id: &str) -> LiquidityAsset {
    LiquidityAsset {
        asset_id: asset_id.to_string(),
        kind: LiquidityAssetKind::Ckb,
        udt_type_script: None,
        min_amount: 1,
        max_amount: 1000,
        available_capacity: 10_000,
        base_fee: 10,
        proportional_fee_ppm: 100,
        enabled: true,
    }
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_asset_upsert_get_list() {
    let (store, _dir) = generate_store();
    let ckb = mock_liquidity_asset("ckb");
    let mut udt = mock_liquidity_asset("udt");
    udt.kind = LiquidityAssetKind::Udt;
    udt.udt_type_script = Some(serde_json::from_value(serde_json::json!({
        "code_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "hash_type": "type",
        "args": "0x01"
    })).expect("script"));

    store.upsert_liquidity_asset(ckb.clone()).unwrap();
    store.upsert_liquidity_asset(udt.clone()).unwrap();

    assert_eq!(store.get_liquidity_asset("ckb").unwrap(), Some(ckb.clone()));
    assert_eq!(store.get_liquidity_asset("missing").unwrap(), None);
    assert_eq!(store.list_liquidity_assets().unwrap(), vec![ckb, udt]);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_asset_validation_rejects_invalid_asset() {
    let (store, _dir) = generate_store();
    let mut invalid = mock_liquidity_asset("bad-udt");
    invalid.kind = LiquidityAssetKind::Udt;
    invalid.udt_type_script = None;

    let result = store.upsert_liquidity_asset(invalid);

    assert!(matches!(
        result,
        Err(crate::liquidity::store::LiquidityStoreError::InvalidAsset(_))
    ));
    assert_eq!(store.list_liquidity_assets().unwrap(), Vec::<LiquidityAsset>::new());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb 'test_store_liquidity_asset' -- --nocapture`

Expected: FAIL because the current asset method stubs do not persist records.

- [ ] **Step 3: Implement asset methods**

Replace asset method bodies:

```rust
fn upsert_liquidity_asset(&self, asset: LiquidityAsset) -> Result<(), LiquidityStoreError> {
    asset.validate()?;
    let kv = KeyValue::LiquidityAsset(asset.asset_id.clone(), asset);
    let mut batch = self.batch();
    batch.put(kv.key(), kv.value());
    batch.commit();
    Ok(())
}

fn get_liquidity_asset(
    &self,
    asset_id: &str,
) -> Result<Option<LiquidityAsset>, LiquidityStoreError> {
    let key = [&[LIQUIDITY_ASSET_PREFIX], asset_id.as_bytes()].concat();
    Ok(self
        .get(key)
        .map(|value| deserialize_from(value.as_ref(), "LiquidityAsset")))
}

fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError> {
    Ok(self
        .collect_by_prefix(&[LIQUIDITY_ASSET_PREFIX])
        .into_iter()
        .map(|kv| deserialize_from(kv.value.as_ref(), "LiquidityAsset"))
        .collect())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p fnn --features rocksdb 'test_store_liquidity_asset' -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/store/store_impl/mod.rs crates/fiber-lib/src/store/tests/store.rs
git commit -m "feat: persist liquidity assets"
```

---

### Task 7: Add Durability And Validation Coverage

**Files:**
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs`
- Modify: `crates/fiber-lib/src/store/tests/store.rs`

- [ ] **Step 1: Write duplicate and reload tests**

Add tests:

```rust
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn test_store_liquidity_swap_duplicate_insert_is_rejected() {
    let (store, _dir) = generate_store();
    let swap = mock_liquidity_swap(10, LiquiditySwapState::Created, "ckb");
    store.insert_liquidity_swap(swap.clone()).unwrap();

    let result = store.insert_liquidity_swap(swap.clone());

    assert!(matches!(result, Err(crate::liquidity::store::LiquidityStoreError::Backend(_))));
    assert_eq!(store.get_liquidity_swap(&swap.swap_id).unwrap(), Some(swap));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn test_store_liquidity_records_survive_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path();
    let swap = mock_liquidity_swap(11, LiquiditySwapState::Created, "ckb");
    let asset = mock_liquidity_asset("ckb");
    {
        let store = open_store(path).expect("open store");
        store.insert_liquidity_swap(swap.clone()).unwrap();
        store.upsert_liquidity_asset(asset.clone()).unwrap();
    }
    {
        let store = open_store(path).expect("reopen store");
        assert_eq!(store.get_liquidity_swap(&swap.swap_id).unwrap(), Some(swap));
        assert_eq!(store.get_liquidity_asset("ckb").unwrap(), Some(asset));
    }
}
```

- [ ] **Step 2: Run tests to verify they pass or expose bugs**

Run: `cargo test -p fnn --features rocksdb 'test_store_liquidity' -- --nocapture`

Expected: PASS. If duplicate insert overwrites, fix `insert_liquidity_swap` to check existence before batching.

- [ ] **Step 3: Add check_validate deserialization arms**

In `check_validate`, add match arms:

```rust
LIQUIDITY_SWAP_PREFIX => {
    check_deserialization::<LiquiditySwapRecord>(&value, "LIQUIDITY_SWAP_PREFIX", &mut errors);
}
LIQUIDITY_SWAP_STATE_PREFIX => {}
LIQUIDITY_SWAP_ASSET_PREFIX => {}
LIQUIDITY_ASSET_PREFIX => {
    check_deserialization::<LiquidityAsset>(&value, "LIQUIDITY_ASSET_PREFIX", &mut errors);
}
```

- [ ] **Step 4: Run full liquidity/store focused checks**

Run:

```bash
cargo test -p fnn --features rocksdb 'test_store_liquidity' -- --nocapture
cargo test -p fnn --features rocksdb liquidity -- --nocapture
cargo fmt --all -- --check
```

Expected: all PASS, formatting check produces no output.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/store/store_impl/mod.rs crates/fiber-lib/src/store/tests/store.rs
git commit -m "test: cover liquidity store recovery"
```

---

### Task 8: Final Review And Verification

**Files:**
- Review all touched files.

- [ ] **Step 1: Inspect status and diffs**

Run:

```bash
git status --short
git diff --stat HEAD~6..HEAD
git log --oneline -10
```

Expected: working tree clean; recent commits are the M2 task commits.

- [ ] **Step 2: Run required verification**

Run:

```bash
cargo fmt --all -- --check
cargo test -p fnn --features rocksdb 'test_store_liquidity' -- --nocapture
cargo test -p fnn --features rocksdb liquidity -- --nocapture
```

Expected: all PASS.

- [ ] **Step 3: Request code review**

Use the `requesting-code-review` skill. Ask the reviewer to check:

- transition validation;
- index consistency;
- pagination cursor semantics;
- partial update preservation;
- asset validation persistence;
- no runtime recovery side effects added before M3/M4.

- [ ] **Step 4: Fix review findings one at a time**

For each blocker or important finding:

1. Write or update the failing test.
2. Run it and verify failure.
3. Implement the minimal fix.
4. Run the focused test and required verification.
5. Commit with a focused message.

- [ ] **Step 5: Final commit status**

Run: `git status --short`

Expected: clean worktree.
