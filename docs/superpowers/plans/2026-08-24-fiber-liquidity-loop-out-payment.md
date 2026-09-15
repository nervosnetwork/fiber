# Cross-Node Loop Out Payment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the cross-node Loop Out payment path end to end so the CKB/UDT Loop Out happy path reaches `Success` on both nodes through public RPC only.

**Architecture:** The provider generates a secret preimage at quote time (`payment_hash = blake2b_256(preimage)`), persists it, registers an invoice + preimage with its network actor so the incoming Fiber payment settles, and observes settlement to advance `PaymentInFlight -> PaymentSettled`. The client receives the payout outpoint from `provider_accept_loop_out`, passes it to `loop_out`, watches the payout, pays, learns the preimage, and claims.

**Tech Stack:** Rust, Ractor, ckb_types, CKB invoice (`InvoiceBuilder`), RocksDB, cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-08-24-fiber-liquidity-loop-out-payment-design.md`

---

## Key code locations (verified)

- `crates/fiber-lib/src/liquidity/types.rs:123` — `LoopOutQuoteTerms` (add `payment_preimage`).
- `crates/fiber-types/src/liquidity.rs:78` — `LoopOutQuoteRecord` (persisted; add `payment_preimage` + migration).
- `crates/fiber-lib/src/liquidity/store.rs:64` — `loop_out_quote_record_from_terms`; `:89` — `loop_out_quote_terms_from_record`; `:210` — `loop_out_quote_record_from_bytes` (bincode fallback chain).
- `crates/fiber-lib/src/liquidity/quote.rs:210` — `liquidity_quote_envelope_from_terms`; `:239` — `validate_imported_quote`.
- `crates/fiber-lib/src/liquidity/actor.rs`:
  - `:469` — `handle_provider_quote_loop_out` (uses `loop_out_quote_hash` at `:1663`).
  - `:452` — `handle_loop_out`; `:1781` — `create_client_loop_out`.
  - `:976` — `handle_provider_accept_loop_out`; `:1990` — `accept_provider_loop_out`.
  - `:773` — `handle_payout_confirmed` (provider branch `:839`).
  - `:847` — `handle_payment_settled` (provider branch `:887`).
  - `:1551` — `swap_response`.
  - `:1752` — `LoopOutPaymentAdapter` trait; `:1771` — `LoopOutPaymentStatus`.
  - `:1720` — `RecoveryAction`; `:1736` — `recovery_action_for_loop_out_state`; `:1114` — `resume_swap`.
  - `:2451` — `mark_provider_payment_settled`; `:2464` — `mark_provider_claim_observed`.
- `crates/fiber-lib/src/liquidity/payment.rs:66` — `NetworkLoopOutPaymentAdapter`; `:166` — trait impl.
- `crates/fiber-lib/src/fiber/network.rs:1031` — `NetworkActorCommand::AddInvoice`; `:1036` — `GetInvoice`; `:4064` — `add_invoice`.
- `crates/fiber-json-types/src/liquidity.rs:289` — `LoopOutParams`; `:325` — `LiquiditySwapResponse`.
- `crates/fiber-lib/src/rpc/mod.rs:350` — liquidity actor spawn.
- `crates/fiber-lib/src/lib.rs:95` — `gen_rand_sha256_hash`.
- `crates/fiber-lib/src/invoice/invoice_impl.rs:32` — `InvoiceBuilder`; `:86` — `payment_preimage`; `:143` — `build`.
- `crates/fiber-types/src/invoice.rs:283` — `Currency`; `:338` — `HashAlgorithm`.
- `crates/fiber-lib/src/fiber/config.rs:459` — `FiberConfig::currency`.

---

### Task 1: Preimage generation, payment hash, persistence, quote-id uniqueness

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/types.rs` (add field)
- Modify: `crates/fiber-types/src/liquidity.rs` (add field)
- Modify: `crates/fiber-lib/src/liquidity/store.rs` (map + migration)
- Modify: `crates/fiber-lib/src/liquidity/actor.rs` (`handle_provider_quote_loop_out`, `loop_out_quote_hash`)
- Modify: `crates/fiber-lib/src/liquidity/quote.rs` (`validate_imported_quote`)
- Test: `crates/fiber-lib/src/liquidity/actor.rs` (existing `mod tests`)

