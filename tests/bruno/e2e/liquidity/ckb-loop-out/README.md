# CKB Loop Out E2E Suite

Real-process Bruno suite exercising a full CKB Loop Out swap between two
liquidity-enabled Fiber nodes against the local CKB dev chain.

Run from `tests/bruno`:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/ckb-loop-out -r --env test
```

Prerequisites (see `tests/deploy/udt-init`):

- CKB dev chain with the liquidity-lock contract deployed (its outpoint is
  exported as `LIQUIDITY_LOCK_*` environment variables; nodes resolve the
  contract themselves from genesis).
- Nodes 1 and 2 running with the `liquidity` RPC module enabled.

## Node role mapping

This suite assigns roles opposite to the generic `LIQUIDITY_PROVIDER_*` /
`LIQUIDITY_CLIENT_*` environment variables emitted by the dev-chain generator:

| Suite role | Node | Environment variables used by this suite |
| ---------- | ---- | ---------------------------------------- |
| Client (channel opener, quote importer, `loop_out` executor, claimant) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE1_ADDR` |
| Provider (provider mode, asset registry, quote, accept) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY`, `NODE2_ADDR` |

The generator exports `LIQUIDITY_PROVIDER_RPC_URL`/`LIQUIDITY_PROVIDER_PUBKEY`
pointing at node1 and `LIQUIDITY_CLIENT_*` pointing at node2. Those variables
are intentionally **not** consumed here: they would silently invert the roles
this suite mandates (provider mode and the quote/accept calls run on node2,
and `provider_quote_loop_out` stamps the quoting node's own pubkey into
`provider_pubkey`). The suite therefore uses the neutral `NODE1_*`/`NODE2_*`
variables exclusively. Shared chain metadata (`LIQUIDITY_CKB_ASSET_ID`) is
used as-is.

## Flow

1. Connect node1 to node2 and enable provider mode on node2.
2. Register the CKB asset (`ckb`) in the provider registry (idempotent on
   re-runs: "already exists" is accepted).
3. Open a CKB channel node1 → node2 (auto-accepted), mine blocks, and wait
   for `ChannelReady` on both nodes.
4. `provider_quote_loop_out` on node2, `import_liquidity_quote` on node1,
   `provider_accept_loop_out` on node2 (returns the payout outpoint),
   `loop_out` on node1 with that outpoint.
5. Mine blocks to confirm the provider payout, wait for the client claim to
   broadcast (payment settlement reveals the preimage), mine again to confirm
   the claim, and wait for `success` on both swaps.
6. Assert chain transaction records, exact channel balance deltas (principal =
   amount + provider fee, routing fee is 0 on the direct channel), the settled
   client payment, and the paid provider invoice.

Swap amount and fee caps are defined once in request 10's pre-request script
(`SWAP_AMOUNT_HEX`, `MAX_PROVIDER_FEE_HEX`, `MAX_ROUTING_FEE_HEX`); every
later assertion derives from the quote/registry responses.
