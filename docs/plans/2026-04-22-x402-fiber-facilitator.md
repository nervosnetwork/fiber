# x402 Fiber Facilitator Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a minimal x402-compatible Fiber facilitator that exposes `GET /supported`, `POST /verify`, and `POST /settle` so x402 resource servers can accept Fiber invoice payments.

**Architecture:** Implement the facilitator as a small HTTP service in `fnn` backed by the existing Fiber store, invoice parsing, and payment/invoice state. The first milestone is invoice-based exact payments only: the client pays a normal Fiber invoice, then presents `invoice + preimage` to the facilitator; verification checks x402 request shape, invoice details, preimage binding, and merchant-side invoice state before settlement returns a receipt-like response.

**Tech Stack:** Rust, hyper, tower, existing Fiber RPC/store/invoice types, tokio tests.

## Delivered MVP Checklist

- [x] `GET /supported` returns one exact Fiber kind for the local chain mapping
- [x] `POST /verify` accepts invoice plus preimage proof payloads
- [x] `/verify` enforces scheme, network, asset, amount, merchant `payTo`, invoice ownership, and paid invoice status
- [x] `POST /settle` returns a deterministic receipt for already-paid invoices
- [x] pure x402 verifier tests exist in addition to HTTP black-box tests
- [x] focused x402 tests pass

## Deferred Items

- [ ] no TypeScript `@x402/fiber` package yet
- [ ] no hold-invoice flow yet
- [ ] no delegated authorization flow yet
- [ ] no multi-asset x402 expression yet
- [ ] no x402 extension support yet
- [ ] no dedicated x402 listener or config section yet
- [ ] no richer payer identity reporting yet

---

### Task 1: Add failing tests for x402 facilitator HTTP surface

**Files:**
- Modify: `crates/fiber-lib/src/fiber/tests/mod.rs`
- Create: `crates/fiber-lib/src/fiber/tests/x402.rs`
- Reference: `crates/fiber-lib/src/tests/test_utils.rs`

**Step 1: Write the failing tests**

Add black-box HTTP tests covering the MVP contract:

```rust
#[tokio::test]
async fn test_x402_supported_lists_exact_fiber_kind() {
    // start a test node with x402 facilitator enabled
    // GET /supported
    // assert 200
    // assert kinds contains { x402Version: 2, scheme: "exact", network: "fiber:testnet" }
}

#[tokio::test]
async fn test_x402_verify_rejects_invalid_preimage() {
    // create merchant invoice
    // POST /verify with wrong preimage
    // assert 4xx/200-invalid according to chosen handler semantics
    // assert invalidReason indicates proof mismatch
}

#[tokio::test]
async fn test_x402_verify_accepts_paid_invoice_proof() {
    // create merchant invoice with known preimage
    // payer node pays invoice
    // POST /verify with invoice + preimage proof
    // assert isValid = true and payer is present
}

#[tokio::test]
async fn test_x402_settle_returns_receipt_for_verified_invoice() {
    // after invoice is paid
    // POST /settle with same proof
    // assert success = true, network = fiber:testnet, transaction is stable non-empty receipt id
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo nextest run x402_supported_lists_exact_fiber_kind x402_verify_rejects_invalid_preimage x402_verify_accepts_paid_invoice_proof x402_settle_returns_receipt_for_verified_invoice --no-fail-fast`

Expected: FAIL because the x402 module/server/routes do not exist yet.

**Step 3: Write minimal test helpers**

If the tests need shared helpers, add only the minimal test-only helper functions to start the facilitator service and make HTTP requests.

**Step 4: Run tests again**

Run the same `cargo nextest run ...` command.

Expected: still FAIL, but now for missing production code rather than test harness mistakes.

**Step 5: Commit**

Do not commit unless explicitly requested.

### Task 2: Define x402 request/response types and Fiber proof model

**Files:**
- Create: `crates/fiber-lib/src/x402/types.rs`
- Create: `crates/fiber-lib/src/x402/mod.rs`
- Modify: `crates/fiber-lib/src/lib.rs`

**Step 1: Write the failing unit tests**

Add focused serialization/validation tests for:
- `SupportedResponse`
- `VerifyRequest` / `VerifyResponse`
- `SettleRequest` / `SettleResponse`
- Fiber exact proof payload shape

Use the actual x402 V2 field names:

```rust
struct PaymentRequirements {
    scheme: String,
    network: String,
    asset: String,
    amount: String,
    pay_to: String,
    max_timeout_seconds: u64,
    extra: serde_json::Map<String, Value>,
}

struct FiberExactProof {
    invoice: String,
    payment_preimage: String,
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo nextest run x402 --no-fail-fast`

Expected: FAIL because the x402 module/types do not exist.

**Step 3: Write minimal implementation**

Implement only the types and helpers needed by the tests:
- x402 V2 request/response structs
- network mapping helper returning `fiber:mainnet`, `fiber:testnet`, or `fiber:dev`
- proof decoding helper that extracts `invoice` and `payment_preimage`

**Step 4: Run tests to verify they pass**

Run: `cargo nextest run x402 --no-fail-fast`

Expected: type-focused tests PASS.

**Step 5: Commit**

Do not commit unless explicitly requested.

### Task 3: Expose payment preimage in existing payment result types

**Files:**
- Modify: `crates/fiber-lib/src/fiber/network.rs`
- Modify: `crates/fiber-lib/src/fiber/payment.rs`
- Modify: `crates/fiber-json-types/src/payment.rs`
- Modify: `crates/fiber-lib/src/rpc/payment.rs`
- Modify: `fiber-js/src/types/payment.ts`

**Step 1: Write the failing tests**

Add a regression test proving a successful invoice payment can be read back with its preimage in the payment result JSON type.

