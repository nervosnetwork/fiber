# Provider Mode E2E Suite

Real-process Bruno suite exercising the liquidity provider mode switch and the
provider-side rejection matrix that guards it, against a single dev-chain node
(node2). No channels, no on-chain transactions and no mutator sidecar are
required; every state change happens through the `liquidity` RPC module.

Run from `tests/bruno`:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/provider-mode -r --env test
```

Prerequisites: node2 running with the `liquidity` RPC module enabled (the
suite never talks to node1 or to the CKB chain).

## Flow

1. **Disable** provider mode (`set_liquidity_provider_mode`, request 01) so the
   rejection matrix below is deterministic regardless of the state left by
   earlier suites or runs.
2. Assert the disabled status shape (`get_liquidity_provider_status`,
   request 02): `enabled: false`, non-negative integer `enabled_asset_count`
   and `active_swaps`.
3. **Rejection matrix while disabled** (requests 03-06): all four provider
   endpoints reject with exactly `provider mode is disabled` before any other
   validation runs (`ensure_provider_mode` is the first statement in every
   provider handler in `crates/fiber-lib/src/liquidity/actor.rs`):
   - `provider_quote_loop_out` (well-formed claimant lock, the quote itself
     never reaches validation);
   - `quote_loop_in` (placeholder invoice and empty refund lock, never parsed);
   - `provider_accept_loop_out` (all-zero quote id, rejected before the quote
     lookup);
   - `provider_accept_loop_in` (all-zero quote id and lock tx hash).
4. **Registry independence** (request 07): `add_liquidity_asset` works while
   the provider is disabled ("already exists" accepted on re-runs) — the
   registry is preparable before going live.
5. **Enable** provider mode (request 08) and assert the status (request 09):
   `enabled: true` and `enabled_asset_count >= 1`.
6. **Enabled quote** (requests 10-11): `list_liquidity_assets` propagates the
   ckb registry fee parameters, and the identical `provider_quote_loop_out`
   request that was rejected in request 03 now returns a full envelope
   (registry-derived provider fee, on-chain estimate `0x3e8`, capacity
   requirement `0x2710`, claimant lock echo, molecule-decodable provider refund
   lock, `payout_deadline = expires_at + 10s`).
7. **Disabled asset rejection** (requests 12-14): a dedicated
   `provider-mode-disabled-probe` UDT asset keeps this test isolated from the
   production simple UDT asset. `disable_liquidity_asset` (bare asset id
   string parameter) returns the disabled entry, and quoting the disabled
   asset fails with `liquidity asset disabled: <asset_id>`
   (`validate_loop_out_quote_request` checks the asset switch first).
8. **Final status** (request 15): the provider is left enabled for collection
   runs, and the disabled probe asset does not count towards
   `enabled_asset_count`.

## Rerunnable by design

- The suite sets its own preconditions: it disables the provider first and
  re-enables it at the end, so it can run standalone, before or after the
  other liquidity suites.
- Registry requests are idempotent ("already exists" accepted); the probe
  asset is re-disabled by request 13 even when a previous run left it
  disabled.
- No swap state is created: `active_swaps` stays a passive observation.
