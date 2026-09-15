# Fiber Liquidity M3 Runtime Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make M3 Manual Loop Out usable at runtime by wiring mutation RPCs to a real Ractor `LiquidityActor`, real Fiber payment calls, and a liquidity-specific CKB watcher boundary while preserving restart-safe ordering.

**Architecture:** `LiquidityActor` owns all mutation workflows and recovery; persisted `LiquiditySwapRecord` remains the source of truth. RPC handlers become thin actor callers, while read-only RPCs remain store-backed. Payment and chain integrations live behind liquidity-specific adapters so actor tests can prove ordering without depending on live CKB or routing.

**Tech Stack:** Rust 1.93, Ractor, jsonrpsee, existing Fiber `NetworkActorMessage`, existing CKB actors, RocksDB-backed `LiquidityStore`, cargo-nextest/cargo test.

---

## File Structure

- Modify `crates/fiber-lib/src/liquidity/actor.rs`: replace placeholder message enum with reply-port actor messages, add `LiquidityActor`, actor state, command/reply structs, recovery scheduling, and deterministic actor tests.
- Modify `crates/fiber-lib/src/liquidity/payment.rs`: add runtime `NetworkLoopOutPaymentAdapter` over `NetworkActorMessage::Command(NetworkActorCommand::SendPayment/GetPayment)` and tests with a mock network actor.
- Modify `crates/fiber-lib/src/liquidity/chain.rs`: add `LiquidityChainWatcher` trait, transaction identity types, runtime CKB-backed watcher skeleton, and idempotency tests with a mock watcher.
- Modify `crates/fiber-lib/src/rpc/liquidity.rs`: accept a `LiquidityActor` ref, delegate mutation RPCs through `handle_actor_call!`, keep `get_swap` and `list_swaps` store-backed, and update tests.
- Modify `crates/fiber-lib/src/rpc/mod.rs`: spawn or inject the liquidity actor when the liquidity module is enabled and network/CKB actors are available; wire actor ref into `LiquidityRpcServerImpl`.
- Modify `crates/fiber-lib/src/lib.rs` or module exports only if the actor/adapters need public construction from startup wiring.
- Modify `crates/fiber-json-types/src/liquidity.rs` only if actor replies require missing JSON fields; prefer existing DTOs.
- Modify `crates/fiber-lib/src/rpc/README.md` only if RPC docs change after `make gen-rpc-doc`.

## Task 1: Actor Message Boundary And Reply Types

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing tests for reply-port messages and recovery command coverage**

Add tests near the existing `#[cfg(test)] mod tests` in `crates/fiber-lib/src/liquidity/actor.rs`:

```rust
#[test]
fn actor_message_names_cover_m3_mutations_and_recovery() {
    let names = LiquidityActorMessage::variant_names();

    assert_eq!(
        names,
        &[
            "quote_loop_out",
            "loop_out",
            "provider_quote_loop_out",
            "provider_accept_loop_out",
            "resume_non_terminal",
            "payout_confirmed",
            "payment_settled",
            "claim_confirmed",
            "provider_claim_observed",
            "refund_confirmed",
        ]
    );
}

#[test]
fn resume_non_terminal_action_is_explicit_actor_message() {
    assert!(LiquidityActorMessage::variant_names().contains(&"resume_non_terminal"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb actor_message_names_cover_m3_mutations_and_recovery resume_non_terminal_action_is_explicit_actor_message -- --nocapture`

Expected: FAIL because `LiquidityActorMessage::variant_names` and the runtime variants do not exist.

- [ ] **Step 3: Replace the placeholder message enum with runtime variants**

In `crates/fiber-lib/src/liquidity/actor.rs`, add `RpcReplyPort` and JSON DTO imports:

```rust
use fiber_json_types::{
    LiquidityQuoteResponse, LiquiditySwapResponse, LoopOutParams, ProviderAcceptLoopOutParams,
    ProviderQuoteLoopOutParams, QuoteLoopOutParams,
};
use ractor::RpcReplyPort;
```

Replace the existing `LiquidityActorMessage` enum with:

