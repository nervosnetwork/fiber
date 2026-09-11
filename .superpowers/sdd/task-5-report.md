# Task 5 Report

## Status

Implemented dual-format watchtower settlement witness parsing.

## Changes

- `Htlc` now retains either the legacy 20-byte payment-hash prefix or the V1 32-byte full hash.
- `SettlementWitness::build_from_witness` selects 85-byte/20-byte or 97-byte/32-byte HTLC parsing from the persisted per-channel contract version.
- Tracked watchtower channel data preserves the negotiated version and uses it when rebuilding and comparing witnesses.
- Local and remote watchtower channel registration paths carry the version through events, RPC, storage, and migration schema metadata.
- Added focused legacy and V1 parser coverage, including 67-byte and 99-byte unlock lengths.
- No Task 6 reconciliation behavior was implemented.

## Verification

- `cargo nextest run -p fnn --features sqlite watchtower --no-fail-fast`: 25 passed.
- `cargo nextest run -p fiber-bin --no-fail-fast`: 1 passed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`: passed.
- `make check-migrate`: passed.
- `make check-dirty-rpc-doc`: passed after generated RPC documentation was included.

## Concerns

- `ChannelData` is backward-compatible through `serde(default)`; existing persisted channels without the field load as legacy.
- `tests/deploy/contracts/commitment-lock` was already an unstaged generated binary change and was not modified or committed by this task.

## Review Fix Report

### Changes

- First-settlement witness matching and construction now use the tracked channel's contract version, including the V1 97-byte/full-payment-hash layout.
- The watchtower store trait no longer has a version-discarding default implementation. Legacy registration explicitly selects Legacy, while versioned registration is required to preserve the supplied version.
- Added focused V1 first-settlement witness coverage and V1 store registration round-trip coverage.
- No Task 6 reconciliation behavior was implemented.

### Verification

- `cargo nextest run -p fnn --features sqlite watchtower --no-fail-fast`: 28 passed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`: passed.
- `make check-migrate`: passed.

### Concerns

- `tests/deploy/contracts/commitment-lock` remains an unrelated pre-existing unstaged generated binary change and is excluded from the commit.