- [ ] **Step 1: Write failing tests**

In `crates/fiber-lib/src/liquidity/actor.rs` `mod tests`, add two unit tests. Use existing helpers `test_loop_out_quote`, `now_ms`, and `deterministic_provider_pubkey` where possible; add a focused test that calls the public quote handler.

```rust
#[tokio::test]
async fn provider_loop_out_quote_payment_hash_is_blake2b_of_fresh_preimage() {
    let events = Shared::new(Vec::new());
    let store = TestLiquidityStore::new(events.clone(), "provider");
    let chain = TestLiquidityChain::new(events.clone());
    let (payment, _) = TestLoopOutPayment::with_default_statuses(events.clone());
    let (actor, _handle) = spawn_test_actor(store.clone(), payment, chain).await;

    let params = ProviderQuoteLoopOutParams { /* fill required fields */ };
    let envelope = call_provider_quote_loop_out(&actor, params).await.unwrap();

    // envelope must NOT expose any preimage; assert payment_hash is a blake2b hash of a
    // non-default preimage by round-tripping through the persisted quote terms.
    let terms = store.get_loop_out_quote(&envelope.quote_id.into()).unwrap().unwrap();
    let preimage = terms.payment_preimage.expect("provider quote persists its preimage");
    assert_ne!(preimage, Hash256::default());
    let expected: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
    assert_eq!(terms.payment_hash, expected);
}

#[test]
fn two_provider_quotes_with_same_params_get_distinct_quote_ids_and_preimages() {
    // Build two quotes through the handler (or call loop_out_quote_hash twice), assert
    // quote_id differs and preimage differs. This guards quote-id uniqueness.
}
```

Because the exact test-harness helper names in this module are not part of the public API, use the existing test helpers already present in `crates/fiber-lib/src/liquidity/actor.rs` `mod tests` (e.g. `spawn_actor`, `call_provider_quote_loop_out`, `test_loop_out_quote`). Match their real signatures when writing the tests.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(provider_loop_out_quote_payment_hash_is_blake2b_of_fresh_preimage)'`
Expected: FAIL — `LoopOutQuoteTerms` has no `payment_preimage` field (compile error) or the assertion on `payment_preimage.expect(...)` fails.

- [ ] **Step 3: Add `payment_preimage` to `LoopOutQuoteTerms`**

In `crates/fiber-lib/src/liquidity/types.rs`, add after `payment_hash`:

```rust
    /// Provider-generated preimage revealed only through payment settlement.
    /// Always `None` on the client side.
    pub payment_preimage: Option<Hash256>,
```

`Hash256` is already imported in this file.

- [ ] **Step 4: Add `payment_preimage` to `LoopOutQuoteRecord` (fiber-types)**

In `crates/fiber-types/src/liquidity.rs` `LoopOutQuoteRecord`, add after `payment_hash`:

```rust
    /// Provider-generated preimage persisted for recovery. Absent on client/imported quotes.
    #[serde(default)]
    pub payment_preimage: Option<Hash256>,
```

`Hash256` is already imported.

- [ ] **Step 5: Map the field through the store**

In `crates/fiber-lib/src/liquidity/store.rs`:

In `loop_out_quote_record_from_terms`, add:
```rust
        payment_hash: quote.payment_hash,
        payment_preimage: quote.payment_preimage,
```

In `loop_out_quote_terms_from_record`, add:
```rust
        payment_hash: record.payment_hash,
        payment_preimage: record.payment_preimage,
```

- [ ] **Step 6: Add bincode migration fallback**

`LoopOutQuoteRecord` is bincode-serialized, so the new field changes its layout. The current schema (with `client_invoice`, without `payment_preimage`) must become a legacy fallback.

In `crates/fiber-lib/src/liquidity/store.rs`, add a legacy struct mirroring the CURRENT `LoopOutQuoteRecord` shape but WITHOUT `payment_preimage`, named `LoopOutQuoteRecordWithoutPreimage`:

