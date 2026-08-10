# Fiber Liquidity M5 Integrated Provider Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement native CKB observed-output validation, asset management RPCs, and a global provider toggle to make the provider safe and operable.

**Architecture:** Add `GetLiveCell` to `CkbChainActor`, implement `validate_observed_loop_in_lock` in `CkbLiquidityChainWatcher` by calling that actor message, add asset management RPCs delegating to `LiquidityActor`, and add a persisted `provider_mode` flag gating all provider quote/accept paths.

**Tech Stack:** Rust, Ractor, jsonrpsee, ckb_types, existing `CkbChainActor`, existing `LiquidityStore`, `cargo test -p fnn --features rocksdb`, `cargo fmt`.

---

## File Structure

- Modify `crates/fiber-lib/src/ckb/actor.rs`: add `GetLiveCell` message and handler.
- Modify `crates/fiber-lib/src/liquidity/chain.rs`: replace hardcoded `validate_observed_loop_in_lock` Err with real implementation; add test adapter validation.
- Modify `crates/fiber-lib/src/liquidity/actor.rs`: add actor messages/handlers for asset management RPCs and provider toggle; wire toggle gates into provider handlers; add tests.
- Modify `crates/fiber-lib/src/liquidity/store.rs`: add `set_provider_mode`/`get_provider_mode` trait methods and `LiquidityStoreError` variants.
- Modify `crates/fiber-lib/src/store/store_impl/mod.rs`: implement provider_mode RocksDB persistence.
- Modify `crates/fiber-json-types/src/liquidity.rs`: update `LiquidityProviderStatus` with `active_swaps` field.
- Modify `crates/fiber-lib/src/rpc/liquidity.rs`: wire new RPC methods, update method list, add tests.
- Modify `crates/fiber-lib/src/rpc/README.md`: regenerate.

---

### Task 1: CkbChainActor GetLiveCell + Native validate_observed_loop_in_lock

**Files:**
- Modify: `crates/fiber-lib/src/ckb/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Test: `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Add GetLiveCell message to CkbChainMessage**

In `crates/fiber-lib/src/ckb/actor.rs`, add after `ReportSendTxError`:

```rust
GetLiveCell(
    packed::OutPoint,
    RpcReplyPort<Result<Option<packed::CellOutput>, RpcError>>,
),
```

- [ ] **Step 2: Add GetLiveCell handler to CkbChainActor::handle**

In the same file, in the `handle` match block after the `ReportSendTxError` arm:

```rust
CkbChainMessage::GetLiveCell(outpoint, reply_port) => {
    let ckb_client = state.config.ckb_rpc_client();
    let outpoint_json: ckb_jsonrpc_types::OutPoint = outpoint.clone().into();
    let result = ckb_client
        .get_live_cell(outpoint_json, true)
        .await
        .map(|live_cell| {
            live_cell.cell.map(|cell| cell.output.into())
        })
        .map_err(|e| RpcError::Other(format!("get_live_cell failed: {e}")));
    if !reply_port.is_closed() {
        let _ = reply_port.send(result);
    }
}
```

- [ ] **Step 3: Run cargo check to verify compilation**

Run: `cargo check -p fnn --features rocksdb`
Expected: PASS (no compile errors on the new message and handler)

- [ ] **Step 4: Commit**

```bash
git add crates/fiber-lib/src/ckb/actor.rs
git commit -m "feat: add GetLiveCell message to CkbChainActor"
```

- [ ] **Step 5: Write failing test for validate_observed_loop_in_lock CKB validation**

In `crates/fiber-lib/src/liquidity/chain.rs` test module, add:

```rust
#[test]
fn validate_observed_loop_in_lock_rejects_cell_not_found() {
    let events = Shared::new(Vec::new());
    let store = TestLiquidityStore::new(events.clone(), "ckb");
    let quote = test_loop_in_quote(now_ms() + 60_000);
    store
        .insert_loop_out_quote(quote.clone(), now_ms())
        .unwrap();
    let watcher = CkbLiquidityChainWatcher::new(
        spawn_mock_ckb_chain_actor_with_no_cell().await,
        store.clone(),
    );
    let mut watcher = watcher;
    let outpoint = packed::OutPoint::new([99u8; 32].pack(), 0);

    let result = watcher
        .validate_observed_loop_in_lock(&quote, &outpoint)
        .await;

    assert!(result.is_err());
    let error = result.unwrap_err().to_string();
    assert!(error.contains("cell not found") || error.contains("not found"));
}
```

