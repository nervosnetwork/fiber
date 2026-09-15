# Commitment Lock Review Comments 2-3-4 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix V1 reserve sizing and watchtower snapshot reconstruction while removing the duplicate TLC tracking implementation, without adding the requested database migration.

**Architecture:** Thread `CommitmentContractVersion` through capacity calculations so the commitment cell is sized for its actual lock args. Make watchtower TLC tracking consume the settlement snapshot already verified by the caller, and keep one conversion helper in `onchain_tlc_reconcile.rs`.

**Tech Stack:** Rust, Cargo, bincode-backed Fiber store, CKB transaction helpers.

## Global Constraints

- Do not add or modify database migration code for `ChannelActorData`.
- Preserve Legacy behavior and serialization compatibility.
- V1 commitment-lock args use 58 bytes; Legacy uses 57 bytes.
- Run targeted tests before broader formatting and compile checks.

---

### Task 1: Fix Version-Aware Reserve Capacity

**Files:**
- Modify: `crates/fiber-lib/src/fiber/channel.rs`
- Modify: `crates/fiber-lib/src/fiber/fee.rs`
- Test: existing unit tests adjacent to capacity and fee validation code

- [ ] **Step 1: Add failing assertions** for Legacy and V1 occupied/reserved capacity, showing V1 is one byte larger and the minimum reserve increases accordingly.
- [ ] **Step 2: Run the focused tests** and confirm the V1 assertions fail against the hard-coded 57-byte calculation.
- [ ] **Step 3: Add `CommitmentContractVersion` to the capacity calculation signatures** and select 57 or 58 bytes from the version; update every caller with the channel's negotiated version.
- [ ] **Step 4: Run the focused capacity and fee tests** and confirm both versions pass without changing Legacy values.
- [ ] **Step 5: Run `cargo fmt --all`** and inspect the diff for unrelated changes.

### Task 2: Reuse the Verified Watchtower Snapshot

**Files:**
- Modify: `crates/fiber-lib/src/fiber/onchain_tlc_reconcile.rs`
- Modify: `crates/fiber-lib/src/watchtower/actor.rs`
- Test: `crates/fiber-lib/src/fiber/tests/onchain_tlc_reconcile.rs` and/or watchtower tests

- [ ] **Step 1: Add a regression test** with no revocation data, an on-chain S0 commitment, and pending S1 data; assert TLC tracking uses S0 and returns `Some(empty)` when S0 has no TLCs.
- [ ] **Step 2: Run the regression test** and confirm it fails because the current watchtower helper guesses `pending_remote_settlement_data`.
- [ ] **Step 3: Change the shared helper** to convert a caller-provided verified `SettlementData` rather than re-selecting a snapshot; retain direction and contract-version conversion behavior.
- [ ] **Step 4: Pass the `settlement_data` already returned by `verify_and_select_settlement_data()` from `try_settle_commitment_tx()`.
- [ ] **Step 5: Run the reconciliation/watchtower focused tests** and confirm the regression and existing tests pass.

### Task 3: Remove Duplicate TLC Tracking Logic

**Files:**
- Modify: `crates/fiber-lib/src/fiber/onchain_tlc_reconcile.rs`
- Modify: `crates/fiber-lib/src/watchtower/actor.rs`
- Modify: `crates/fiber-lib/src/fiber/tests/onchain_tlc_reconcile.rs`

- [ ] **Step 1: Remove the watchtower-local snapshot-selection and TLC-conversion helper** after Task 2 has established the shared interface.
- [ ] **Step 2: Adapt the shared tracked TLC type or conversion boundary** so watchtower scanning still receives the required commitment contract version.
- [ ] **Step 3: Update imports and tests** to use the single shared implementation.
- [ ] **Step 4: Run `cargo nextest run` for the affected Fiber tests** and verify no dead-code allowance or duplicate helper remains.

### Task 4: Final Verification

**Files:**
- No additional files expected.

- [ ] **Step 1: Run `cargo fmt --all -- --check`.**
- [ ] **Step 2: Run `cargo check --locked`.**
- [ ] **Step 3: Run targeted reconciliation, fee, and watchtower tests.**
- [ ] **Step 4: Run `git diff --check` and inspect `git status`/diff to confirm migration files were not changed.**