```rust
#[serde_as]
#[derive(Deserialize, Serialize)]
struct LoopOutQuoteRecordWithoutPreimage {
    quote_id: Hash256,
    swap_kind: fiber_types::LiquiditySwapKind,
    provider: Pubkey,
    asset: LiquidityAsset,
    amount: u128,
    provider_fee: u128,
    routing_fee_limit: u128,
    onchain_fee_estimate_ckb: u64,
    capacity_requirement_ckb: u64,
    payment_hash: Hash256,
    expires_at: u64,
    payout_deadline: u64,
    refund_after_lock_time: u64,
    #[serde_as(as = "EntityHex")]
    claimant_lock: ckb_types::packed::Script,
    #[serde_as(as = "EntityHex")]
    refund_lock: ckb_types::packed::Script,
    client_invoice: Option<String>,
    created_at: u64,
}

impl From<LoopOutQuoteRecordWithoutPreimage> for fiber_types::LoopOutQuoteRecord {
    fn from(record: LoopOutQuoteRecordWithoutPreimage) -> Self {
        Self {
            quote_id: record.quote_id,
            swap_kind: record.swap_kind,
            provider: record.provider,
            asset: record.asset,
            amount: record.amount,
            provider_fee: record.provider_fee,
            routing_fee_limit: record.routing_fee_limit,
            onchain_fee_estimate_ckb: record.onchain_fee_estimate_ckb,
            capacity_requirement_ckb: record.capacity_requirement_ckb,
            payment_hash: record.payment_hash,
            payment_preimage: None,
            expires_at: record.expires_at,
            payout_deadline: record.payout_deadline,
            refund_after_lock_time: record.refund_after_lock_time,
            claimant_lock: record.claimant_lock,
            refund_lock: record.refund_lock,
            client_invoice: record.client_invoice,
            created_at: record.created_at,
        }
    }
}
```

Also update `QuoteRecordWithoutClientInvoice` and `LegacyLoopOutQuoteRecord` `From` impls to add `payment_preimage: None`.

Update the deserialize chain in `loop_out_quote_record_from_bytes` so the new legacy variant is the FIRST fallback:

```rust
    bincode::deserialize::<fiber_types::LoopOutQuoteRecord>(value)
        .or_else(|_| bincode::deserialize::<LoopOutQuoteRecordWithoutPreimage>(value).map(Into::into))
        .or_else(|_| bincode::deserialize::<QuoteRecordWithoutClientInvoice>(value).map(Into::into))
        .or_else(|_| bincode::deserialize::<LegacyLoopOutQuoteRecord>(value).map(Into::into))
        .map_err(|err| { /* unchanged */ })
```

- [ ] **Step 7: Generate preimage + hash + unique quote id in `handle_provider_quote_loop_out`**

In `crates/fiber-lib/src/liquidity/actor.rs`, replace the `LoopOutQuoteTerms { ... }` literal in `handle_provider_quote_loop_out` (around `:491`) with:

```rust
        let preimage = crate::gen_rand_sha256_hash();
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
        let quote_id: Hash256 = crate::gen_rand_sha256_hash();
        let terms = LoopOutQuoteTerms {
            quote_id,
            swap_kind: LiquiditySwapKind::LoopOut,
            provider: self.provider_pubkey,
            asset,
            amount: params.amount,
            provider_fee: validated.provider_fee,
            routing_fee_limit: validated.routing_fee_limit,
            onchain_fee_estimate_ckb: 1_000,
            capacity_requirement_ckb: 10_000,
            payment_hash,
            payment_preimage: Some(preimage),
            expires_at: validated.expires_at,
            payout_deadline: validated.expires_at.saturating_add(10_000),
            refund_after_lock_time: absolute_timestamp_since(
                validated.expires_at.saturating_add(20_000),
            )?,
            claimant_lock,
            refund_lock: self.provider_funding_lock_script.clone(),
            client_invoice: None,
        };
```

Remove the now-unused `loop_out_quote_hash` function (`:1663`) if nothing else references it (verify with a grep). Add `use fiber_types::HashAlgorithm;` if not already imported. `quote_id` is now fresh-random per quote.

- [ ] **Step 8: Keep the preimage out of the client path**

In `crates/fiber-lib/src/liquidity/quote.rs` `validate_imported_quote`, the `LoopOutQuoteTerms { ... }` literal must set `payment_preimage: None` (the client never imports a preimage). Add after `payment_hash,`:

```rust
        payment_preimage: None,
```

`liquidity_quote_envelope_from_terms` already omits it; leave unchanged.