Run: `cargo test -p fnn --features rocksdb ckb_watcher_validate_observed_loop_in_lock_rejects_cell_not_found -- --nocapture`

Expected: FAIL because we need `test_loop_in_quote` to produce a `LoopOutQuoteTerms` with proper lock scripts for validation.

- [ ] **Step 6: Add test helper test_loop_in_quote**

In `crates/fiber-lib/src/liquidity/chain.rs` test module, add a test helper:

```rust
fn test_loop_in_quote(now_ms: u64) -> LoopOutQuoteTerms {
    let payment_hash: Hash256 = loop_out_quote_hash(
        &ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: 100,
            receiver: "ckt1receiver".to_string(),
            max_provider_fee: 10,
            max_routing_fee: 5,
            expires_after_seconds: 60,
        },
        now_ms,
        b"payment",
    );
    LoopOutQuoteTerms {
        quote_id: payment_hash,
        swap_kind: LiquiditySwapKind::LoopIn,
        provider: deterministic_provider_pubkey(),
        asset: test_ckb_asset(),
        amount: 100,
        provider_fee: 5,
        routing_fee_limit: 10,
        onchain_fee_estimate_ckb: 1_000,
        capacity_requirement_ckb: 61,
        payment_hash,
        expires_at: now_ms,
        payout_deadline: now_ms,
        refund_after_lock_time: now_ms,
        claimant_lock: packed::Script::new_builder()
            .code_hash([10u8; 32].pack())
            .hash_type(packed::Byte::new(0))
            .args(vec![1u8; 20].pack())
            .build(),
        refund_lock: packed::Script::new_builder()
            .code_hash([11u8; 32].pack())
            .hash_type(packed::Byte::new(0))
            .args(vec![2u8; 20].pack())
            .build(),
        client_invoice: None,
    }
}

fn test_ckb_asset() -> LiquidityAsset {
    LiquidityAsset {
        asset_id: "ckb".to_string(),
        kind: LiquidityAssetKind::Ckb,
        udt_type_script: None,
        min_amount: 1,
        max_amount: 100_000_000,
        available_capacity: 1_000_000,
        base_fee: 5,
        proportional_fee_ppm: 0,
        enabled: true,
    }
}
```

- [ ] **Step 7: Actually write the real test using CkbLiquidityChainWatcher with no-cell mock**

Since we can't easily test the real `CkbLiquidityChainWatcher` because the chain actor pattern uses real RPC, write a test that validates the validation logic in isolation by testing the arg-parsing layer. Add a parse helper and test it:

In `crates/fiber-lib/src/liquidity/chain.rs`, add:

