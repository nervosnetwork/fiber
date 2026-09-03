# Provider Mode E2E Suite

Real-process Bruno suite exercising the liquidity provider mode switch, its
rejection matrix, and the recovery of a non-terminal swap while the provider
is disabled — against the local CKB dev chain with a simple-UDT loop-in swap
as the vehicle.

Run from `tests/bruno`:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/provider-mode -r --env test
```

Prerequisites (see `tests/deploy/udt-init`):

- CKB dev chain with the liquidity-lock contract and the simple UDT contract
  deployed (`LIQUIDITY_LOCK_*` / `SIMPLE_UDT_*` environment variables) and the
  indexer enabled (`ckb run --indexer`).
- Nodes 1 and 2 running with the `liquidity` RPC module enabled and the exact
  `SIMPLE_UDT_*` script in their UDT whitelist (`auto_accept_amount` 1000).
- The node1 wallet holds funded simple UDT cells (the generator provisions
  them via `init_udt_accounts`).

## Node role mapping

Same node role assignment as the `udt-loop-in` suite:

| Suite role | Node | Environment variables used by this suite |
| ---------- | ---- | ---------------------------------------- |
| Client (invoice holder, quote importer, `loop_in` executor) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE1_ADDR` |
| Provider (provider mode switch, quote, accept, payer, claimer) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY`, `NODE2_ADDR` |

The suite uses the neutral `NODE1_*`/`NODE2_*` variables exclusively (same
reasoning as `udt-loop-in`).

## Flow

### Setup with the provider enabled (requests 01-14)

1. Connect node1 to node2 and open a UDT channel node2 → node1
   (auto-accepted; the provider pays the client invoice through it in the
   positive path), mine blocks, wait for `ChannelReady` on both nodes.
2. Create the client invoice on node1 (random preimage keeps the quote unique
   across re-runs), capture the client refund lock and the provider claimant
   lock, enable provider mode (idempotent), register the simple UDT asset
   (idempotent; the registry fee parameters are captured in both the fresh
   and already-exists paths), `quote_loop_in` on node2 and
   `import_liquidity_quote` on node1.
3. `loop_in` on node1 with `funding_tx: "local-wallet"`: the client swap is
   persisted as `OnchainLockPending` and its lock cell is broadcast but
   **not mined yet**. The provider holds no swap record for the quote (the
   accept has not run).

### Disabled matrix (requests 15-22)

4. **Disable** provider mode with the non-terminal client swap in flight.
5. Assert the disabled status shape (`enabled: false`, non-negative integer
   `enabled_asset_count` / `active_swaps`).
6. **Rejection matrix while disabled**: all four provider endpoints reject
   with exactly `provider mode is disabled` before any other validation runs
   (`ensure_provider_mode` is the first statement in every provider handler
   in `crates/fiber-lib/src/liquidity/actor.rs`):
   - `provider_quote_loop_out` (well-formed claimant lock, never validated);
   - `quote_loop_in` (placeholder invoice and empty refund lock, never
     parsed);
   - `provider_accept_loop_out` (all-zero quote id, rejected before the quote
     lookup);
   - `provider_accept_loop_in` (all-zero quote id; request 25 repeats this
     for the actually imported quote).
7. **Registry independence**: `add_liquidity_asset` and
   `list_liquidity_assets` work while the provider is disabled — the registry
   is preparable before going live.

### Non-terminal swap recovery while disabled (requests 23-25, F1)

8. Mine blocks with the provider still disabled. The client lock
   confirmation is driven by the client-side chain tracer with **no provider
   mode gate** (`handle_loop_in_lock_confirmed` branches only on the swap
   role), so the swap must advance:
   - `get_swap` on node1 reaches `onchain_locked` with the exact
     kind/asset/amount/payment hash, and the persisted `loop_in_lock` chain
     record flips to `confirmed`;
   - the provider (node2) still holds **no** swap record for the quote — the
     recovery happened entirely client-side.
9. `provider_accept_loop_in` for the **actually imported quote** (with its
   now-confirmed valid lock cell) still fails with
   `provider mode is disabled`: the mode gate precedes the quote lookup and
   cell validation.

### Re-enable and positive completion (requests 26-39, F2)

10. Re-enable provider mode and assert the status (`enabled: true`,
    `enabled_asset_count >= 2` — the simple UDT and ckb assets).
11. The `provider_quote_loop_out` request that was rejected in step 6 now
    returns a full envelope (registry-derived provider fee, on-chain estimate
    `0x3e8`, capacity requirement `0x2710`, claimant lock echo,
    molecule-decodable provider refund lock, `payout_deadline =
    expires_at + 10s`).
12. A dedicated `provider-mode-disabled-probe` UDT asset keeps the
    disabled-asset rejection isolated from the production asset:
    `disable_liquidity_asset` (bare asset id string parameter) returns the
    disabled entry and quoting it fails with
    `liquidity asset disabled: <asset_id>`.
13. The **identical** `provider_accept_loop_in` acceptance rejected in step 9
    now succeeds for the same quote (state `OnchainLockPending` or
    `OnchainLocked` depending on tracer timing), the provider pays the client
    invoice through the UDT channel, broadcasts the claim, and reaches
    `Success` after mining. The client invoice ends `Paid`; the client swap
    stays terminal at `OnchainLocked` (M0: no invoice-paid hook advances the
    client swap).
14. Final status: the provider is left enabled for collection runs, the
    disabled probe asset does not count towards `enabled_asset_count`, and
    the completed provider swap is terminal.

## Rerunnable by design

- The suite sets its own preconditions: it enables the provider for setup,
  disables it for the matrix, and re-enables it at the end.
- The client invoice uses a random preimage, so the quote (and the lock cell)
  is unique per run; registry requests are idempotent ("already exists"
  accepted) and re-disable the probe asset.
- Every mode transition is asserted on the `set_liquidity_provider_mode`
  response itself.