```rust
/// Messages accepted by the liquidity actor boundary.
#[derive(Debug)]
pub enum LiquidityActorMessage {
    /// Client-side request for a provider Loop Out quote.
    QuoteLoopOut(
        QuoteLoopOutParams,
        RpcReplyPort<Result<LiquidityQuoteResponse, LiquidityLoopOutError>>,
    ),
    /// Client-side acceptance/execution of a Loop Out quote.
    LoopOut(
        LoopOutParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Provider-side quote request.
    ProviderQuoteLoopOut(
        ProviderQuoteLoopOutParams,
        RpcReplyPort<Result<LiquidityQuoteResponse, LiquidityLoopOutError>>,
    ),
    /// Provider-side quote acceptance.
    ProviderAcceptLoopOut(
        ProviderAcceptLoopOutParams,
        RpcReplyPort<Result<LiquiditySwapResponse, LiquidityLoopOutError>>,
    ),
    /// Resume every persisted non-terminal Loop Out swap.
    ResumeNonTerminal(RpcReplyPort<Result<usize, LiquidityLoopOutError>>),
    /// Internal continuation after payout lock confirmation.
    PayoutConfirmed(Hash256),
    /// Internal continuation after payment settlement.
    PaymentSettled(Hash256, Hash256),
    /// Internal continuation after client claim confirmation.
    ClaimConfirmed(Hash256),
    /// Internal continuation after provider observes the client claim.
    ProviderClaimObserved(Hash256),
    /// Internal continuation after provider refund confirmation.
    RefundConfirmed(Hash256),
}

impl LiquidityActorMessage {
    /// Stable variant names used by tests and RPC wiring assertions.
    pub fn variant_names() -> &'static [&'static str] {
        &[
            "quote_loop_out",
            "loop_out",
            "provider_quote_loop_out",
            "provider_accept_loop_out",
            "resume_non_terminal",
            "payout_confirmed",
            "payment_settled",
            "claim_confirmed",
            "provider_claim_observed",
            "refund_confirmed",
        ]
    }
}
```

Remove or migrate old `QuoteLoopOutCommand`, `AcceptLoopOutCommand`, and `ResumeLoopOut` usages only where the compiler requires it. Keep helper structs if existing tests still use them.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p fnn --features rocksdb actor_message_names_cover_m3_mutations_and_recovery resume_non_terminal_action_is_explicit_actor_message -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: define liquidity actor message boundary"
```

## Task 2: Actor State And Mutation Workflow Harness

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing actor orchestration tests**

Add tests proving actor-owned mutations preserve existing ordering:

```rust
#[tokio::test]
async fn liquidity_actor_loop_out_persists_before_payment_and_claim() {
    let harness = RuntimeActorHarness::new_client();
    let quote = harness.loop_out_quote_terms();
    harness.store_quote(quote.clone());

    let response = harness.call_loop_out(quote.quote_id).await.unwrap();

    assert_eq!(response.swap_id, quote.quote_id.into());
    assert_eq!(
        harness.events(),
        vec![
            "client_insert_created",
            "client_transition_quoted",
            "client_transition_payout_pending",
            "watch_payout",
            "client_transition_payout_locked",
            "client_transition_payment_in_flight",
            "send_payment",
            "client_persist_preimage",
            "client_transition_payment_settled",
            "client_transition_claim_pending",
            "broadcast_claim",
            "watch_claim",
        ]
    );
}

#[tokio::test]
async fn liquidity_actor_provider_accept_persists_outpoint_before_broadcast() {
    let harness = RuntimeActorHarness::new_provider();
    let quote = harness.loop_out_quote_terms();
    harness.store_quote(quote.clone());

    let response = harness.call_provider_accept(quote.quote_id).await.unwrap();

    assert_eq!(response.swap_id, quote.quote_id.into());
    assert_eq!(
        harness.events(),
        vec![
            "provider_insert_created",
            "provider_transition_quoted",
            "provider_transition_payout_pending",
            "reserve_payout",
            "provider_persist_outpoint",
            "broadcast_payout",
            "watch_payout",
        ]
    );
}
```

Implement the `RuntimeActorHarness` inside the test module with the existing `TestLiquidityStore`, fake payment adapter, fake chain watcher, and `ractor::call` to the actor. Use the same deterministic quote builders already present in the actor tests.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb liquidity_actor_loop_out_persists_before_payment_and_claim liquidity_actor_provider_accept_persists_outpoint_before_broadcast -- --nocapture`

Expected: FAIL because `LiquidityActor` does not exist.

- [ ] **Step 3: Implement minimal `LiquidityActor` and actor state**

Add these runtime types in `crates/fiber-lib/src/liquidity/actor.rs`:

```rust
use async_trait::async_trait;
use ractor::{Actor, ActorProcessingErr, ActorRef};

/// Runtime dependencies for `LiquidityActor`.
pub struct LiquidityActorArguments<S, P, C> {
    pub store: S,
    pub payment: P,
    pub chain: C,
}

/// Durable mutation actor for liquidity workflows.
pub struct LiquidityActor<S, P, C>(std::marker::PhantomData<(S, P, C)>);

/// Mutable runtime state held by `LiquidityActor`.
pub struct LiquidityActorState<S, P, C> {
    store: S,
    payment: P,
    chain: C,
}

#[async_trait]
impl<S, P, C> Actor for LiquidityActor<S, P, C>
where
    S: LiquidityStore + Send + Sync + Clone + 'static,
    P: LoopOutPaymentAdapter + Send + 'static,
    P::Error: Display + Send,
    C: LoopOutChainAdapter + Send + 'static,
    C::Error: Display + Send,
{
    type Msg = LiquidityActorMessage;
    type State = LiquidityActorState<S, P, C>;
    type Arguments = LiquidityActorArguments<S, P, C>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(LiquidityActorState {
            store: args.store,
            payment: args.payment,
            chain: args.chain,
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            LiquidityActorMessage::LoopOut(params, reply) => {
                let result = state.handle_loop_out(params).await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ProviderAcceptLoopOut(params, reply) => {
                let result = state.handle_provider_accept_loop_out(params).await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::ResumeNonTerminal(reply) => {
                let result = state.resume_non_terminal().await;
                let _ = reply.send(result);
            }
            LiquidityActorMessage::PayoutConfirmed(swap_id) => {
                state.handle_payout_confirmed(swap_id).await?;
            }
            LiquidityActorMessage::PaymentSettled(swap_id, preimage) => {
                state.handle_payment_settled(swap_id, preimage).await?;
            }
            LiquidityActorMessage::ClaimConfirmed(swap_id) => {
                mark_client_claim_confirmed(&state.store, swap_id, now_ms())
                    .map_err(|error| ActorProcessingErr::from(error.to_string()))?;
            }
            LiquidityActorMessage::ProviderClaimObserved(swap_id) => {
                mark_provider_claim_observed(&state.store, swap_id, now_ms())
                    .map_err(|error| ActorProcessingErr::from(error.to_string()))?;
            }
            LiquidityActorMessage::RefundConfirmed(_swap_id) => {}
            LiquidityActorMessage::QuoteLoopOut(_params, reply) => {
                let _ = reply.send(Err(LiquidityLoopOutError::Store(
                    "client quote delegation is wired in a later task".to_string(),
                )));
            }
            LiquidityActorMessage::ProviderQuoteLoopOut(_params, reply) => {
                let _ = reply.send(Err(LiquidityLoopOutError::Store(
                    "provider quote delegation is wired in a later task".to_string(),
                )));
            }
        }
        Ok(())
    }
}
```

Add `LiquidityActorState` methods that call the existing pure helpers. Use existing quote reconstruction helpers if present; if the accepted quote terms are not persisted yet, add a small in-memory test-only quote resolver in the harness and keep production handler returning a clear store error until Task 3 adds durable quote lookup.

Add:

```rust
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
```

- [ ] **Step 4: Run actor tests**

Run: `cargo test -p fnn --features rocksdb liquidity_actor_loop_out_persists_before_payment_and_claim liquidity_actor_provider_accept_persists_outpoint_before_broadcast -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: add liquidity runtime actor harness"
```

## Task 3: Durable Quote Lookup And Provider Quoting

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Modify: `crates/fiber-lib/src/liquidity/store.rs` only if no quote lookup/reservation API exists
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing quote tests**

Add tests:

```rust
#[tokio::test]
async fn provider_quote_loop_out_validates_asset_and_returns_terms() {
    let harness = RuntimeActorHarness::new_provider_with_asset();

    let quote = harness
        .call_provider_quote(ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: 1000,
            receiver: "ckt1receiver".to_string(),
            max_provider_fee: 100,
            max_routing_fee: 50,
            expires_after_seconds: 60,
        })
        .await
        .unwrap();

    assert_eq!(quote.asset_id, "ckb");
    assert_eq!(quote.amount, 1000);
    assert!(quote.provider_fee <= 100);
    assert!(quote.routing_fee_limit <= 50);
}

#[tokio::test]
async fn loop_out_rejects_unknown_quote_id_before_side_effects() {
    let harness = RuntimeActorHarness::new_client();

    let error = harness.call_loop_out([9u8; 32].into()).await.unwrap_err();

    assert!(error.to_string().contains("quote"));
    assert!(harness.events().is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb provider_quote_loop_out_validates_asset_and_returns_terms loop_out_rejects_unknown_quote_id_before_side_effects -- --nocapture`

Expected: FAIL because actor quote handlers are not implemented.

- [ ] **Step 3: Implement durable quote lookup with the smallest existing storage surface**

Inspect `crates/fiber-lib/src/liquidity/store.rs` for existing quote storage added in M3. If quote terms are already represented by a swap record plus asset registry, add local reconstruction helpers in `actor.rs`:

```rust
fn quote_terms_from_record(record: LiquiditySwapRecord) -> Result<LoopOutQuoteTerms, LiquidityLoopOutError> {
    Err(LiquidityLoopOutError::Store(format!(
        "accepted quote terms are incomplete for swap {:?}",
        record.swap_id
    )))
}
```

Then replace the error body with actual field mapping from existing persisted quote terms. If the store does not persist all fields required by `LoopOutQuoteTerms`, add focused fields or a `LiquidityQuoteRecord` API in `store.rs` instead of opaque JSON. The API shape must be:

```rust
fn insert_loop_out_quote(&self, quote: LoopOutQuoteTerms, created_at: u64) -> Result<(), LiquidityStoreError>;
fn get_loop_out_quote(&self, quote_id: &Hash256) -> Result<Option<LoopOutQuoteTerms>, LiquidityStoreError>;
```

Implement provider quote handler by reusing existing quote validation helpers from `crates/fiber-lib/src/liquidity/quote.rs`. Return `LiquidityQuoteResponse` using the existing conversion code or add:

```rust
fn quote_response_from_terms(terms: LoopOutQuoteTerms) -> LiquidityQuoteResponse {
    LiquidityQuoteResponse {
        quote_id: terms.quote_id.into(),
        swap_kind: fiber_json_types::LiquiditySwapKind::LoopOut,
        asset_id: terms.asset.asset_id,
        amount: terms.amount,
        provider_fee: terms.provider_fee,
        routing_fee_limit: terms.routing_fee_limit,
        onchain_fee_estimate_ckb: terms.onchain_fee_estimate_ckb,
        capacity_requirement_ckb: terms.capacity_requirement_ckb,
        payment_hash: terms.payment_hash.into(),
        expires_at: terms.expires_at,
        payout_deadline: Some(terms.payout_deadline),
        refund_after_lock_time: terms.refund_after_lock_time,
    }
}
```

- [ ] **Step 4: Run quote tests and existing store tests**

Run: `cargo test -p fnn --features rocksdb provider_quote_loop_out_validates_asset_and_returns_terms loop_out_rejects_unknown_quote_id_before_side_effects test_store_liquidity -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/store.rs
git commit -m "feat: persist liquidity loop out quotes"
```

## Task 4: Real Fiber Payment Adapter

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/payment.rs`
- Test: `crates/fiber-lib/src/liquidity/payment.rs`

- [ ] **Step 1: Write failing adapter tests**

Add tests that spawn a mock network actor and assert the adapter sends the existing payment command:

```rust
#[tokio::test]
async fn network_loop_out_payment_adapter_sends_existing_network_payment_command() {
    let network = spawn_payment_mock(NetworkPaymentMockMode::Settle([7u8; 32].into())).await;
    let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

    let preimage = adapter
        .send_loop_out_payment(LoopOutPaymentRequest::new([3u8; 32].into(), 100, 2, 5).unwrap())
        .unwrap();

    assert_eq!(preimage, [7u8; 32].into());
    assert_eq!(network.take_events(), vec!["send_payment"]);
}

#[tokio::test]
async fn network_loop_out_payment_adapter_reloads_settled_payment() {
    let network = spawn_payment_mock(NetworkPaymentMockMode::ReloadSettled([8u8; 32].into())).await;
    let adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

    let preimage = adapter.reload_settled_preimage([3u8; 32].into()).await.unwrap();

    assert_eq!(preimage, Some([8u8; 32].into()));
    assert_eq!(network.take_events(), vec!["get_payment"]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb network_loop_out_payment_adapter_sends_existing_network_payment_command network_loop_out_payment_adapter_reloads_settled_payment -- --nocapture`

Expected: FAIL because `NetworkLoopOutPaymentAdapter` does not exist.

- [ ] **Step 3: Implement the network-backed adapter**

Add to `crates/fiber-lib/src/liquidity/payment.rs`:

```rust
use ractor::{call, ActorRef};

use crate::fiber::network::SendPaymentResponse;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};

/// Payment adapter that sends Loop Out payments through the existing Fiber network actor.
#[derive(Clone)]
pub struct NetworkLoopOutPaymentAdapter {
    network_actor: ActorRef<NetworkActorMessage>,
}

impl NetworkLoopOutPaymentAdapter {
    pub fn new(network_actor: ActorRef<NetworkActorMessage>) -> Self {
        Self { network_actor }
    }

    pub async fn reload_settled_preimage(
        &self,
        payment_hash: Hash256,
    ) -> Result<Option<Hash256>, LiquidityLoopOutError> {
        let response = call!(self.network_actor, |reply| {
            NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, reply))
        })
        .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?
        .map_err(LiquidityLoopOutError::PaymentFailed)?;

        settled_preimage_from_response(response)
    }
}

fn settled_preimage_from_response(
    response: SendPaymentResponse,
) -> Result<Option<Hash256>, LiquidityLoopOutError> {
    if response.status == fiber_types::PaymentStatus::Success {
        return response.preimage.ok_or_else(|| {
            LiquidityLoopOutError::PaymentFailed("settled payment is missing preimage".to_string())
        }).map(Some);
    }
    Ok(None)
}
```

If `SendPaymentResponse` uses a different field name for preimage, use that exact existing field. Implement `LoopOutPaymentAdapter` for a blocking wrapper only if the trait remains synchronous; otherwise change `LoopOutPaymentAdapter` to async and update actor helpers in the same commit.

The send command must build `SendPaymentCommand` with:

```rust
SendPaymentCommand {
    target_pubkey: None,
    amount: Some(request.amount),
    payment_hash: Some(request.payment_hash),
    final_tlc_expiry_delta: None,
    tlc_expiry_limit: None,
    invoice: None,
    timeout: None,
    max_fee_amount: Some(request.max_fee_amount),
    max_fee_rate: None,
    max_parts: None,
    keysend: false,
    udt_type_script: None,
    allow_self_payment: false,
    custom_records: None,
    hop_hints: None,
    dry_run: false,
}
```

- [ ] **Step 4: Run payment adapter tests**

Run: `cargo test -p fnn --features rocksdb network_loop_out_payment_adapter_sends_existing_network_payment_command network_loop_out_payment_adapter_reloads_settled_payment -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/payment.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: send loop out payments through network actor"
```

## Task 5: Liquidity Chain Watcher Boundary

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/chain.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/chain.rs`

