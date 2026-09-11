# Final Review Fix Report

## Findings fixed

- Inbound channel-open negotiation now computes `CommitmentContractVersion` once when `OpenChannel` is received. The selected value is stored in the pending in-memory entry, persisted in `ChannelOpenRecord`, and passed unchanged into `AcceptChannel` and the final channel actor. Accept no longer derives the version from the current peer session.
- Full-hash-invalid on-chain preimages now use `error!`; the exact settlement record is still persisted and the mismatching preimage is not inserted into the watch-preimage store.

## Verification

- `cargo nextest run -p fnn --features rocksdb pending_open_retains_contract_version_selected_at_receipt settlement_builder_rejects_preimage_for_different_full_hash`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`
- `make check-migrate`

All listed commands passed. The focused run passed 2 tests.

## Scope

The generated `tests/deploy/contracts/commitment-lock` binary was not modified by this fix.