- [ ] **Step 9: Update other `LoopOutQuoteTerms` constructors**

Every remaining `LoopOutQuoteTerms { ... }` literal in the crate (tests and helpers in `actor.rs`, `quote.rs`) must now include `payment_preimage: None` (or the appropriate value). Use `cargo check` to find and fix all sites.

- [ ] **Step 10: Run tests and commit**

Run:
```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)' --no-fail-fast
cargo fmt --all -- --check
```
Expected: all liquidity tests pass (existing tests that asserted deterministic `quote_id` may need updating to reflect fresh-random ids — update only if the test was asserting the old deterministic hash; keep the idempotency assertion that "same quote_id + same terms -> success").

Commit:
```bash
git add crates/fiber-lib/src/liquidity/types.rs crates/fiber-types/src/liquidity.rs crates/fiber-lib/src/liquidity/store.rs crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/quote.rs
git commit -m "feat: generate and persist provider loop out preimage"
```

---

### Task 2: Provider invoice registration

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs` (`LoopOutPaymentAdapter` trait, `handle_provider_accept_loop_out`)
- Modify: `crates/fiber-lib/src/liquidity/payment.rs` (`NetworkLoopOutPaymentAdapter`, trait impl)
- Modify: `crates/fiber-lib/src/rpc/mod.rs` (pass currency)
- Test: `crates/fiber-lib/src/liquidity/payment.rs` (`mod tests`)

- [ ] **Step 1: Write failing test for the adapter method**

In `crates/fiber-lib/src/liquidity/payment.rs` `mod tests`, add a test using the existing mock network actor (see `network_loop_out_payment_adapter_sends_invoice_payment_without_provider_target`) that calls a new method `register_provider_loop_out_invoice` and then asserts `GetInvoice`/`get_invoice` for the hash returns the invoice with the expected preimage.

```rust
#[tokio::test]
async fn register_provider_invoice_stores_invoice_and_preimage() {
    // Build adapter with a test network actor whose store accepts insert_invoice.
    // Call adapter.register_provider_loop_out_invoice(payment_hash, preimage, amount, None).
    // Assert the network actor's invoice store now has the invoice (via get_invoice)
    // and that invoice.payment_hash() == payment_hash.
}
```

Match the exact mock helpers already present in that `mod tests`.

- [ ] **Step 2: Verify it fails**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(register_provider_invoice_stores_invoice_and_preimage)'`
Expected: FAIL — method `register_provider_loop_out_invoice` not found on `LoopOutPaymentAdapter`.

- [ ] **Step 3: Add trait method**

In `crates/fiber-lib/src/liquidity/actor.rs`, extend `LoopOutPaymentAdapter`:

```rust
    /// Register the provider invoice + preimage so an incoming Loop Out payment settles.
    async fn register_provider_loop_out_invoice(
        &mut self,
        payment_hash: Hash256,
        preimage: Hash256,
        amount: u128,
        udt_type_script: Option<ckb_types::packed::Script>,
    ) -> Result<(), Self::Error>;
```

Add the `use fiber_types::HashAlgorithm;` and any needed imports.

- [ ] **Step 4: Add `currency` to the adapter**

In `crates/fiber-lib/src/liquidity/payment.rs`, add a `currency: Currency` field to `NetworkLoopOutPaymentAdapter`, import `Currency` from `fiber_types`. Update `new` to derive the default devnet currency for tests, and add a constructor:

```rust
    pub fn with_currency(network_actor: ActorRef<NetworkActorMessage>, currency: Currency) -> Self {
        Self::with_polling_policy_and_currency(
            network_actor,
            NetworkLoopOutPaymentPollingPolicy::default(),
            currency,
        )
    }
```

Keep `new(network_actor)` working (defaulting to `Currency::Fibd`) so existing call sites and tests compile; `with_currency` will be used from `rpc/mod.rs`.

- [ ] **Step 5: Implement `register_provider_loop_out_invoice`**

In `crates/fiber-lib/src/liquidity/payment.rs` trait impl, build the invoice and register it:

```rust
    async fn register_provider_loop_out_invoice(
        &mut self,
        payment_hash: Hash256,
        preimage: Hash256,
        amount: u128,
        udt_type_script: Option<ckb_types::packed::Script>,
    ) -> Result<(), Self::Error> {
        let mut builder = InvoiceBuilder::new(self.currency)
            .amount(Some(amount))
            .payment_preimage(preimage);
        if let Some(script) = udt_type_script {
            builder = builder.udt_type_script(script);
        }
        let invoice = builder.build().map_err(|e| e.to_string())?;
        let invoice_hash = invoice.payment_hash();
        if *invoice_hash != payment_hash {
            return Err(format!(
                "provider invoice payment hash mismatch: expected {payment_hash:?}, got {invoice_hash:?}"
            )
            .into());
        }
        let result = call!(self.network_actor, |reply| {
            NetworkActorCommand::AddInvoice(invoice.clone(), Some(preimage), reply)
        })
        .map_err(|e| e.to_string())?;
        // Idempotent: an already-registered identical invoice is success.
        match result {
            Ok(()) => Ok(()),
            Err(InvoiceError::InvoiceAlreadyExists) => Ok(()),
            Err(e) => Err(e.to_string().into()),
        }
    }
```

Import `InvoiceBuilder` (from `crate::invoice::InvoiceBuilder`), `NetworkActorCommand`, and the `InvoiceError` type. If the trait's `Error` is `String` (verify), `.into()` is fine; otherwise map to the concrete error.

- [ ] **Step 6: Register the invoice at provider accept**

In `crates/fiber-lib/src/liquidity/actor.rs` `handle_provider_accept_loop_out`, after `accept_provider_loop_out(...)` and before/after `watch_payout_lock`, register the invoice. The preimage comes from the persisted quote terms (`self.quote_terms(&quote_id)?` returns `LoopOutQuoteTerms` with `payment_preimage`). Compute `gross_amount` via `loop_out_gross_payment_amount(quote.amount, quote.provider_fee, quote.routing_fee_limit)`. For UDT pass `quote.asset.udt_type_script` mapped to `packed::Script`; for CKB pass `None`.

```rust
        let preimage = quote.payment_preimage.ok_or_else(|| {
            LiquidityLoopOutError::Store(format!(
                "provider loop out quote {quote_id:?} is missing its preimage"
            ))
        })?;
        let gross_amount =
            loop_out_gross_payment_amount(quote.amount, quote.provider_fee, quote.routing_fee_limit)?;
        let udt_type_script = quote
            .asset
            .udt_type_script
            .as_ref()
            .map(|s| s.clone().into());
        self.payment
            .register_provider_loop_out_invoice(quote.payment_hash, preimage, gross_amount, udt_type_script)
            .await
            .map_err(|e| LiquidityLoopOutError::PaymentFailed(e.to_string()))?;
```

Import `loop_out_gross_payment_amount` if needed.

- [ ] **Step 7: Pass currency from production startup**

In `crates/fiber-lib/src/rpc/mod.rs`, replace `NetworkLoopOutPaymentAdapter::new(network_actor)` with `NetworkLoopOutPaymentAdapter::with_currency(network_actor, fiber_config.currency())`. `fiber_config` is in scope there.

- [ ] **Step 8: Run tests and commit**

Run:
```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity::payment)'
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)'
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
cargo fmt --all -- --check
```
Expected: pass.

Commit:
```bash
git add crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/payment.rs crates/fiber-lib/src/rpc/mod.rs
git commit -m "feat: register provider loop out invoice with preimage"
```

---

### Task 3: Payout outpoint propagation

**Files:**
- Modify: `crates/fiber-json-types/src/liquidity.rs` (`LiquiditySwapResponse`, `LoopOutParams`)
- Modify: `crates/fiber-lib/src/liquidity/actor.rs` (`swap_response`, `handle_provider_accept_loop_out`, `handle_loop_out`, `create_client_loop_out`)
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs` (params/response mapping, if a conversion layer exists)
- Test: `crates/fiber-lib/src/liquidity/actor.rs` (`mod tests`)

- [ ] **Step 1: Write failing test**

In `actor.rs` `mod tests`, add a test that after `provider_accept_loop_out`, the returned `LiquiditySwapResponse.payout_outpoint` is `Some(outpoint)` matching the persisted provider swap's `onchain_outpoint`.

```rust
#[tokio::test]
async fn provider_accept_loop_out_returns_payout_outpoint() {
    // Accept a loop out quote via call_provider_accept_loop_out, then assert
    // response.payout_outpoint == Some(expected_outpoint) where expected comes from
    // the store's provider swap onchain_outpoint.
}
```

- [ ] **Step 2: Verify it fails**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(provider_accept_loop_out_returns_payout_outpoint)'`
Expected: FAIL — `LiquiditySwapResponse` has no `payout_outpoint` field.