- [ ] **Step 1: Write failing idempotency tests**

Add tests:

```rust
#[test]
fn chain_watcher_broadcast_plan_reuses_persisted_outpoint() {
    let outpoint = test_outpoint(42);
    let record = test_swap_record_with_outpoint(outpoint.clone());

    let action = ChainResumeAction::for_payout_pending(&record).unwrap();

    assert_eq!(action, ChainResumeAction::WatchPayout(outpoint));
}

#[test]
fn chain_watcher_refuses_claim_without_preimage() {
    let record = test_swap_record_without_preimage();

    let error = LoopOutClaimPlan::from_record(&record).unwrap_err();

    assert!(error.to_string().contains("preimage"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb chain_watcher_broadcast_plan_reuses_persisted_outpoint chain_watcher_refuses_claim_without_preimage -- --nocapture`

Expected: FAIL because watcher planning types do not exist.

- [ ] **Step 3: Implement watcher planning and trait methods**

Add to `crates/fiber-lib/src/liquidity/chain.rs`:

```rust
use fiber_types::Hash256;

use crate::liquidity::store::LiquiditySwapRecord;
use crate::liquidity::types::LiquidityLoopOutError;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ChainResumeAction {
    WatchPayout(ckb_types::packed::OutPoint),
    BroadcastPayout,
    WatchClaim(ckb_types::packed::OutPoint),
    BroadcastClaim(LoopOutClaimPlan),
    WatchRefund(ckb_types::packed::OutPoint),
}

impl ChainResumeAction {
    pub fn for_payout_pending(
        record: &LiquiditySwapRecord,
    ) -> Result<Self, LiquidityLoopOutError> {
        match record.onchain_outpoint.clone() {
            Some(outpoint) => Ok(Self::WatchPayout(outpoint)),
            None => Ok(Self::BroadcastPayout),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LoopOutClaimPlan {
    pub swap_id: Hash256,
    pub payment_preimage: Hash256,
}

impl LoopOutClaimPlan {
    pub fn from_record(record: &LiquiditySwapRecord) -> Result<Self, LiquidityLoopOutError> {
        let payment_preimage = record.payment_preimage.ok_or_else(|| {
            LiquidityLoopOutError::Chain("cannot claim loop out payout without preimage".to_string())
        })?;
        Ok(Self {
            swap_id: record.swap_id,
            payment_preimage,
        })
    }
}

pub trait LiquidityChainWatcher {
    type Error;

    fn reserve_payout_lock_outpoint(
        &mut self,
        quote: &crate::liquidity::types::LoopOutQuoteTerms,
    ) -> Result<ckb_types::packed::OutPoint, Self::Error>;

    fn broadcast_payout_lock(
        &mut self,
        quote: &crate::liquidity::types::LoopOutQuoteTerms,
        outpoint: &ckb_types::packed::OutPoint,
    ) -> Result<(), Self::Error>;

    fn watch_payout_lock(&mut self, outpoint: &ckb_types::packed::OutPoint) -> Result<(), Self::Error>;

    fn broadcast_claim(&mut self, plan: LoopOutClaimPlan) -> Result<(), Self::Error>;

    fn watch_claim(&mut self, outpoint: &ckb_types::packed::OutPoint) -> Result<(), Self::Error>;

    fn broadcast_refund(&mut self, outpoint: &ckb_types::packed::OutPoint) -> Result<(), Self::Error>;

    fn watch_refund(&mut self, outpoint: &ckb_types::packed::OutPoint) -> Result<(), Self::Error>;
}
```

Add the runtime CKB-backed watcher skeleton used by startup wiring:

```rust
use ractor::ActorRef;

use crate::ckb::actor::CkbChainMessage;

#[derive(Clone)]
pub struct CkbLiquidityChainWatcher {
    ckb_chain_actor: ActorRef<CkbChainMessage>,
}

impl CkbLiquidityChainWatcher {
    pub fn new(ckb_chain_actor: ActorRef<CkbChainMessage>) -> Self {
        Self { ckb_chain_actor }
    }
}

impl LiquidityChainWatcher for CkbLiquidityChainWatcher {
    type Error = LiquidityLoopOutError;

    fn reserve_payout_lock_outpoint(
        &mut self,
        _quote: &crate::liquidity::types::LoopOutQuoteTerms,
    ) -> Result<ckb_types::packed::OutPoint, Self::Error> {
        Err(LiquidityLoopOutError::Chain(
            "payout transaction construction is not configured".to_string(),
        ))
    }

    fn broadcast_payout_lock(
        &mut self,
        _quote: &crate::liquidity::types::LoopOutQuoteTerms,
        _outpoint: &ckb_types::packed::OutPoint,
    ) -> Result<(), Self::Error> {
        Err(LiquidityLoopOutError::Chain(
            "payout transaction broadcast is not configured".to_string(),
        ))
    }

    fn watch_payout_lock(
        &mut self,
        _outpoint: &ckb_types::packed::OutPoint,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn broadcast_claim(&mut self, _plan: LoopOutClaimPlan) -> Result<(), Self::Error> {
        Err(LiquidityLoopOutError::Chain(
            "claim transaction construction is not configured".to_string(),
        ))
    }

    fn watch_claim(&mut self, _outpoint: &ckb_types::packed::OutPoint) -> Result<(), Self::Error> {
        Ok(())
    }

    fn broadcast_refund(
        &mut self,
        _outpoint: &ckb_types::packed::OutPoint,
    ) -> Result<(), Self::Error> {
        Err(LiquidityLoopOutError::Chain(
            "refund transaction construction is not configured".to_string(),
        ))
    }

    fn watch_refund(&mut self, _outpoint: &ckb_types::packed::OutPoint) -> Result<(), Self::Error> {
        Ok(())
    }
}
```

