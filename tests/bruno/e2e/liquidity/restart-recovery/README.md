# Liquidity Restart Recovery E2E Suite

Real-process Bruno suite proving that a Loop Out swap parked in
`PayoutPending` survives a full FNN process restart of the provider node and
recovers to terminal `Success` with the same payment hash. Unlike the other
liquidity suites, this directory is not a single Bruno collection: it is a
two-phase flow driven by a dedicated supervisor that owns the complete process
lifecycle (CKB dev chain + both FNN nodes).

Run it (from anywhere; the supervisor resolves repo paths itself):

```bash
tests/bruno/e2e/liquidity/restart-recovery/run-restart-test.sh
```

## Why a dedicated supervisor

The other liquidity suite runners assume the dev chain and both nodes are
already running. Restart coverage cannot: the suite must stop and restart one
FNN process mid-flow with exactly the same data directory and secret password.
The supervisor therefore

- starts the CKB dev chain and each FNN node itself and records every spawned
  PID in an owned-PID registry;
- only ever signals PIDs from that registry (SIGTERM, bounded await, SIGKILL
  escalation, reap) - there is no `pkill`, no pattern-matched process
  discovery, and no reuse of the generic `tests/nodes/start.sh` monitor;
- refuses to start when any required port (CKB 8114, node RPC 21714/21715,
  node P2P 8344/8345) is already held by a foreign process;
- installs EXIT/INT/TERM traps that tear down exactly the owned PIDs;
- preserves diagnostics on failure: node logs live under
  `tests/artifacts/liquidity/restart-recovery/logs/` for the whole run, and a
  failure additionally writes redacted swap/payment/chain-transaction
  snapshots through `tests/bruno/scripts/write-liquidity-diagnostics.sh` and
  tails the logs to stderr;
- uses bounded waits for every readiness and shutdown transition.

Provisioning is owned as well, and split by chain state:

- **Fresh dev chain** (`tests/deploy/node-data` missing): the supervisor runs
  the nilpotent `tests/deploy/init-dev-chain.sh`, which provisions everything
  itself with its own temporary CKB - contracts deployed, node wallets funded,
  UDT accounts initialized, and the liquidity-enabled `tests/nodes/*/config.yml`
  generated.
- **Existing dev chain**: the full provisioner (`tests/deploy/deploy.sh` ->
  `udt-init` main mode) funds UDT accounts over live CKB RPC, which this
  supervisor does not have before it starts its own chain. It therefore only
  refreshes the genesis-derived Bruno environment files chain-free via
  `GENERATE_BRUNO_ENVIRONMENTS_ONLY=1 NODES_DIR=... tests/deploy/udt-init`
  (`udt-init/src/main.rs` main mode guard) and then **verifies** that both
  generated node configs enable the `liquidity` RPC module, refusing to start
  with regeneration instructions if they do not. The supervisor never rewrites
  node configs against an existing chain.

The supervisor builds the `fnn` binary (`--locked`, plus `debug-add-tlc` for
non-release builds, mirroring `tests/nodes/start.sh`) and the `udt-init`
provisioner. Before the first node start it wipes
`tests/nodes/{1,2}/fiber/store` so the run begins from a deterministic
swap/channel state (`KEEP_FIBER_STATE=1` keeps them).

Environment knobs: `TEST_ENV` (default `debug`), `RPC_READY_TIMEOUT`
(default 120s), `STOP_GRACE_SECONDS` (default 30s), `KEEP_FIBER_STATE`,
`SKIP_PROVISIONING` (skips the chain-free environment refresh; the node-config
verification always runs).

## Flow

| Phase | Actor | Steps |
| ----- | ----- | ----- |
| supervisor | - | preflight, build, provision (fresh: full init; existing: chain-free env refresh + config verification), start CKB + node1 + node2 (owned PIDs) |
| phase1 (Bruno) | both | connect, provider mode, CKB asset, open channel, fund, ready, quote/import/accept/execute |
| phase1 (Bruno) | both | park both swaps in `PayoutPending` and pin the durable pre-stop state |
| supervisor | node2 | discover swap ids over RPC, gate on the persisted payout broadcast record, write the handoff artifact, SIGTERM node2, verify its RPC is down, restart node2 (same data dir + `password2`), wait for RPC |
| phase2 (Bruno) | both | reconnect, verify persisted swap + payout record, mine the payout, payment leg, claim, mine, both swaps `Success` with the exact pre-restart channel balance delta |
| supervisor | - | tear down owned PIDs (trap) |

The payout transaction is broadcast by `provider_accept_loop_out` in phase 1
and deliberately never mined while the provider is up. Nothing can advance the
swaps during the downtime: the client only moves out of `PayoutPending` after
the payout cell is confirmed on chain, and we only mine in phase 2 after the
restarted provider is reconnected and its channel is `ChannelReady` again. The
provider therefore never observes a payout confirmation, a payment settlement,
or a claim while it is down - everything after the restart is live observation
through watches re-registered by startup recovery, not historical tracer
catch-up (for the committed-claim catch-up variant see the in-process test
`liquidity_ckb_loop_out_provider_restart_discovers_committed_claim` in
`crates/fiber-lib/src/tests/liquidity_integration.rs`).

## Restart semantics exercised (as implemented in the runtime)

Recovery is driven by `resume_non_terminal`
(`crates/fiber-lib/src/liquidity/actor.rs`), which runs exactly once when the
liquidity actor starts (`crates/fiber-lib/src/rpc/mod.rs`). For each persisted
non-terminal Loop Out swap it dispatches on
`recovery_action_for_loop_out_state`:

| Persisted state | Recovery action | Restart behavior |
| --------------- | --------------- | ---------------- |
| `PayoutPending` | `WatchPayout` | re-registers `watch_payout_lock` from the persisted outpoint (provider fails closed without one) |
| `PayoutLocked` / `PaymentInFlight` | `ResumePayment` / `ReloadPayment` | client re-authorizes (revalidates the live payout cell and pays once); provider re-registers the invoice and re-observes the payment |
| `PaymentSettled` / `ClaimPending` | `BroadcastClaim` | client rebuilds/broadcasts the claim and watches it; provider registers `watch_provider_claim` (outpoint spend tracer) |
| `RefundPending` | `RefundProviderPayout` | broadcasts and watches the refund |

This suite stops the provider in `PayoutPending`, so the provider recovery
path is `WatchPayout`. What then happens on the restarted provider (all from
persisted store state, `crates/fiber-lib/src/liquidity/actor.rs`):

1. `resume_non_terminal` re-registers the payout cell watch from the outpoint
   persisted by `provider_accept_loop_out` (phase-1 request 16 pins that
   record as `broadcast` before the stop).
2. The provider invoice registered at accept survives the restart because
   invoices are store-persisted (`NetworkActorCommand::GetInvoice` reads from
   the store), so the incoming payment can settle.
3. Once the payout is mined, `handle_payout_confirmed` (provider branch)
   confirms the payout chain-tx record, `mark_provider_payout_locked`,
   `mark_provider_payment_in_flight`, and spawns the payment observer.
4. The client pays the invoice through the re-established channel; the
   observer sees the invoice `Paid`, `mark_provider_payment_settled`, and
   `watch_provider_claim` is registered.
5. The client broadcasts the claim; phase 2 mines it; the claim tracer
   observes the payout cell spend and `mark_provider_claim_observed`
   transitions `ClaimPending` -> `Success`.

The client (node1) never restarts. Its path is the normal loop-out
continuation: payout confirmed -> live cell revalidation (`payout_locked`) ->
invoice payment (`payment_in_flight` -> settled) -> claim broadcast
(`claim_pending`) -> claim confirmed (`success`).

`Success` with the same payment hash is asserted on both nodes (phase-2
request 09), and request 11 proves the provider continued the exact payout
record persisted before the stop (same tx hash, now `confirmed`, no
duplicate), with no fabricated claim record.

## Phase handoff design

Bruno variables do not survive across CLI invocations, so the handoff is
three-layered:

1. **RPC discovery by the supervisor.** After phase 1 the supervisor queries
   `list_swaps` (client), `get_swap` /
   `list_liquidity_chain_transactions` (provider) and `list_channels`
   (client) over JSON-RPC, gates on the expected pre-stop state (exactly one
   client `loop_out` swap in `payout_pending`, provider payout record
   `broadcast`, exactly one `ChannelReady` channel), and captures `swap_id`,
   `payment_hash`, `payout_tx_hash`, `channel_id`, and the client channel
   balances.
2. **Artifact.** Those values are written to
   `tests/artifacts/liquidity/restart-recovery/phase-handoff.json` (gitignored
   artifacts area) as the auditable record of what was stopped and what must
   recover.
3. **Bruno env injection.** The supervisor passes the same values to phase 2
   via repeated `--env-var RESTART_*=...` flags (supported by the pinned
   `@usebruno/cli@1.20.0`): `RESTART_SWAP_ID`, `RESTART_PAYMENT_HASH`,
   `RESTART_PAYOUT_TX_HASH`, `RESTART_CHANNEL_ID`,
   `RESTART_CLIENT_LOCAL_BEFORE` and `RESTART_CLIENT_REMOTE_BEFORE`. Phase-2
   requests cross-check every injected value against the live swap/chain-tx
   records, and request 01 falls back to discovering the single non-terminal
   `loop_out` swap when the suite is run by hand without injection. The
   balance baseline pair activates the exact channel-balance delta assertion
   in request 12 (payment principal out, principal in); without injection
   (manual run) request 12 logs and skips that one assertion while all swap
   and chain-transaction assertions stay enforced.

## Node role mapping

| Suite role | Node | Environment variables used |
| ---------- | ---- | -------------------------- |
| Client (channel opener, quote importer, `loop_out` executor, payer, claimant) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE2_ADDR` |
| Provider (provider mode, asset registry, quote, accept, payout, restart target) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY` |

## Prerequisites

- `ckb`, `ckb-cli`, `cargo`, `jq`, `curl`, `nc`, and node >= 18 (for
  `npx @usebruno/cli@1.20.0`) on PATH.
- No other process holding the CKB/FNN ports listed above.
- Fresh machine: the first run initializes the dev chain (contracts deployed,
  wallets funded, liquidity-enabled node configs generated) - this takes
  several minutes. Later runs reuse the existing chain data and only refresh
  the Bruno environment files chain-free; they require the node configs to
  already enable the `liquidity` RPC module (the supervisor verifies and
  prints regeneration instructions otherwise).

## Plan naming note

The task plan refers to this suite as `restart/`; the committed directory is
`restart-recovery/`. The name was chosen to describe what the suite actually
proves (restart-driven recovery of a non-terminal swap) and to keep the
directory name distinct from the generic "restart" wording used by the
supervisor's log lines. All plan requirements map onto
`tests/bruno/e2e/liquidity/restart-recovery/`.