- [ ] **Step 3: Add field to `LiquiditySwapResponse`**

In `crates/fiber-json-types/src/liquidity.rs`, add to `LiquiditySwapResponse`:

```rust
    /// Payout lock outpoint produced by the provider accept (Loop Out provider only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_outpoint: Option<OutPoint>,
```

Import `OutPoint` (already imported in this file for other types). Add it to the JSON schema derive as appropriate (it is already `#[derive(JsonSchema)]`).

- [ ] **Step 4: Add field to `LoopOutParams`**

```rust
    /// Payout lock outpoint returned by `provider_accept_loop_out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_outpoint: Option<OutPoint>,
```

- [ ] **Step 5: Populate the response**

In `crates/fiber-lib/src/liquidity/actor.rs` `swap_response`, map the internal `swap.onchain_outpoint` (a `packed::OutPoint`) to the JSON `OutPoint` type:

```rust
        Ok(LiquiditySwapResponse {
            swap_id: swap.swap_id.into(),
            state: format!("{:?}", swap.state),
            payment_hash: swap.payment_hash.into(),
            payout_outpoint: swap.onchain_outpoint.clone().map(Into::into),
            created_at: swap.created_at,
        })
```

Verify the internal `LiquiditySwapRecord` exposes `onchain_outpoint: Option<packed::OutPoint>` (it does, per existing usage at `:1125`). Confirm the JSON `OutPoint` is `ckb_jsonrpc_types::OutPoint` and that `packed::OutPoint -> ckb_jsonrpc_types::OutPoint` implements `From`; if not, construct it via `OutPoint { tx_hash, index }`.

- [ ] **Step 6: Consume the outpoint on the client**

In `handle_loop_out`, `params.payout_outpoint` is the JSON outpoint. Convert to `packed::OutPoint` and set it on the client swap before watching. Update `create_client_loop_out` to accept the outpoint and persist it.

Change `create_client_loop_out` signature to accept `payout_outpoint: Option<packed::OutPoint>` and set `onchain_outpoint` on the client swap record via `update_liquidity_swap` (reuse the `LiquiditySwapUpdate { onchain_outpoint: .., ..Default::default() }` pattern used at `:2026`).

In `handle_loop_out`:

```rust
        let payout_outpoint = params.payout_outpoint.map(|o| packed_outpoint_from_json(o));
        let swap_id = create_client_loop_out(&self.store, quote.clone(), now_ms, payout_outpoint)?;
```

Add a helper `packed_outpoint_from_json` (or use `Into`/`From` if present) converting `ckb_jsonrpc_types::OutPoint` -> `ckb_types::packed::OutPoint`.

- [ ] **Step 7: Update RPC mapping if needed**

If `crates/fiber-lib/src/rpc/liquidity.rs` or `fiber-json-types` has an explicit `From`/`TryFrom` between JSON `LoopOutParams`/`LiquiditySwapResponse` and internal types, update those conversions to carry `payout_outpoint`. Run `cargo check` to find every construction site of `LiquiditySwapResponse` and `LoopOutParams` and fix them (adding `payout_outpoint: None` where not applicable, e.g. Loop In).

- [ ] **Step 8: Run tests and commit**

Run:
```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)'
cargo nextest run -p fiber-json-types -E 'test(liquidity)'
cargo fmt --all -- --check
```
Expected: pass.

Commit:
```bash
git add crates/fiber-json-types/src/liquidity.rs crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/rpc/liquidity.rs
git commit -m "feat: propagate payout outpoint to client loop out"
```

---

### Task 4: Provider incoming-payment observation and state flow

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs` (`handle_payout_confirmed` provider branch, `LoopOutPaymentAdapter`, recovery, `reconcile_provider_loop_out_payment`)
- Modify: `crates/fiber-lib/src/liquidity/payment.rs` (adapter method `reload_provider_loop_out_payment`)
- Test: `crates/fiber-lib/src/liquidity/actor.rs` (`mod tests`)