Then replace the temporary errors with actual `CkbChainMessage::SendTx` and `CkbChainMessage::CreateTxTracer` calls once transaction builders return `TransactionView`. This keeps startup wiring compile-safe before the transaction construction subtask is complete and prevents fake success for mutation RPCs because broadcast methods fail closed.

Bridge `LoopOutChainAdapter` to `LiquidityChainWatcher` in `actor.rs` or replace the older trait if all tests are migrated.

- [ ] **Step 4: Run chain tests**

Run: `cargo test -p fnn --features rocksdb chain_watcher_broadcast_plan_reuses_persisted_outpoint chain_watcher_refuses_claim_without_preimage -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/chain.rs crates/fiber-lib/src/liquidity/actor.rs
git commit -m "feat: add liquidity chain watcher boundary"
```

## Task 6: RPC Delegation To LiquidityActor

**Files:**
- Modify: `crates/fiber-lib/src/rpc/liquidity.rs`
- Test: `crates/fiber-lib/src/rpc/liquidity.rs`

- [ ] **Step 1: Write failing RPC delegation tests**

Add tests in `crates/fiber-lib/src/rpc/liquidity.rs`:

```rust
#[tokio::test]
async fn loop_out_rpc_delegates_to_actor() {
    let actor = spawn_liquidity_rpc_mock(RpcMockResponse::LoopOut).await;
    let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

    let response = rpc
        .loop_out(LoopOutParams {
            quote_id: [1u8; 32].into(),
            max_provider_fee: 10,
            max_routing_fee: 5,
        })
        .await
        .unwrap();

    assert_eq!(response.swap_id, [1u8; 32].into());
    assert_eq!(actor.take_events(), vec!["loop_out"]);
}

#[tokio::test]
async fn liquidity_mutation_rpcs_no_longer_return_placeholder_unavailable_errors() {
    let actor = spawn_liquidity_rpc_mock(RpcMockResponse::AllMutations).await;
    let rpc = LiquidityRpcServerImpl::new(MockLiquidityStore::default(), Some(actor.ref_.clone()));

    let error_text = rpc
        .provider_accept_loop_out(ProviderAcceptLoopOutParams {
            quote_id: [1u8; 32].into(),
            claimant_lock: "0x".to_string(),
            refund_lock: "0x".to_string(),
        })
        .await
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();

    assert!(!error_text.contains("unavailable until the liquidity actor RPC boundary is wired"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb loop_out_rpc_delegates_to_actor liquidity_mutation_rpcs_no_longer_return_placeholder_unavailable_errors -- --nocapture`

Expected: FAIL because `LiquidityRpcServerImpl::new` only accepts a store and mutation RPCs return placeholder errors.

- [ ] **Step 3: Wire mutation RPCs through actor calls**

Change the server struct:

```rust
use ractor::ActorRef;
use crate::liquidity::actor::LiquidityActorMessage;

pub struct LiquidityRpcServerImpl<S> {
    store: S,
    actor: Option<ActorRef<LiquidityActorMessage>>,
}

impl<S> LiquidityRpcServerImpl<S> {
    pub fn new(store: S, actor: Option<ActorRef<LiquidityActorMessage>>) -> Self {
        Self { store, actor }
    }
}
```

For each mutation method, replace placeholder errors with actor calls:

```rust
let actor = self.actor.as_ref().ok_or_else(|| rpc_error("liquidity actor is not available"))?;
let message = |reply| LiquidityActorMessage::LoopOut(params.clone(), reply);
crate::handle_actor_call!(actor.clone(), message, params).and_then(|result| result.rpc_err())
```

Use the matching message variants for `quote_loop_out`, `provider_quote_loop_out`, and `provider_accept_loop_out`. Keep `get_swap` and `list_swaps` unchanged.

- [ ] **Step 4: Run RPC tests**

Run: `cargo test -p fnn --features rocksdb loop_out_rpc_delegates_to_actor liquidity_mutation_rpcs_no_longer_return_placeholder_unavailable_errors -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/rpc/liquidity.rs
git commit -m "feat: delegate liquidity mutation rpcs to actor"
```

## Task 7: Startup Wiring And Resume On Startup

**Files:**
- Modify: `crates/fiber-lib/src/rpc/mod.rs`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: focused compile and actor recovery tests

