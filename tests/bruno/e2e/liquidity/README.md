# Liquidity E2E Suites

This hierarchy is reserved for production liquidity E2E flows. Mock-backed
helper checks live under `tests/bruno/smoke/liquidity-helpers/` so recursive
production runs never execute synthetic 105-row diagnostics assertions.

Suites:

- `ckb-loop-in/`, `ckb-loop-out/`, `udt-loop-in/`, `udt-loop-out/`,
  `provider-mode/`, `refund/`: single-collection flows against an
  externally-provisioned dev chain and nodes.
- `restart-recovery/`: two-phase flow driven by its own supervisor
  (`run-restart-test.sh`) that starts CKB and both FNN nodes itself, stops and
  restarts the provider mid-suite, and requires the `PayoutPending` Loop Out
  swap to recover to `Success` with the same payment hash.