- [ ] **Step 1: Write failing test**

Add a test that, after the provider payout is confirmed, the actor transitions to `PaymentInFlight` and begins observing the incoming payment; then when the incoming payment settles, the swap reaches `PaymentSettled`.

```rust
#[tokio::test]
async fn provider_observes_incoming_payment_settlement_after_payout() {
    // Confirm payout -> assert state PaymentInFlight and a payment observation job was spawned.
    // Simulate invoice Paid -> send LiquidityActorMessage::PaymentSettled(swap_id, preimage)
    // -> assert state PaymentSettled.
}
```

Use the existing `confirm_payout`, `call_payment_settled`, and event helpers in `mod tests`.

- [ ] **Step 2: Verify it fails**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(provider_observes_incoming_payment_settlement_after_payout)'`
Expected: FAIL — after payout confirm, provider stays `PayoutLocked` (no observation job, no `PaymentInFlight`).

- [ ] **Step 3: Add adapter reload method**

In `crates/fiber-lib/src/liquidity/actor.rs`, add to `LoopOutPaymentAdapter`:

```rust
    /// Poll the registered provider invoice and classify incoming payment settlement.
    async fn reload_provider_loop_out_payment(
        &mut self,
        payment_hash: Hash256,
    ) -> Result<LoopOutPaymentStatus, Self::Error>;
```