```rust
#[tokio::test]
async fn test_get_payment_exposes_payment_preimage_after_success() {
    // create invoice with known preimage
    // pay it
    // call get_payment
    // assert payment_preimage == expected preimage
}
```

**Step 2: Run test to verify it fails**

Run: `cargo nextest run get_payment_exposes_payment_preimage_after_success --no-fail-fast`

Expected: FAIL because `GetPaymentCommandResult` does not include `payment_preimage`.

**Step 3: Write minimal implementation**

Thread the winning attempt preimage through:
- internal `SendPaymentResponse`
- JSON RPC `GetPaymentCommandResult`
- `send_payment_response_to_json`
- JS type definitions

Use the first successful attempt preimage if payment status is `Success`, otherwise `None`.

**Step 4: Run test to verify it passes**

Run: `cargo nextest run get_payment_exposes_payment_preimage_after_success --no-fail-fast`

Expected: PASS.

**Step 5: Commit**

Do not commit unless explicitly requested.

### Task 4: Implement pure verification logic for exact Fiber invoice proofs

**Files:**
- Create: `crates/fiber-lib/src/x402/facilitator.rs`
- Modify: `crates/fiber-lib/src/x402/mod.rs`
- Reference: `crates/fiber-lib/src/invoice/*`, `crates/fiber-types/src/invoice.rs`

**Step 1: Write the failing unit tests**

Cover pure verification rules without HTTP:
- unsupported scheme rejected unless `scheme == "exact"`
- unsupported network rejected unless it matches local chain mapping
- malformed proof rejected if `invoice` or `payment_preimage` missing
- preimage hash must match invoice payment hash
- requirements amount/payTo/asset must match parsed invoice
- invoice must belong to the configured merchant node
- merchant store status must be `Paid` for standard invoices in MVP

**Step 2: Run tests to verify they fail**

Run: `cargo nextest run x402_verify_ --no-fail-fast`

Expected: FAIL because the verifier does not exist.

**Step 3: Write minimal implementation**

Implement a verifier that:
- parses the invoice string into `CkbInvoice`
- computes hash using invoice hash algorithm (defaulting correctly)
- compares invoice details to x402 requirements
- checks merchant store invoice status
- returns `VerifyResponse { isValid, invalidReason, payer }`

Use a small error enum/string mapping; do not over-generalize.

**Step 4: Run tests to verify they pass**

Run: `cargo nextest run x402_verify_ --no-fail-fast`

Expected: PASS.

**Step 5: Commit**

Do not commit unless explicitly requested.

### Task 5: Add facilitator HTTP server and wire it into `fnn`

**Files:**
- Create: `crates/fiber-lib/src/x402/server.rs`
- Modify: `crates/fiber-lib/src/config.rs`
- Modify: `crates/fiber-bin/src/main.rs`
- Modify: `crates/fiber-lib/Cargo.toml` if extra HTTP feature wiring is needed

**Step 1: Write the failing integration tests**

Extend the HTTP black-box tests to start the x402 facilitator server from a node config and hit:
- `GET /supported`
- `POST /verify`
- `POST /settle`

**Step 2: Run tests to verify they fail**

Run: `cargo nextest run x402_supported_ x402_verify_ x402_settle_ --no-fail-fast`

Expected: FAIL because there is no HTTP service or config wiring.

**Step 3: Write minimal implementation**

Implement a tiny Hyper/Tower service with these routes:
- `GET /supported`
- `POST /verify`
- `POST /settle`

Wire it through a new config section on `Config`, parsed alongside existing services, and start/stop it in `fiber-bin` similarly to RPC lifecycle handling.

For MVP settlement behavior:
- re-run verify logic
- if valid and invoice is already `Paid`, return success receipt immediately
- `transaction` should be a deterministic receipt string derived from payment hash or invoice hash, not an empty string
- do not attempt a second network-side settlement for already-paid standard invoices

**Step 4: Run tests to verify they pass**

Run: `cargo nextest run x402_supported_ x402_verify_ x402_settle_ --no-fail-fast`

Expected: PASS.

**Step 5: Commit**

Do not commit unless explicitly requested.

### Task 6: Document MVP constraints and follow-up work

**Files:**
- Modify: `docs/plans/2026-04-22-x402-fiber-facilitator.md`
- Create or modify: `docs/solutions/` or a focused `README` once implementation location is clear

**Step 1: Write the failing docs checklist**

Create a checklist in the doc for the delivered MVP and explicitly list deferred items:
- no TypeScript `@x402/fiber` package yet
- no hold-invoice or delegated authorization flow yet
- no multi-asset x402 expression yet
- no x402 extension support yet

**Step 2: Run docs sanity check**

Run: `cargo fmt --all -- --check`

Expected: PASS or formatting-only failures.

**Step 3: Write/update docs**

Document:
- supported network strings
- expected proof payload shape
- exact verification semantics
- MVP limitations

**Step 4: Run docs-related verification**

Run: `cargo fmt --all && cargo nextest run x402 --no-fail-fast`

Expected: PASS.

**Step 5: Commit**

Do not commit unless explicitly requested.

### Task 7: Final verification

**Files:**
- No code changes required unless verification fails

**Step 1: Run focused test suite**

Run: `cargo nextest run x402 --no-fail-fast`

Expected: all x402 tests PASS.

**Step 2: Run broader regression coverage**

Run: `cargo nextest run rpc get_payment_exposes_payment_preimage_after_success --no-fail-fast`

Expected: PASS.

**Step 3: Run formatting**

Run: `cargo fmt --all -- --check`

Expected: PASS.

**Step 4: Run clippy on touched crates**

Run: `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`

Expected: PASS.

**Step 5: Summarize evidence**

Record the exact commands run and whether each passed or failed before claiming completion.
