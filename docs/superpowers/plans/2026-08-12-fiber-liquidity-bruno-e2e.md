# Fiber Liquidity Bruno E2E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Verify CKB and UDT liquidity flows across separate FNN processes and a real CKB dev chain using only public HTTP JSON-RPC.

**Architecture:** Append liquidity-lock to the dev genesis layout, generate liquidity-enabled independent node configs, and drive cross-node quote import/execution with Bruno suites. Reusable bounded polling scripts provide diagnostics; lock-time and restart scenarios use dedicated shell orchestration where Bruno cannot own processes.

**Tech Stack:** Bash, CKB dev chain/indexer, FNN binary, Bruno CLI 1.20, JavaScript/axios, JSON-RPC, GitHub Actions.

---

## File Structure

- Modify `tests/nodes/deployer/dev.toml`: append liquidity-lock genesis cell.
- Modify `crates/fiber-lib/src/ckb/contracts.rs`: resolve appended liquidity-lock index.
- Add `tests/deploy/contracts/liquidity-lock` and update `tests/deploy/contracts/README.md`.
- Modify `tests/nodes/deployer/config.yml` and `tests/deploy/udt-init/src/main.rs`: generate liquidity-enabled configs/environment.
- Modify `tests/bruno/environments/test.bru` and `tests/bruno/environments/xudt-test.bru`.
- Add `tests/bruno/scripts/wait-liquidity-state.js` and diagnostics helpers.
- Add a checked-in Rust malformed liquidity-cell transaction builder under `tests/`.
- Add suites under `tests/bruno/e2e/liquidity/`.
- Add restart runner under `tests/bruno/e2e/liquidity/restart/run-restart-test.sh`.
- Modify `.github/workflows/e2e.yml`: serial liquidity entries and artifacts.
- Modify `.gitignore`: ignore `/tests/artifacts/` runtime diagnostics.

**Prerequisites:** Complete and verify both
`2026-08-12-fiber-liquidity-cross-node-negotiation.md` and
`2026-08-12-fiber-liquidity-in-process-e2e.md`. This plan depends on immutable
quote envelopes, idempotent acceptance, provider-mode and chain-transaction RPCs,
durable signed transaction recovery, and proven mock-chain flows.

### Task 1: Register Liquidity-Lock On The Dev Chain

- [ ] Add `dev_genesis_resolves_liquidity_lock_at_index_10`; load the generated
  genesis and assert transaction 0 output 10 resolves as `Contract::LiquidityLock`
  with `DepType::Code`, `ScriptHashType::Data2`, and non-empty deps.
- [ ] Reuse the artifact pinned by the in-process plan: fiber-scripts commit
  `4da5d299fe449bb799bf057e0bb1f02c8fe7d101`, command
  `make -C contracts/liquidity-lock build TOP="$(pwd)/" BUILD_DIR=build/release`,
  SHA-256 `b7bfe051b6062f1ebb814ddf392406f86df9f403280eb48ec93a4d141c72dc4e`.
- [ ] Append the binary after `xudt_rce`; existing simple UDT/XUDT indices remain
  8/9 and liquidity-lock is index 10. Update `ContractsContext` mappings/comments
  and add real index/hash assertions in `udt-init` tests.
- [ ] Run `tests/deploy/init-dev-chain.sh -f`, query genesis, and assert expected code hash/outpoint.
- [ ] Stage only artifact/provenance/genesis/context/test files, inspect the cached
  diff, and commit with `git commit -m "test: register liquidity lock on dev chain"`.

### Task 2: Generate Liquidity-Enabled Node Configs

- [ ] Add `generated_nodes_one_and_two_enable_liquidity`; assert nodes 1/2 include
  `liquidity`, while bootnode/node3 keep their existing modules. Assert every
  participating node resolves the liquidity-lock cell dep.
- [ ] Update per-node config generation, not the common template globally; preserve
  separate stores, keys, ports, and UDT whitelist.
- [ ] Generate environment values for node URLs/pubkeys, liquidity asset IDs,
  simple UDT and XUDT scripts/code hashes, and artifact identity in both environment
  files. Do not treat provider-local `asset_id` as a chain identifier.
- [ ] Start nodes and smoke-call provider status, set provider mode, add/list assets.
- [ ] Stage config/generator/environment files, inspect the cached diff, and commit
  with `git commit -m "test: configure liquidity E2E nodes"`.

### Task 3: Add Bruno Polling And Diagnostics Helpers

- [ ] Add `wait-liquidity-state.js` using the existing Bruno-provided axios pattern;
  bounded attempts catch transient HTTP errors and include the last response/error.
- [ ] Add chain transaction/live-cell polling and RPC diagnostics fetching both
  nodes' `get_swap`, `list_swaps`, and `list_liquidity_chain_transactions`.
  Keep filesystem log collection in shell helpers under
  `tests/artifacts/liquidity/<suite>/`.
- [ ] Add `/tests/artifacts/` to `.gitignore`; before every commit inspect
  `git status --short` and use explicit path-based staging so runtime logs, node
  data, transaction dumps, and secrets cannot enter commits.
- [ ] Add Bruno smoke requests proving success and timeout diagnostics; do not add
  standalone Node tests unless dependencies and a lockfile are explicitly added.
- [ ] Stage helper/smoke files, inspect the cached diff, and commit with
  `git commit -m "test: add Bruno liquidity polling helpers"`.

### Task 4: CKB Loop Out Bruno Suite