```rust
/// Parse the 152-byte liquidity-lock args and validate against quote.
fn validate_liquidity_lock_args(
    args: &[u8],
    quote: &LoopOutQuoteTerms,
    liquidity_lock_code_hash: &packed::Byte32,
    liquidity_lock_hash_type: u8,
    cell_code_hash: &packed::Byte32,
    cell_hash_type: u8,
) -> Result<(), LiquidityLoopOutError> {
    if cell_code_hash != liquidity_lock_code_hash || cell_hash_type != liquidity_lock_hash_type {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock script does not match liquidity-lock contract".to_string(),
        ));
    }
    if args.len() != 152 {
        return Err(LiquidityLoopOutError::Chain(format!(
            "observed lock args length {} does not match expected 152",
            args.len()
        )));
    }
    let expected_payment_hash: [u8; 32] = args[0..32].try_into().unwrap();
    if expected_payment_hash != quote.payment_hash.0 {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock payment_hash mismatch".to_string(),
        ));
    }
    let expected_claimant_hash = ckb_hash::blake2b_256(quote.claimant_lock.as_slice());
    if args[32..64] != expected_claimant_hash {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock claimant_lock_hash mismatch".to_string(),
        ));
    }
    let expected_refund_hash = ckb_hash::blake2b_256(quote.refund_lock.as_slice());
    if args[64..96] != expected_refund_hash {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock refund_lock_hash mismatch".to_string(),
        ));
    }
    let refund_after: u64 = u64::from_le_bytes(args[96..104].try_into().unwrap());
    if refund_after != quote.refund_after_lock_time {
        return Err(LiquidityLoopOutError::Chain(
            "observed lock refund_after_lock_time mismatch".to_string(),
        ));
    }
    let onchain_amount: u128 = u128::from_le_bytes(args[104..120].try_into().unwrap());
    let expected_gross = loop_in_gross_onchain_amount(quote)?;
    if onchain_amount != expected_gross {
        return Err(LiquidityLoopOutError::Chain(format!(
            "observed lock amount mismatch: expected {expected_gross}, got {onchain_amount}"
        )));
    }
    let asset_type_hash = &args[120..152];
    match quote.asset.kind {
        LiquidityAssetKind::Ckb => {
            if asset_type_hash != [0u8; 32] {
                return Err(LiquidityLoopOutError::Chain(
                    "observed lock asset_type_hash mismatch for CKB asset".to_string(),
                ));
            }
        }
        LiquidityAssetKind::Udt => {
            let expected_udt_hash = if let Some(ref udt) = quote.asset.udt_type_script {
                ckb_hash::blake2b_256(udt.as_slice())
            } else {
                return Err(LiquidityLoopOutError::Chain(
                    "UDT asset missing udt_type_script".to_string(),
                ));
            };
            if asset_type_hash != expected_udt_hash.as_slice() {
                return Err(LiquidityLoopOutError::Chain(
                    "observed lock asset_type_hash mismatch for UDT asset".to_string(),
                ));
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 8: Add tests for validate_liquidity_lock_args**

```rust
#[test]
fn validate_liquidity_lock_args_accepts_matching_ckb_lock_script_args() {
    use crate::liquidity::build_liquidity_lock_args;

    let quote = test_loop_in_quote(1_000_000);
    let code_hash = [9u8; 32].pack();
    let args = build_liquidity_lock_args(
        quote.payment_hash.0,
        &quote.claimant_lock,
        &quote.refund_lock,
        quote.refund_after_lock_time,
        loop_in_gross_onchain_amount(&quote).unwrap(),
        None,
    );
    assert!(validate_liquidity_lock_args(
        &args,
        &quote,
        &code_hash,
        0,
        &code_hash,
        0,
    )
    .is_ok());
}

#[test]
fn validate_liquidity_lock_args_rejects_wrong_code_hash() {
    let quote = test_loop_in_quote(1_000_000);
    let args = vec![0u8; 152];
    let result = validate_liquidity_lock_args(
        &args,
        &quote,
        &[9u8; 32].pack(),
        0,
        &[8u8; 32].pack(),
        0,
    );
    assert!(result.unwrap_err().to_string().contains("liquidity-lock contract"));
}

#[test]
fn validate_liquidity_lock_args_rejects_wrong_payment_hash() {
    let quote = test_loop_in_quote(1_000_000);
    let mut args = vec![0u8; 152];
    // set a different payment_hash
    args[0..32].copy_from_slice(&[99u8; 32]);
    let result = validate_liquidity_lock_args(
        &args,
        &quote,
        &[9u8; 32].pack(),
        0,
        &[9u8; 32].pack(),
        0,
    );
    assert!(result.unwrap_err().to_string().contains("payment_hash"));
}

