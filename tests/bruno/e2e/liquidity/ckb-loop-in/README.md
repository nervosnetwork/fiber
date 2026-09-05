# CKB Loop In E2E Suite

Real-process Bruno suite exercising a full CKB Loop In swap between two
liquidity-enabled Fiber nodes against the local CKB dev chain.

Run from `tests/bruno`:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/ckb-loop-in -r --env test
```

Prerequisites (see `tests/deploy/udt-init`):

- CKB dev chain with the liquidity-lock contract deployed (its outpoint is
  exported as `LIQUIDITY_LOCK_*` environment variables; nodes resolve the
  contract themselves from genesis).
- Nodes 1 and 2 running with the `liquidity` RPC module enabled.

## Node role mapping

Same node role assignment as the `ckb-loop-out` suite:

| Suite role | Node | Environment variables used by this suite |
| ---------- | ---- | ---------------------------------------- |
| Client (invoice holder, quote importer, `loop_in` executor, lock funder) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE1_ADDR` |
| Provider (provider mode, asset registry, quote, accept, payer, claimer) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY`, `NODE2_ADDR` |

The generator exports `LIQUIDITY_PROVIDER_RPC_URL`/`LIQUIDITY_PROVIDER_PUBKEY`
pointing at node1 and `LIQUIDITY_CLIENT_*` pointing at node2. Those variables
are intentionally **not** consumed here (same reasoning as in `ckb-loop-out`):
`quote_loop_in` stamps the quoting node's own pubkey into `provider_pubkey`,
and provider mode/quote/accept run on node2. The suite uses the neutral
`NODE1_*`/`NODE2_*` variables exclusively. Shared chain metadata
(`LIQUIDITY_CKB_ASSET_ID`) is used as-is.

### Channel opener role change (vs Loop Out)

For Loop In the **provider opens and funds the channel** (node2 → node1,
request 07). The provider must pay the client invoice through the channel once
the lock confirms, so it needs the outbound (local) capacity; node1 auto-accepts
and contributes its configured auto-accept amount on top. The client keeps its
funds on-chain for the lock transaction instead.

## Flow

1. Connect node1 to node2 and enable provider mode on node2.
2. Register the CKB asset (`ckb`) in the provider registry (idempotent on
   re-runs: "already exists" is accepted) and relax its limits with
   `update_liquidity_asset`: the Loop In lock cell capacity equals the gross
   on-chain amount and must cover the CKB minimum occupied capacity of the
   liquidity-lock script (roughly 222 CKB), so the 300 CKB swap amount used by
   this suite requires `max_amount`/`available_capacity` of at least that size.
3. Open a CKB channel node2 → node1 (auto-accepted), mine blocks, and wait for
   `ChannelReady` on both nodes.
4. Create the client invoice on node1 (`new_invoice`, amount = swap amount; a
   random preimage keeps the quote unique across re-runs), capture node1's
   `default_funding_lock_script` as the client refund lock and node2's as the
   expected claimant lock (both encoded as packed Molecule script bytes via
   `tests/bruno/scripts/molecule-script.js`).
5. `quote_loop_in` on node2 (the provider-side quote endpoint; it requires
   provider mode and takes the client invoice plus refund lock), relay the
   envelope through `import_liquidity_quote` on node1.
6. Assert pre-lock invariants: the client invoice is still `Open` and node2
   holds no payment record for the invoice payment hash.
7. `loop_in` on node1 with `funding_tx: "local-wallet"` (the only supported
   descriptor): the client funds the liquidity-lock cell with the gross
   on-chain amount from its local wallet. Capture the lock outpoint from
   `list_liquidity_chain_transactions` (role `loop_in_lock`), mine blocks, and
   wait for the client swap to reach `onchain_locked`.
8. `provider_accept_loop_in` on node2 with the lock tx hash and output index
   (the lock must already be live/confirmed; the accept validates the cell
   against the quote). The provider then pays the client invoice through the
   channel, learns the preimage from the settlement, and broadcasts the claim
   transaction spending the lock cell.
9. Mine blocks to confirm the claim and wait for `success` on the provider
   swap; assert the paid client invoice, both chain-transaction record sets,
   and the exact channel balance deltas.

## Money flows and assertions

- **Channel**: the provider pays the client invoice for exactly the raw swap
  amount (`QUOTE_AMOUNT`); routing fees are 0 on the direct channel. Client
  `local_balance` increases by the amount, provider `local_balance` decreases
  by it (asserted exactly on both nodes, requests 31/32).
- **On-chain**: the client funds the lock cell with the gross amount
  (`QUOTE_GROSS_ONCHAIN` = amount + provider fee, which also equals
  `capacity_requirement_ckb` for CKB loop-in quotes; asserted against the live
  lock cell capacity in request 22); the provider claims the
  full lock cell into its own default funding lock. The provider fee is
  therefore charged on-chain, not through the channel: the provider nets the
  provider fee plus the claimed cell, the client pays the funding transaction
  fee on top of the gross amount (wallet-level, not exactly assertable).
- **Invoice**: issued and settled for the raw swap amount (`Paid`, request 28).
- The provider fee expectation is derived from the registry response
  (`base_fee + amount * proportional_fee_ppm / 1_000_000`), never duplicated.

Swap amount and fee caps are defined once in request 04's pre-request script
(`SWAP_AMOUNT_HEX`, `MAX_PROVIDER_FEE_HEX`, `MAX_ROUTING_FEE_HEX`); every later
assertion derives from the quote/registry responses.

## Loop In / Loop Out differences encoded in this suite

- `quote_loop_in` is the provider-side quote endpoint for Loop In (there is no
  `provider_quote_loop_in`); `payout_deadline` equals `expires_at` (Loop Out
  adds a 10 second grace period) and `payment_hash` comes from the client
  invoice.
- `funding_tx: "local-wallet"` is a `loop_in` parameter, not a channel open
  parameter; the channel is funded by plain `open_channel` + auto-accept.
- The client lock transaction is persisted under the payout role but surfaced
  as `loop_in_lock` by `list_liquidity_chain_transactions`.
- The provider drives the claim for Loop In and **does** persist a claim record
  (opposite of Loop Out, where the client claims and the provider only
  observes); the client records no claim and no refund on success.
- M0 limitation: the client loop-in swap ends at `onchain_locked`. There is no
  invoice-paid hook advancing the client swap, so `Success` is only reached on
  the provider side (request 27 asserts both final states explicitly).