- [ ] Create `tests/bruno/e2e/liquidity/ckb-loop-out/` with unique sequential
  requests: connect nodes; open/fund a channel with client outbound capacity above
  amount plus both fee caps; finalize it; enable provider; add CKB asset; request a
  quote with final claimant/refund scripts; import; accept by quote ID; execute;
  generate blocks; poll both nodes to `Success`; verify payout/claim identities and
  statuses through the chain-transaction RPC and CKB RPC.
- [ ] Transfer envelope fields with `bru.setVar`; never edit stores.
- [ ] From `tests/bruno`, run:
  `npm exec -- @usebruno/cli@1.20.0 run e2e/liquidity/ckb-loop-out -r --env test`.
- [ ] Stage only this suite/shared helper changes, inspect the cached diff, and
  commit with `git commit -m "test: add Bruno CKB Loop Out flow"`.

### Task 5: CKB Loop In Bruno Suite

- [ ] Open/fund the reverse-liquidity CKB channel. Fetch each node's
  `default_funding_lock_script` from `node_info` and encode packed Molecule script
  bytes with a shared tested helper; use `funding_tx: "local-wallet"`.
- [ ] Provider returns envelope; client imports/executes; obtain the lock outpoint
  from `list_liquidity_chain_transactions`; mine it; provider accepts it; poll
  payment/claim and both swaps to `Success`.
- [ ] Assert envelope provider pubkey equals `NODE2_PUBKEY`; before lock
  confirmation assert the invoice remains open and no payment record exists, then
  assert payment starts only after the lock record is confirmed.
- [ ] Run from `tests/bruno` with `--env test`, stage the suite/helper files,
  inspect the cached diff, and commit with
  `git commit -m "test: add Bruno CKB Loop In flow"`.

### Task 6: UDT Loop Out And Loop In Bruno Suites

- [ ] Use one exact generated simple-UDT script for provider registry, whitelist,
  channel funding type, payment, and chain outputs. Fund sufficient tokens and CKB
  operational capacity; Loop Out gives the client outbound UDT balance and Loop In
  gives the provider outbound balance.
- [ ] Add provider UDT asset through public RPC.
- [ ] Implement UDT Loop Out and Loop In suites; verify final chain output type/data and terminal state through RPC + CKB RPC scripts.
- [ ] Run simple UDT suites with `--env test`; the generated XUDT values remain
  available for existing suites but XUDT liquidity coverage is outside this plan.
  Stage exact suite files and commit with
  `git commit -m "test: add Bruno UDT liquidity flows"`.

### Task 7: Mismatch And Disabled-Provider Suites

- [ ] Add a Rust helper that accepts exact lock-argument/type/data mutations and
  emits a signed transaction. Bruno submits it through CKB JSON-RPC, mines it, and
  passes the outpoint to `provider_accept_loop_in`. Add one request case each for
  payment hash, claimant/refund script, amount, refund `since`, asset hash, type
  script, missing/15/17-byte data, and wrong UDT amount; invoice remains open and
  payment absent.
- [ ] For provider mode: enable and create a valid quote, disable, assert new quote
  and acceptance of the existing quote fail, assert an already non-terminal swap
  still recovers, re-enable, then prove quote/accept succeeds.
- [ ] Keep each suite independent by explicitly setting mode/assets.
- [ ] Stage helper/suite files, inspect the cached diff, and commit with
  `git commit -m "test: add Bruno liquidity rejection cases"`.

### Task 8: Real Lock-Time Refund Suite

- [ ] Create a deterministic payment failure leaving a live lock; decode the
  envelope's `refund_after_lock_time` as the exact CKB `since` metric defined by
  the runtime and capture its chain transaction records.
- [ ] Attempt refund one unit before maturity and assert rejection. Advance the
  corresponding block/timestamp/epoch property with bounded generation and verify
  maturity through CKB RPC rather than a fixed sleep.
- [ ] Let actor recovery rebroadcast the persisted signed refund transaction, poll
  its public record to `Confirmed` and the swap to `Refunded`, and verify the
  original lock cell is dead.
- [ ] Run the refund supervisor from a clean chain, stage only refund suite/runner
  files, inspect the cached diff, and commit with
  `git commit -m "test: add real liquidity refund E2E"`.

### Task 9: Shell-Driven Restart Recovery

- [ ] Build a dedicated supervisor that starts CKB and each FNN itself, records
  every PID, and never starts the generic `tests/nodes/start.sh` monitor. Do not
  copy its broad `pkill -f`; kill and await only the selected owned PID.
- [ ] Create a Loop Out swap in `PayoutPending`, stop provider FNN, restart it with
  the same data directory/password, reconnect with `connect_peer`, and require
  recovery to the specific terminal state `Success` with the same payout hash.
- [ ] Use traps for cleanup and preserve node logs/swap/tx diagnostics on failure.
- [ ] Stage only supervisor/suite files, inspect the cached diff, and commit with
  `git commit -m "test: add liquidity restart recovery E2E"`.

### Task 10: CI And Final Verification

- [ ] Add clean independent matrix jobs for four happy paths and rejection. Add
  dedicated sequential shell jobs for refund and restart; invoke the restart
  supervisor directly. Matrix entries need not be globally serialized because
  runners are isolated.
- [ ] Write node/CKB logs, swap JSON, chain transaction JSON, and CKB tx/live-cell
  snapshots under `tests/artifacts/liquidity/<suite>/`; upload that directory on
  failure.
- [ ] Run every suite twice from a clean chain using
  `REMOVE_OLD_STATE=y`; also rerun one suite against existing state to prove its
  unique IDs and explicit provider/asset setup are independent.
- [ ] Run `cargo fmt --all -- --check`, `cargo check --locked`,
  `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`,
  `make check-migrate`, and `make check-dirty-rpc-doc`.
- [ ] Request final spec/code review and fix all blocking findings.
