# Task 4 Report

## Assessment of Previous Partial Changes

The previous partial implementation correctly introduced the per-channel version
through the open, external-funding open, and accept construction paths; persisted
it in `ChannelActorData`; added the default optional feature advertisement; made
the settlement witness builder version-aware; updated fee sizing; and added the
85/97-byte witness test.

Corrections made:

- Added the required legacy `0x00` settlement marker and conditional V1 `0x01`
  feature byte to the actual commitment output args builder, preserving 57-byte
  legacy and 58-byte V1 args.
- Kept the two revoke-signature message builders unchanged in layout. They are
  not on-chain commitment args and the commitment-lock contract hashes only the
  output plus the relevant base fields there. Adding the marker/feature byte to
  those preimages caused `test_revoke_old_commitment_transaction` to fail.
- Removed the attempted watchtower commitment-version parser. Task 5's deep
  watchtower parsing is deferred; callers now explicitly use the legacy layout,
  while the existing args suffix is propagated when constructing the next cell.
- Set `sample_full` to V1 so the sample exercises the new persisted enum value.
- Added the clippy allowance required by the expanded parameter list.

## Commands and Output

- `cargo fmt --all`
  - Passed with no output.
- `cargo nextest run -p fnn -p fiber-bin --features sqlite settlement_tlc_to_witness_matches_commitment_contract_layout --no-fail-fast`
  - Passed: 1 test, 1 passed, 1226 skipped.
- `cargo nextest run -p fnn -p fiber-bin --features sqlite test_revoke_old_commitment_transaction --no-fail-fast`
  - Passed: 1 test, 1 passed, 1226 skipped.
- `cargo nextest run -p fnn -p fiber-bin --features sqlite --no-fail-fast`
  - Compiled successfully.
  - 1218 tests run: 1189 passed, 29 failed, 9 skipped.
  - All 29 failures were immediate `Cannot assign requested address (os error 99)`
    failures in network/RPC test setup, not assertion or compilation failures.
- `RUST_TEST_THREADS=2 cargo nextest run -p fnn -p fiber-bin --features sqlite --no-fail-fast`
  - Same environment failure pattern: 1218 tests run, 1189 passed, 29 failed,
    9 skipped; the failures were the same address-assignment errors.
- `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`
  - Passed with exit code 0 and no warnings.
- `make check-migrate`
  - Passed: `migration check passed ...`.
- `git diff --check`
  - Passed.

## Deviations and Concerns

- The requested full suite cannot be reported fully green in this environment
  because the test harness cannot assign the network addresses used by 29 tests.
  The targeted protocol tests and all non-address-failing tests passed.
- Watchtower does not yet parse V1 settlement witnesses, as explicitly deferred
  to Task 5. It passes the legacy layout to the newly versioned helpers for now.
- `tests/deploy/contracts/commitment-lock` remains an uncommitted binary change
  inherited from the previous partial worktree. It was not generated or modified
  by this pass and is retained rather than discarded.