In `crates/fiber-lib/src/liquidity/payment.rs` impl, poll `NetworkActorCommand::GetInvoice` and map `CkbInvoiceStatus::Paid` to `Settled` (the preimage is the provider's own, known value), `Open | Received` to `InFlight`, and `Cancelled | Expired` to `Failed`:

```rust
    async fn reload_provider_loop_out_payment(
        &mut self,
        payment_hash: Hash256,
    ) -> Result<LoopOutPaymentStatus, Self::Error> {
        let result = call!(self.network_actor, |reply| {
            NetworkActorCommand::GetInvoice(payment_hash, reply)
        })
        .map_err(|e| e.to_string())?;
        match result {
            Ok((_, status)) => match status {
                CkbInvoiceStatus::Paid => Ok(LoopOutPaymentStatus::Settled(payment_hash)),
                CkbInvoiceStatus::Open | CkbInvoiceStatus::Received => {
                    Ok(LoopOutPaymentStatus::InFlight)
                }
                CkbInvoiceStatus::Cancelled | CkbInvoiceStatus::Expired => {
                    Ok(LoopOutPaymentStatus::Failed(format!("invoice {status:?}")))
                }
            },
            Err(e) => Err(e.to_string().into()),
        }
    }
```

Import `CkbInvoiceStatus`.

- [ ] **Step 4: Observe incoming payment after provider payout confirmed**

In `crates/fiber-lib/src/liquidity/actor.rs` `handle_payout_confirmed` provider branch (`:839`), after `mark_provider_payout_locked`, mark the swap `PaymentInFlight` (via a new helper `mark_provider_payment_in_flight` mirroring `mark_provider_payment_settled`'s first transition) and spawn a job that polls `reload_provider_loop_out_payment` until settled/failed, sending `LiquidityActorMessage::PaymentSettled(swap_id, preimage)` on settle.

Add helper:

```rust
pub fn mark_provider_payment_in_flight<S>(
    store: &S,
    swap_id: Hash256,
    now_ms: u64,
) -> Result<(), LiquidityLoopOutError>
where
    S: LiquidityStore,
{
    transition_swap(store, &swap_id, LiquiditySwapState::PaymentInFlight, now_ms)
}
```

Add an async helper `observe_provider_loop_out_payment` (free function) analogous to `reconcile_loop_out_payment`:

```rust
async fn observe_provider_loop_out_payment<P>(
    store: impl LiquidityStore + Send + Sync + 'static,
    mut payment: P,
    myself: ActorRef<LiquidityActorMessage>,
    swap_id: Hash256,
    preimage: Hash256,
    payment_hash: Hash256,
) where
    P: LoopOutPaymentAdapter + Send + 'static,
    P::Error: Display,
{
    // Poll payment.reload_provider_loop_out_payment(payment_hash) at LOOP_OUT_PAYMENT_RECONCILE_INTERVAL.
    // On Settled -> send PaymentSettled(swap_id, preimage).
    // On Failed  -> persist_loop_out_payment_failure_context and send recovery-finished (so refund path
    //               or failure can proceed).
    // On InFlight -> sleep and retry, bounded by the existing reconcile loop.
}
```

Model it on the existing `reconcile_loop_out_payment` at `:1578`.

In the provider branch of `handle_payout_confirmed`, insert the `active_payment_swaps` guard, `mark_provider_payment_in_flight`, and spawn the observation job (tracked via `spawn_job`, using `self.payment.clone()`, `self.store.clone()`, `myself.clone()`).

- [ ] **Step 5: Wire recovery**

Update `resume_swap` (`:1114`):
- For provider swaps in `PayoutLocked` or `PaymentInFlight` (the current `ResumePayment`/`ReloadPayment` provider branches), spawn the same `observe_provider_loop_out_payment` job (fetching the persisted `preimage` from `quote_terms(&swap.quote_id)?.payment_preimage`) instead of re-watching the payout.
- Keep the `active_payment_swaps` dedupe guard.

- [ ] **Step 6: Re-register invoice idempotently on recovery**

Because `register_provider_loop_out_invoice` treats `InvoiceAlreadyExists` as success, call it during the provider's recovery path (before observing) to guarantee the invoice+preimage is present after a restart. Add this to the provider `ResumePayment`/`ReloadPayment` recovery branch.

- [ ] **Step 7: Run tests and commit**

Run:
```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)' --no-fail-fast
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
cargo fmt --all -- --check
```
Expected: pass.

Commit:
```bash
git add crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/payment.rs
git commit -m "feat: observe provider loop out incoming payment"
```

---

### Task 5: Turn the E2E happy path GREEN and verify

**Files:**
- Modify: `crates/fiber-lib/src/tests/liquidity_integration.rs` (already present, uncommitted RED test)
- Modify: `crates/fiber-lib/src/tests/liquidity_test_utils.rs` (as needed)

- [ ] **Step 1: Update the E2E test to pass the payout outpoint**

The existing RED test `liquidity_ckb_loop_out_e2e` calls `provider_accept_loop_out` then `loop_out`. Update it to capture `payout_outpoint` from the accept response and pass it into `LoopOutParams`.

- [ ] **Step 2: Run the E2E test**

Run: `cargo nextest run -p fnn --features rocksdb -E 'test(liquidity_ckb_loop_out_e2e)'`
Expected: PASS, reaching `Success` on both nodes with payment settlement and claim confirmation via public RPC.

- [ ] **Step 3: Full verification**

Run:
```bash
cargo nextest run -p fnn --features rocksdb -E 'test(liquidity)' --no-fail-fast
cargo nextest run -p fnn --features rocksdb -E 'test(rpc::liquidity)'
cargo nextest run -p fiber-json-types -E 'test(liquidity)'
cargo check --locked -p fnn --features rocksdb
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
cargo fmt --all -- --check
make check-migrate
git diff --check
```
Expected: all pass; `make check-migrate` may require updating `.schema.json` (run `make update-migrate-check` if prompted and review the diff).

- [ ] **Step 4: Commit**

```bash
git add crates/fiber-lib/src/tests/liquidity_integration.rs crates/fiber-lib/src/tests/liquidity_test_utils.rs crates/fiber-lib/src/tests/mod.rs
git commit -m "test: add CKB Loop Out integration flow"
```

---

## Self-review notes

- Spec coverage: Task 1 covers preimage + payment hash + persistence + quote-id uniqueness; Task 2 covers invoice registration; Task 3 covers outpoint propagation; Task 4 covers incoming-payment observation + recovery; Task 5 turns the E2E test GREEN. All spec sections map to a task.
- Type consistency: `payment_preimage: Option<Hash256>` used consistently across `LoopOutQuoteTerms`, `LoopOutQuoteRecord`, and the legacy `From` impls. `payout_outpoint: Option<OutPoint>` (JSON `ckb_jsonrpc_types::OutPoint`) is converted to `packed::OutPoint` on the client side.
- The exact test-helper names inside `actor.rs`/`payment.rs` `mod tests` vary; implementers must match the real helper signatures already present rather than inventing new ones.
