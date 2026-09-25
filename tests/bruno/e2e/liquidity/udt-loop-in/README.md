# UDT Loop In E2E Suite

Real-process Bruno suite exercising a full simple-UDT Loop In swap between two
liquidity-enabled Fiber nodes against the local CKB dev chain.

Run from `tests/bruno`:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/udt-loop-in -r --env test
```

Prerequisites (see `tests/deploy/udt-init`):

- CKB dev chain with the liquidity-lock contract deployed (its outpoint is
  exported as `LIQUIDITY_LOCK_*` environment variables; nodes resolve the
  contract themselves from genesis).
- CKB dev chain with the simple UDT contract in genesis (index 8) and the
  deployer-issued `SIMPLE_UDT_TYPE_SCRIPT` minted to the node wallets (the
  generator provisions huge UDT balances to nodes 1-3 for both simple UDT and
  xUDT via `init_udt_accounts`).
- Nodes 1 and 2 running with the `liquidity` RPC module enabled and the exact
  `SIMPLE_UDT_*` script in their UDT whitelist (`auto_accept_amount` 1000).

The xUDT environment variables (`XUDT_*`) are intentionally not consumed:
M0 liquidity coverage is scoped to the simple UDT asset.

## Node role mapping

Same node role assignment as the `ckb-loop-in` suite:

| Suite role | Node | Environment variables used by this suite |
| ---------- | ---- | ---------------------------------------- |
| Client (invoice holder, quote importer, `loop_in` executor, lock funder) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE1_ADDR` |
| Provider (provider mode, asset registry, quote, accept, payer, claimer) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY`, `NODE2_ADDR` |

The generator exports `LIQUIDITY_PROVIDER_RPC_URL`/`LIQUIDITY_PROVIDER_PUBKEY`
pointing at node1 and `LIQUIDITY_CLIENT_*` pointing at node2. Those variables
are intentionally **not** consumed here (same reasoning as in `ckb-loop-out`):
`quote_loop_in` stamps the quoting node's own pubkey into `provider_pubkey`,
and provider mode/quote/accept run on node2. The suite uses the neutral
`NODE1_*`/`NODE2_*` variables exclusively.

### Channel opener role (vs Loop Out)

For Loop In the **provider opens and funds the channel** (node2 → node1,
request 07), with `funding_udt_type_script` set to the generated simple UDT
script. The provider must pay the client invoice through the channel once the
lock confirms, so it needs the outbound (local) UDT capacity. Node1
auto-accepts a UDT funding above its whitelist `auto_accept_amount` (1000) and
**contributes zero UDT itself** (UDT auto-accept funds `funding_amount: 0`,
`NetworkActorMessage` handling in `fiber/network.rs`), so the entire channel
UDT balance comes from the provider. The client keeps its UDT in its wallet
for the lock transaction instead.

## Flow

1. Connect node1 to node2 and enable provider mode on node2.
2. Register the simple UDT asset in the provider registry via
   `add_liquidity_asset` with `kind: "udt"` and the exact
   `SIMPLE_UDT_TYPE_SCRIPT` (idempotent on re-runs: "already exists" is
   accepted) and relax its limits with `update_liquidity_asset`: the Loop In
   lock cell data carries the gross UDT amount (swap amount + provider fee),
   so the registry caps must cover that gross.
3. Open a UDT channel node2 → node1 (auto-accepted), mine blocks, and wait
   for `ChannelReady` on both nodes.
4. Create the client invoice on node1 (`new_invoice` with `udt_type_script`,
   amount = swap amount; a random preimage keeps the quote unique across
   re-runs), capture node1's `default_funding_lock_script` as the client
   refund lock and node2's as the expected claimant lock (both encoded as
   packed Molecule script bytes via `tests/bruno/scripts/molecule-script.js`).
5. `quote_loop_in` on node2 (the provider-side quote endpoint; it requires
   provider mode and takes the client invoice plus refund lock), relay the
   envelope through `import_liquidity_quote` on node1.
6. Assert pre-lock invariants: the client invoice is still `Open` and node2
   holds no payment record for the invoice payment hash.
7. `loop_in` on node1 with `funding_tx: "local-wallet"` (the only supported
   descriptor): the client funds the liquidity-lock cell with the gross UDT
   amount from its local wallet (the funding collects the client wallet's
   simple UDT cells). Capture the lock outpoint from
   `list_liquidity_chain_transactions` (role `loop_in_lock`), mine blocks, and
   wait for the client swap to reach `onchain_locked`.
8. `provider_accept_loop_in` on node2 with the lock tx hash and output index
   (the accept validates the cell against the quote, including the UDT data
   and the UDT script hash in the lock args). The provider then pays the
   client invoice through the UDT channel, learns the preimage from the
   settlement, and broadcasts the claim transaction spending the lock cell.
9. Mine blocks to confirm the claim and wait for `success` on the provider
   swap; assert the paid client invoice, both chain-transaction record sets,
   the exact channel balance deltas, and the claimed UDT output.

## Money flows and assertions

- **Channel**: the provider pays the client invoice for exactly the raw swap
  amount (`QUOTE_AMOUNT`, 1000 UDT atoms); routing fees are 0 on the direct
  channel. Client `local_balance` increases by the amount, provider
  `local_balance` decreases by it (asserted exactly on both nodes, requests
  31/32).
- **On-chain lock cell (request 22)**: the client funds a liquidity-lock cell
  whose **CKB capacity is the operational capacity** — for UDT quotes
  `capacity_requirement_ckb` equals `onchain_fee_estimate_ckb` (1000 shannons,
  `build_loop_in_quote_terms`), **not** the gross amount as in CKB Loop In.
  The cell data carries the gross UDT amount (`QUOTE_GROSS_UDT` =
  amount + provider fee, 1001 atoms) as a 16-byte little-endian u128, the cell
  type script is exactly `SIMPLE_UDT_TYPE_SCRIPT`, and the 152-byte lock args
  layout is asserted: payment hash, refund since, gross amount field, and the
  trailing `asset_type_hash` equal to `SIMPLE_UDT_SCRIPT_HASH` (the blake2b
  hash of the UDT type script molecule).
- **On-chain claim (request 30)**: the provider claims the lock cell into its
  own default funding lock. The claimed output preserves the simple UDT type
  script and the gross UDT amount with the same 1000-shannon capacity; the
  provider therefore nets the provider fee in UDT, and the client pays the
  funding transaction fee on top (wallet-level, not exactly assertable).
- **Invoice**: issued and settled for the raw swap amount (`Paid`, request 28).
- The provider fee expectation is derived from the registry response
  (`base_fee + amount * proportional_fee_ppm / 1_000_000`, = 1 UDT atom for
  the suite parameters), never duplicated.

Wallet UDT balances are not exposed through a node RPC, so balance assertions
are limited to channel balances (exact) and on-chain cell contents (exact via
`get_live_cell`/`get_transaction`). The client wallet must hold at least the
gross UDT amount plus CKB fees; the generator provisions that.

Swap amount and fee caps are defined once in request 04's pre-request script
(`SWAP_AMOUNT_HEX`, `MAX_PROVIDER_FEE_HEX`, `MAX_ROUTING_FEE_HEX`); every later
assertion derives from the quote/registry responses.

## Loop In / Loop Out differences encoded in this suite

- `quote_loop_in` is the provider-side quote endpoint for Loop In (there is no
  `provider_quote_loop_in`); `payout_deadline` equals `expires_at` (Loop Out
  adds a 10 second grace period) and `payment_hash` comes from the client
  invoice.
- The client invoice must carry the simple UDT type script (`udt_script`
  attribute); the provider quote/import validation rejects UDT mismatches.
- `funding_tx: "local-wallet"` is a `loop_in` parameter, not a channel open
  parameter; the channel is funded by plain `open_channel` +
  `funding_udt_type_script` + auto-accept.
- The client lock transaction is persisted under the payout role but surfaced
  as `loop_in_lock` by `list_liquidity_chain_transactions`.
- The provider drives the claim for Loop In and **does** persist a claim record
  (opposite of Loop Out, where the client claims and the provider only
  observes); the client records no claim and no refund on success.
- M0 limitation: the client loop-in swap ends at `onchain_locked`. There is no
  invoice-paid hook advancing the client swap, so `Success` is only reached on
  the provider side (request 27 asserts both final states explicitly).