- [ ] **Step 1: Write failing recovery tests**

Add tests in `actor.rs`:

```rust
#[tokio::test]
async fn resume_non_terminal_schedules_every_m3_loop_out_state_once() {
    let harness = RuntimeActorHarness::new_with_non_terminal_swaps();

    let resumed = harness.call_resume_non_terminal().await.unwrap();

    assert_eq!(resumed, 6);
    assert_eq!(
        harness.events(),
        vec![
            "watch_payout",
            "resume_payment",
            "reload_payment",
            "broadcast_claim",
            "watch_claim",
            "broadcast_refund",
        ]
    );
}

#[tokio::test]
async fn claim_pending_recovery_watches_existing_claim_without_rebroadcast() {
    let harness = RuntimeActorHarness::new_claim_pending_with_outpoint();

    harness.call_resume_non_terminal().await.unwrap();

    assert_eq!(harness.events(), vec!["watch_claim"]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p fnn --features rocksdb resume_non_terminal_schedules_every_m3_loop_out_state_once claim_pending_recovery_watches_existing_claim_without_rebroadcast -- --nocapture`

Expected: FAIL because resume scans are not implemented.

- [ ] **Step 3: Implement recovery scan**

In `LiquidityActorState::resume_non_terminal`, use the store's liquidity list/recovery API to load all Loop Out swaps in states:

```rust
const RECOVERABLE_LOOP_OUT_STATES: &[LiquiditySwapState] = &[
    LiquiditySwapState::PayoutPending,
    LiquiditySwapState::PayoutLocked,
    LiquiditySwapState::PaymentInFlight,
    LiquiditySwapState::PaymentSettled,
    LiquiditySwapState::ClaimPending,
    LiquiditySwapState::RefundPending,
];
```

For each record, call `recovery_action_for_loop_out_state(record.state)` and execute exactly one idempotent action. The mapping must be:

```rust
RecoveryAction::WatchPayout => chain.watch_payout_lock(existing_outpoint)
RecoveryAction::ResumePayment => continue only after role/client eligibility is confirmed
RecoveryAction::ReloadPayment => payment.reload_settled_preimage(record.payment_hash)
RecoveryAction::BroadcastClaim => claim_client_loop_out(store, chain, record.swap_id, now_ms)
RecoveryAction::WatchClaim => chain.watch_claim(existing_outpoint)
RecoveryAction::RefundProviderPayout => chain.broadcast_refund(existing_outpoint)
```

If the store lacks a direct recovery query, add a focused method to `LiquidityStore`:

```rust
fn list_liquidity_swaps_by_states(
    &self,
    states: &[LiquiditySwapState],
    swap_kind: LiquiditySwapKind,
) -> Result<Vec<LiquiditySwapRecord>, LiquidityStoreError>;
```

- [ ] **Step 4: Wire actor into RPC startup**

In `crates/fiber-lib/src/rpc/mod.rs`, when liquidity is enabled and `network_actor` plus `ckb_chain_actor` are available, spawn the actor before merging the liquidity RPC module:

```rust
let liquidity_actor = if config.is_module_enabled("liquidity") {
    match (network_actor.clone(), ckb_chain_actor.clone()) {
        (Some(network_actor), Some(ckb_chain_actor)) => {
            let payment = crate::liquidity::payment::NetworkLoopOutPaymentAdapter::new(network_actor);
            let chain = crate::liquidity::chain::CkbLiquidityChainWatcher::new(ckb_chain_actor);
            let (actor, _handle) = ractor::Actor::spawn(
                Some("liquidity-actor".to_string()),
                crate::liquidity::actor::LiquidityActor::<_, _, _>(std::marker::PhantomData),
                crate::liquidity::actor::LiquidityActorArguments {
                    store: store.clone(),
                    payment,
                    chain,
                },
            )
            .await?;
            let _ = ractor::call!(actor, |reply| {
                crate::liquidity::actor::LiquidityActorMessage::ResumeNonTerminal(reply)
            });
            Some(actor)
        }
        _ => None,
    }
} else {
    None
};
```

Then pass `liquidity_actor.clone()` into `LiquidityRpcServerImpl::new(store.clone(), liquidity_actor.clone())`.

Adjust exact spawn error handling to match existing `start_server` return type.

- [ ] **Step 5: Run recovery and compile checks**

Run: `cargo test -p fnn --features rocksdb resume_non_terminal_schedules_every_m3_loop_out_state_once claim_pending_recovery_watches_existing_claim_without_rebroadcast -- --nocapture`

Expected: PASS.

Run: `cargo check -p fnn --features rocksdb`

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/fiber-lib/src/rpc/mod.rs crates/fiber-lib/src/liquidity/actor.rs crates/fiber-lib/src/liquidity/payment.rs crates/fiber-lib/src/liquidity/chain.rs crates/fiber-lib/src/liquidity/store.rs
git commit -m "feat: spawn liquidity actor and resume swaps"
```

## Task 8: Deterministic Manual Loop Out Runtime Harness

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/actor.rs`
- Test: `crates/fiber-lib/src/liquidity/actor.rs`

