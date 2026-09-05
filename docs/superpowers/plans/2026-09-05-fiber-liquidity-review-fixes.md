# Fiber Liquidity Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the Loop In hash-algorithm mismatch and propagate UDT type scripts through Loop Out Fiber payments.

**Architecture:** Keep validation at the quote boundary and carry the asset metadata through the existing payment request abstraction. Do not alter the chain lock format or introduce a second asset representation.

**Tech Stack:** Rust, Tokio/Ractor actors, `fiber_types::HashAlgorithm`, Cargo nextest.

---

### Task 1: Enforce CKB hash invoices

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/quote.rs:123-142`
- Test: `crates/fiber-lib/src/liquidity/quote.rs` existing unit test module

- [ ] **Step 1: Add a failing SHA-256 invoice test**

Create an invoice with `HashAlgorithm::Sha256`, pass it through the Loop In invoice validation path, and assert `LiquidityLoopOutError::PaymentFailed` identifies the hash algorithm mismatch.

- [ ] **Step 2: Run the focused quote tests and verify the new test fails**

Run `cargo nextest run --locked -p fnn --features rocksdb imported_quote --no-fail-fast`. Expected: the new SHA-256 case fails because the current validator accepts it.

- [ ] **Step 3: Add the minimal effective-hash validation**

After the amount and UDT checks, reject when `invoice.hash_algorithm().copied().unwrap_or_default() != HashAlgorithm::CkbHash`. Keep the error in the existing `PaymentFailed` category.

- [ ] **Step 4: Run the focused quote tests again**

Run the same nextest command. Expected: all matching tests pass, including the new rejection test and existing CKB/UDT quote tests.

### Task 2: Propagate UDT metadata into Loop Out payment

**Files:**
- Modify: `crates/fiber-lib/src/liquidity/payment.rs` request type and `NetworkLoopOutPaymentAdapter::send_loop_out_payment`
- Modify: `crates/fiber-lib/src/liquidity/actor.rs` request construction and recovery/retry construction sites
- Test: `crates/fiber-lib/src/liquidity/payment.rs` existing adapter tests

- [ ] **Step 1: Add a failing adapter assertion**

Extend the mock network state to record `SendPaymentCommand.udt_type_script`. Send a UDT Loop Out request and assert the command contains the quote's exact script. Add a CKB case asserting `None`.

- [ ] **Step 2: Run the focused payment tests and verify the UDT assertion fails**

Run `cargo nextest run --locked -p fnn --features rocksdb loop_out_payment --no-fail-fast`. Expected: the UDT assertion fails because the adapter currently hardcodes `None`.

- [ ] **Step 3: Extend the payment request and all constructors**

Add `udt_type_script: Option<packed::Script>` to the request, populate it from the persisted quote in initial execution and recovery/retry paths, and pass it unchanged to `SendPaymentCommand`.

- [ ] **Step 4: Run focused payment and actor tests**

Run `cargo nextest run --locked -p fnn --features rocksdb loop_out_payment --no-fail-fast` and the actor tests covering Loop Out recovery. Expected: all pass.

### Task 3: Full verification

**Files:**
- Verify: all modified Rust, manifest, lockfile, and documentation files

- [ ] **Step 1: Format and inspect the diff**

Run `cargo fmt --all -- --check` and `git diff --check`.

- [ ] **Step 2: Run compilation and lint checks**

Run `cargo check --locked -p fnn --tests --features rocksdb` and `cargo clippy --locked --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`.

- [ ] **Step 3: Run relevant tests and repository checks**

Run `cargo nextest run --locked -p fnn --features rocksdb --no-fail-fast`, `make check-migrate`, `make check-dirty-rpc-doc`, and `cargo shear`.

- [ ] **Step 4: Review final status**

Run `git status --short`, inspect the complete diff, and report any environment-limited E2E tests without claiming success for tests that timed out or could not run.