#[test]
fn validate_liquidity_lock_args_rejects_wrong_amount() {
    use crate::liquidity::build_liquidity_lock_args;

    let quote = test_loop_in_quote(1_000_000);
    let code_hash = [9u8; 32].pack();
    let args = build_liquidity_lock_args(
        quote.payment_hash.0,
        &quote.claimant_lock,
        &quote.refund_lock,
        quote.refund_after_lock_time,
        999, // wrong amount
        None,
    );
    let result = validate_liquidity_lock_args(
        &args,
        &quote,
        &code_hash,
        0,
        &code_hash,
        0,
    );
    assert!(result.unwrap_err().to_string().contains("amount"));
}
```

Run: `cargo test -p fnn --features rocksdb validate_liquidity_lock_args -- --nocapture`
Expected: FAIL (not yet compiled)

- [ ] **Step 9: Implement validate_liquidity_lock_args and make tests pass**

Add the function (from Step 7) and run tests:
```bash
cargo test -p fnn --features rocksdb validate_liquidity_lock_args -- --nocapture
```
Expected: PASS

- [ ] **Step 10: Implement validate_observed_loop_in_lock in CkbLiquidityChainWatcher**

Replace the hardcoded Err in `crates/fiber-lib/src/liquidity/chain.rs`:

```rust
async fn validate_observed_loop_in_lock(
    &mut self,
    quote: &LoopOutQuoteTerms,
    outpoint: &packed::OutPoint,
) -> Result<(), Self::Error> {
    let artifact = self
        .liquidity_lock_artifact
        .as_ref()
        .ok_or_else(Self::missing_payout_builder)?;
    let cell = ractor::call!(self.ckb_chain_actor, |reply| {
        CkbChainMessage::GetLiveCell(outpoint.clone(), reply)
    })
    .map_err(|error| {
        LiquidityLoopOutError::Chain(format!(
            "failed to query live cell for observed loop in lock: {error}"
        ))
    })?
    .map_err(|error| {
        LiquidityLoopOutError::Chain(format!(
            "ckb rpc error querying live cell for observed loop in lock: {error}"
        ))
    })?
    .ok_or_else(|| {
        LiquidityLoopOutError::Chain(
            "observed loop in lock cell not found or already spent".to_string(),
        )
    })?;

    let lock_script = cell.lock();
    let args = lock_script.args().raw_data();
    validate_liquidity_lock_args(
        &args,
        quote,
        &artifact.code_hash,
        artifact.hash_type.into(),
        &lock_script.code_hash(),
        lock_script.hash_type().into(),
    )?;

    if quote.asset.kind == LiquidityAssetKind::Udt {
        let cell_type_script = cell.type_().to_opt();
        let expected_type = quote.asset.udt_type_script.as_ref();
        match (cell_type_script, expected_type) {
            (Some(cell_type), Some(expected)) if cell_type == *expected => {}
            (None, None) => {}
            _ => {
                return Err(LiquidityLoopOutError::Chain(
                    "observed loop in lock UDT type script mismatch".to_string(),
                ));
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 11: Add imports in chain.rs**

Add at top of `crates/fiber-lib/src/liquidity/chain.rs`:

```rust
use ckb_types::prelude::Entity;
use crate::liquidity::quote::loop_in_gross_onchain_amount;
```

- [ ] **Step 12: Run tests to verify**

```bash
cargo test -p fnn --features rocksdb validate_liquidity_lock_args -- --nocapture
cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture
```
Expected: all chain tests pass + new tests pass

- [ ] **Step 13: Commit**

```bash
git add crates/fiber-lib/src/liquidity/chain.rs
git commit -m "feat: implement native validate_observed_loop_in_lock"
```

---

### Task 2: Asset Management RPCs

**Files:**
- Modify: `crates/fiber-json-types/src/liquidity.rs`
- Modify: `crates/fiber-lib/src/liquidity/store.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs`
- Modify: `crates/fiber-lib/src/rpc/README.md`

- [ ] **Step 1: Update LiquidityProviderStatus type**

In `crates/fiber-json-types/src/liquidity.rs`, add `active_swaps`:

```rust
pub struct LiquidityProviderStatus {
    /// Whether provider mode is enabled.
    pub enabled: bool,
    /// Number of currently enabled assets.
    pub enabled_asset_count: u64,
    /// Number of non-terminal provider swaps.
    pub active_swaps: u64,
}
```

- [ ] **Step 2: Add store traits for provider_mode**

In `crates/fiber-lib/src/liquidity/store.rs`, add to `LiquidityStore` trait:

```rust
    /// Set the global provider mode flag.
    fn set_provider_mode(&self, enabled: bool) -> Result<(), LiquidityStoreError>;

    /// Read the global provider mode flag (defaults to false if not set).
    fn get_provider_mode(&self) -> Result<bool, LiquidityStoreError>;
```

- [ ] **Step 3: Add LiquidityStoreError variant**

In `crates/fiber-lib/src/liquidity/store.rs`, add:

```rust
    /// Provider mode operation failed.
    #[error("provider mode operation failed: {0}")]
    ProviderModeError(String),
```

- [ ] **Step 4: Write failing test for AddLiquidityAsset RPC delegation**

In `crates/fiber-lib/src/liquidity/actor.rs` test module, add:

```rust
#[tokio::test]
async fn add_liquidity_asset_persists_and_returns_asset_info() {
    let harness = RuntimeActorHarness::new_provider();
    let params = AddLiquidityAssetParams {
        asset: LiquidityAssetInfo {
            asset_id: "test-udt".to_string(),
            kind: fiber_json_types::LiquidityAssetKind::Udt,
            udt_type_script: Some(ckb_jsonrpc_types::Script {
                code_hash: H256::from([0x11u8; 32]),
                hash_type: ckb_jsonrpc_types::ScriptHashType::Data,
                args: ckb_jsonrpc_types::JsonBytes::from_bytes(vec![1u8; 20]),
            }),
            min_amount: 10,
            max_amount: 1000,
            available_capacity: 5000,
            base_fee: 5,
            proportional_fee_ppm: 100,
            enabled: true,
        },
    };
    let result = harness.call_add_liquidity_asset(params).await.unwrap();

    assert_eq!(result.asset_id, "test-udt");
    assert_eq!(result.enabled, true);
    let persisted = harness
        .store
        .get_liquidity_asset("test-udt")
        .unwrap()
        .unwrap();
    assert_eq!(persisted.asset_id, "test-udt");
}
```

Run: `cargo test -p fnn --features rocksdb add_liquidity_asset_persists_and_returns_asset_info -- --nocapture`
Expected: FAIL

- [ ] **Step 5: Add actor message and handler for add_liquidity_asset**

In `crates/fiber-lib/src/liquidity/actor.rs`:

Add import:
```rust
use fiber_json_types::{
    LiquidityQuoteResponse, LiquiditySwapResponse, LoopInParams, LoopOutParams,
    ProviderAcceptLoopInParams, ProviderAcceptLoopOutParams, ProviderQuoteLoopOutParams,
    QuoteLoopInParams, QuoteLoopOutParams,
    AddLiquidityAssetParams, UpdateLiquidityAssetParams, LiquidityAssetInfo,
    ListLiquidityAssetsResponse, LiquidityProviderStatus,
};
```

Add to `LiquidityActorMessage`:
```rust
    AddLiquidityAsset(
        AddLiquidityAssetParams,
        RpcReplyPort<Result<LiquidityAssetInfo, LiquidityLoopOutError>>,
    ),
```

Add handler (method on `LiquidityActorState`):
```rust
    fn handle_add_liquidity_asset(
        &mut self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, LiquidityLoopOutError> {
        let asset = params.asset;
        if self
            .store
            .get_liquidity_asset(&asset.asset_id)
            .map_err(map_store_error)?
            .is_some()
        {
            return Err(LiquidityLoopOutError::Store(
                format!("asset {} already exists", asset.asset_id),
            ));
        }
        let liquidity_asset = json_asset_to_liquidity_asset(&asset)?;
        self.store
            .upsert_liquidity_asset(liquidity_asset)
            .map_err(map_store_error)?;
        Ok(asset)
    }
```

Add conversion helper:
```rust
fn json_asset_to_liquidity_asset(
    info: &LiquidityAssetInfo,
) -> Result<LiquidityAsset, LiquidityLoopOutError> {
    if info.asset_id.trim().is_empty() {
        return Err(LiquidityLoopOutError::Store(
            "asset_id must not be empty".to_string(),
        ));
    }
    if info.min_amount > info.max_amount {
        return Err(LiquidityLoopOutError::Store(
            "min_amount must not exceed max_amount".to_string(),
        ));
    }
    let kind = match info.kind {
        fiber_json_types::LiquidityAssetKind::Ckb => LiquidityAssetKind::Ckb,
        fiber_json_types::LiquidityAssetKind::Udt => LiquidityAssetKind::Udt,
    };
    let udt_type_script = match (kind, &info.udt_type_script) {
        (LiquidityAssetKind::Udt, Some(script)) => Some(Script::new_builder()
            .code_hash(packed::Byte32::from_slice(script.code_hash.as_bytes()).unwrap())
            .hash_type(match script.hash_type {
                ckb_jsonrpc_types::ScriptHashType::Data => packed::Byte::new(0),
                ckb_jsonrpc_types::ScriptHashType::Type => packed::Byte::new(1),
                ckb_jsonrpc_types::ScriptHashType::Data1 => packed::Byte::new(2),
                ckb_jsonrpc_types::ScriptHashType::Data2 => packed::Byte::new(4),
            })
            .args(ckb_types::bytes::Bytes::from(script.args.as_bytes().to_vec()).pack())
            .build()),
        (LiquidityAssetKind::Udt, None) => {
            return Err(LiquidityLoopOutError::Store(
                "UDT asset must have a udt_type_script".to_string(),
            ));
        }
        _ => None,
    };
    Ok(LiquidityAsset {
        asset_id: info.asset_id.clone(),
        kind,
        udt_type_script,
        min_amount: info.min_amount,
        max_amount: info.max_amount,
        available_capacity: info.available_capacity,
        base_fee: info.base_fee,
        proportional_fee_ppm: info.proportional_fee_ppm,
        enabled: info.enabled,
    })
}
```

- [ ] **Step 6: Add test helper call_add_liquidity_asset to RuntimeActorHarness**

```rust
async fn call_add_liquidity_asset(
    &self,
    params: AddLiquidityAssetParams,
) -> Result<LiquidityAssetInfo, LiquidityLoopOutError> {
    let actor = self.spawn_actor().await;
    ractor::call!(actor, |reply| {
        LiquidityActorMessage::AddLiquidityAsset(params, reply)
    })
    .unwrap()
}
```

Run test: `cargo test -p fnn --features rocksdb add_liquidity_asset_persists_and_returns_asset_info -- --nocapture`
Expected: PASS

- [ ] **Step 7: Add RPC delegation and wiring for add_liquidity_asset**

In `crates/fiber-lib/src/rpc/liquidity.rs`, add to `LiquidityRpc` trait:
```rust
    #[method(name = "add_liquidity_asset")]
    async fn add_liquidity_asset(
        &self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned>;
```

Add to `liquidity_rpc_method_names`:
```rust
    "add_liquidity_asset",
```

Add delegate in `LiquidityRpcServer` impl:
```rust
    async fn add_liquidity_asset(
        &self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        self.add_liquidity_asset(params).await
    }
```

Add implementation:
```rust
    pub async fn add_liquidity_asset(
        &self,
        params: AddLiquidityAssetParams,
    ) -> Result<LiquidityAssetInfo, ErrorObjectOwned> {
        self.call_liquidity_actor_mutation(params, |params, reply| {
            LiquidityActorMessage::AddLiquidityAsset(params, reply)
        })
        .await
    }
```

Note: reuse the existing `call_liquidity_actor` helper pattern. The helper is generic enough already.

- [ ] **Step 8: Add update/disable/list/status following the same pattern**

Repeat Steps 5-7 for:
- `update_liquidity_asset`: accepts `UpdateLiquidityAssetParams`, validates existing asset, calls `upsert_liquidity_asset`
- `disable_liquidity_asset`: accepts `{ asset_id: String }`, looks up asset, sets `enabled=false`, calls `upsert_liquidity_asset`
- `list_liquidity_assets`: no params, returns `ListLiquidityAssetsResponse`
- `get_liquidity_provider_status`: no params, returns `LiquidityProviderStatus`

Each with: actor message → handler → test harness method → RPC wiring → RPC test mock arm.

Add each method to `liquidity_rpc_method_names`.

- [ ] **Step 9: Run all liquidity RPC tests**

```bash
cargo test -p fnn --features rocksdb rpc::liquidity -- --nocapture
```
Expected: all pass

- [ ] **Step 10: Add RPC mock arm for new messages**

In `LiquidityRpcMock::handle` (in `crates/fiber-lib/src/rpc/liquidity.rs` test module):

```rust
LiquidityActorMessage::AddLiquidityAsset(..) => {
    events.lock().expect("events lock").push("add_liquidity_asset");
    let _ = reply.send(Ok(LiquidityAssetInfo { ... }));
}
```

And similar for UpdateLiquidityAsset, DisableLiquidityAsset, ListLiquidityAssets, GetLiquidityProviderStatus.

- [ ] **Step 11: Regenerate RPC docs and run full tests**

```bash
make gen-rpc-doc
cargo test -p fnn --features rocksdb rpc:: -- --nocapture
```
Expected: all pass

- [ ] **Step 12: Commit**

```bash
git add crates/fiber-json-types/src/liquidity.rs crates/fiber-lib/src/liquidity/store.rs crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/rpc/liquidity.rs crates/fiber-lib/src/rpc/README.md
git commit -m "feat: wire asset management liquidity RPCs"
```

---

### Task 3: Global Provider Toggle

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/store.rs`
- Modify: `crates/fiber-lib/src/store/store_impl/mod.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write test for provider_mode gating**

```rust
#[tokio::test]
async fn provider_quote_loop_out_rejects_when_provider_mode_disabled() {
    let harness = RuntimeActorHarness::new_provider_with_asset();
    assert!(!harness.store.get_provider_mode().unwrap());

    let result = harness
        .call_provider_quote(ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: 100,
            receiver: "ckt1receiver".to_string(),
            max_provider_fee: 10,
            max_routing_fee: 5,
            expires_after_seconds: 60,
        })
        .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("provider mode is disabled"));
}
```

Run: `cargo test -p fnn --features rocksdb provider_quote_loop_out_rejects_when_provider_mode_disabled -- --nocapture`
Expected: FAIL

- [ ] **Step 2: Implement generic provider_mode check helper**

```rust
fn ensure_provider_mode<S>(store: &S) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    if !store.get_provider_mode().map_err(map_store_error)? {
        return Err(LiquidityLoopOutError::Store(
            "provider mode is disabled".to_string(),
        ));
    }
    Ok(())
}
```

- [ ] **Step 3: Gate provider handlers**

Add `ensure_provider_mode(&self.store)?;` at the top of:
- `handle_provider_quote_loop_out`
- `handle_provider_accept_loop_out`
- `handle_provider_accept_loop_in`

- [ ] **Step 4: Run test to verify gating**

```bash
cargo test -p fnn --features rocksdb provider_quote_loop_out_rejects_when_provider_mode_disabled -- --nocapture
```
Expected: PASS

- [ ] **Step 5: Add get_provider_mode/set_provider_mode to TestLiquidityStore**

In the test store implementation:
```rust
fn set_provider_mode(&self, enabled: bool) -> Result<(), LiquidityStoreError> {
    *self.provider_mode.borrow_mut() = enabled;
    Ok(())
}

fn get_provider_mode(&self) -> Result<bool, LiquidityStoreError> {
    Ok(*self.provider_mode.borrow())
}
```

And add `provider_mode: Shared<bool>` field to `TestLiquidityStore` struct with initialization.

- [ ] **Step 6: Add provider_mode to TestLiquidityStore struct**

```rust
struct TestLiquidityStore {
    // ... existing fields ...
    provider_mode: Shared<bool>,
}
```

Update constructor to include `provider_mode: Shared::new(false)`.

- [ ] **Step 7: Add get_provider_mode to store RocksDB impl**

In `crates/fiber-lib/src/store/store_impl/mod.rs`, use a key like `liquidity-provider-mode` with a simple boolean value (serialized as u8: 0 or 1). Write the implementation in the `LiquidityStore` impl block.

Key: `b"liquidity-provider-mode"`
Value: `1u8` for enabled, `0u8` for disabled. Default: false if key not found.

- [ ] **Step 8: Run full actor test suite**

```bash
cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture
```
Expected: all pass

- [ ] **Step 9: Commit**

```bash
git add crates/fiber-lib/src/liquidity/store.rs crates/fiber-lib/src/store/store_impl/mod.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: add global provider mode toggle"
```

---

### Task 4: Final Verification

- [ ] **Step 1: Run all targeted test suites**

```bash
cargo test -p fnn --features rocksdb liquidity::quote -- --nocapture
cargo test -p fnn --features rocksdb liquidity::tx -- --nocapture
cargo test -p fnn --features rocksdb liquidity::chain -- --nocapture
cargo test -p fnn --features rocksdb liquidity::actor -- --nocapture
cargo test -p fnn --features rocksdb rpc:: -- --nocapture
cargo test -p fiber-json-types liquidity -- --nocapture
```

- [ ] **Step 2: Run fmt, check, check-migrate**

```bash
cargo fmt --all -- --check
cargo check -p fnn --features rocksdb
make check-migrate
```

- [ ] **Step 3: Review and commit**

```bash
git add -A
git commit -m "chore: final M5 provider mode verification"
```

---

## E2E Test Suite (After M5)

After M5 tasks complete, the E2E test suite will be designed and implemented in a separate phase covering:

- Loop Out E2E: two-node, real CKB dev chain, deployed liquidity-lock, full flow
- Loop In E2E: two-node, real CKB dev chain, provider_accept_loop_in with validation
- Recovery E2E: crash-and-restart at non-terminal states
- Error E2E: wrong preimage, early refund, UDT mismatch, etc.