- [ ] **Step 1: Write failing end-to-end deterministic test**

Add:

```rust
#[tokio::test]
async fn manual_loop_out_runtime_harness_reaches_success_only_after_claim_confirmation() {
    let client = RuntimeActorHarness::new_client();
    let provider = RuntimeActorHarness::new_provider_with_asset();

    let quote = provider
        .call_provider_quote(ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: 1000,
            receiver: "ckt1receiver".to_string(),
            max_provider_fee: 100,
            max_routing_fee: 50,
            expires_after_seconds: 60,
        })
        .await
        .unwrap();

    provider.call_provider_accept(quote.quote_id.into()).await.unwrap();
    client.import_quote_response(quote.clone());
    client.call_loop_out(quote.quote_id.into()).await.unwrap();

    assert_ne!(client.swap_state(quote.quote_id.into()), LiquiditySwapState::Success);

    client.call_claim_confirmed(quote.quote_id.into()).await.unwrap();
    provider.call_provider_claim_observed(quote.quote_id.into()).await.unwrap();

    assert_eq!(client.swap_state(quote.quote_id.into()), LiquiditySwapState::Success);
    assert_eq!(provider.swap_state(quote.quote_id.into()), LiquiditySwapState::Success);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p fnn --features rocksdb manual_loop_out_runtime_harness_reaches_success_only_after_claim_confirmation -- --nocapture`

Expected: FAIL until actor continuations and harness import methods are complete.

- [ ] **Step 3: Complete internal continuation handlers**

Implement `handle_payout_confirmed`, `handle_payment_settled`, `ClaimConfirmed`, and `ProviderClaimObserved` paths so they call existing helpers:

```rust
mark_client_payout_locked(&state.store, swap_id, now_ms)?;
send_client_loop_out_payment(&state.store, &mut state.payment, quote, now_ms)?;
claim_client_loop_out(&state.store, &mut state.chain, swap_id, now_ms)?;
mark_client_claim_confirmed(&state.store, swap_id, now_ms)?;
mark_provider_claim_observed(&state.store, swap_id, now_ms)?;
```

Do not mark `Success` from `PaymentSettled`. The only success transitions are `ClaimConfirmed` for client and `ProviderClaimObserved` for provider.

- [ ] **Step 4: Run deterministic runtime harness test**

Run: `cargo test -p fnn --features rocksdb manual_loop_out_runtime_harness_reaches_success_only_after_claim_confirmation -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/fiber-lib/src/liquidity/actor.rs
git commit -m "test: cover manual loop out runtime flow"
```

## Task 9: Documentation And Final Verification

**Files:**
- Modify: `crates/fiber-lib/src/rpc/README.md` if generated docs change
- Modify: `docs/specs/liquidity-management.md` only if runtime behavior documented there is stale

- [ ] **Step 1: Regenerate RPC docs if signatures or docs changed**

Run: `make gen-rpc-doc`

Expected: completes successfully. If `crates/fiber-lib/src/rpc/README.md` changes, review and include it in the commit.

- [ ] **Step 2: Run focused formatting and tests**

Run:

```bash
cargo fmt --all -- --check
cargo test -p fiber-json-types liquidity -- --nocapture
cargo test -p fnn --features rocksdb loop_out -- --nocapture
cargo test -p fnn --features rocksdb liquidity -- --nocapture
cargo check -p fnn --features rocksdb
```

Expected: all commands pass.

- [ ] **Step 3: Search for placeholders and unavailable errors**

Run:

```bash
rg "liquidity actor is not wired|unavailable until the liquidity actor RPC boundary is wired|unimplemented!\(|todo!\(" crates/fiber-lib/src/liquidity crates/fiber-lib/src/rpc/liquidity.rs crates/fiber-json-types/src/liquidity.rs
```

Expected: no output for placeholder runtime wiring errors. The string `liquidity actor is not available` is acceptable only for startup configurations where the actor cannot be spawned.

- [ ] **Step 4: Run broader verification if time allows**

Run: `cargo nextest run -p fnn --features rocksdb --no-fail-fast liquidity`

Expected: all selected tests pass. If unrelated order-dependent failures appear, rerun the failing test alone and record the result.

- [ ] **Step 5: Commit final docs/check updates**

Run:

```bash
git add crates/fiber-lib/src/rpc/README.md docs/specs/liquidity-management.md
git commit -m "docs: update liquidity runtime wiring docs"
```

If neither file changed, skip this commit.

## Self-Review Notes

- Spec coverage: actor boundary is covered by Tasks 1-2; RPC wiring by Task 6; payment adapter by Task 4; chain watcher by Task 5; persistence/state ordering by Tasks 2, 7, and 8; recovery by Task 7; completion verification by Task 9.
- Scope control: Loop In, provider discovery, p2p messages, and automated planning are excluded from every task.
- Safety invariants: tests explicitly assert persist-before-side-effect, payout-before-payment, preimage-before-claim, and success-after-claim-confirmation/provider-claim-observation.
