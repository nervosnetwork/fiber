# UDT Loop Out E2E Suite

Real-process Bruno suite exercising the provider-funded prefix of a simple-UDT
Loop Out swap between two liquidity-enabled Fiber nodes against the local CKB
dev chain.

Run from `tests/bruno`:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/udt-loop-out -r --env test
```

## M0 deviation: the suite covers the deterministic prefix only

The full CKB Loop Out success path (client channel payment → settlement →
client claim → `Success` on both swaps → final channel balance deltas) is
**not reachable for UDT swaps with the current code**. Three layers make the
client payment impossible:

1. The liquidity payment adapter builds the client Loop Out payment without
   any asset information: `send_loop_out_payment`
   (`crates/fiber-lib/src/liquidity/payment.rs`) issues a `SendPaymentCommand`
   with `udt_type_script: None` and no invoice.
2. Payment routing matches channel currencies exactly: every hop requires
   `payment_data.udt_type_script == channel_info.udt_type_script`
   (`crates/fiber-lib/src/fiber/graph.rs`). A `None` payment can therefore
   never traverse the UDT channel this suite opens.
3. Settlement would fail even if the payment reached the provider: the
   provider registers its Loop Out invoice with the asset's UDT type script
   (`register_provider_loop_out_invoice`), and
   `try_to_settle_down_tlc` (`crates/fiber-lib/src/fiber/channel.rs`) rejects
   incoming TLCs whose channel funding UDT script does not equal the invoice
   UDT script.

Consequently requests 18-20 assert the last deterministic on-chain state
(the confirmed provider payout with its UDT cell semantics) and the suite ends
there. The client swap ends in `Failed` once the payment fails; no refund
path runs for that state, so the payout cell remains live. Once the payment
layer stamps the quoted asset's UDT type script into the client payment, this
suite should be extended with the CKB Loop Out tail: wait claim broadcast,
confirm the claim (capacity `0x2328`, UDT amount preserved under the claimant
lock), wait `Success` on both swaps, assert the settled payment/paid invoice
and the exact channel balance deltas (client `-principal`, provider
`+principal`, principal = amount + provider fee).

## Prerequisites (see `tests/deploy/udt-init`)

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

Same node role assignment as the `ckb-loop-out` suite:

| Suite role | Node | Environment variables used by this suite |
| ---------- | ---- | ---------------------------------------- |
| Client (channel opener, quote importer, `loop_out` executor, claimant) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE1_ADDR` |
| Provider (provider mode, asset registry, quote, accept, payout funder) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY`, `NODE2_ADDR` |

The generator exports `LIQUIDITY_PROVIDER_RPC_URL`/`LIQUIDITY_PROVIDER_PUBKEY`
pointing at node1 and `LIQUIDITY_CLIENT_*` pointing at node2. Those variables
are intentionally **not** consumed here: they would silently invert the roles
this suite mandates (provider mode and the quote/accept calls run on node2,
and `provider_quote_loop_out` stamps the quoting node's own pubkey into
`provider_pubkey`). The suite therefore uses the neutral `NODE1_*`/`NODE2_*`
variables exclusively.

## Flow

1. Connect node1 to node2 and enable provider mode on node2.
2. Register the simple UDT asset in the provider registry (`add_liquidity_asset`
   with `kind: "udt"` and the exact `SIMPLE_UDT_TYPE_SCRIPT`; idempotent on
   re-runs: "already exists" is accepted).
3. Open a UDT channel node1 → node2 (`funding_udt_type_script`, auto-accepted;
   node2 contributes zero UDT), mine blocks, and wait for `ChannelReady` on
   both nodes. Node2's local UDT balance must be exactly zero at this point.
4. `provider_quote_loop_out` on node2 (asset-agnostic provider constants:
   on-chain fee estimate 1000, capacity requirement 10000 shannons),
   `import_liquidity_quote` on node1, `provider_accept_loop_out` on node2
   (the accept reserves and funds the payout cell from the provider wallet's
   simple UDT cells, returning the payout outpoint), `loop_out` on node1 with
   that outpoint.
5. Wait for `payout_pending` on both nodes, mine blocks to confirm the payout.
6. Assert the UDT payout cell semantics via CKB `get_live_cell` (request 18)
   and the persisted payout records on both nodes (requests 19/20).

## UDT payout cell math (request 18)

- **CKB capacity**: exactly `capacity_requirement_ckb` (10000 shannons,
  `0x2710`). The provider funding request reserves the full requirement as the
  cell capacity for UDT swaps (`build_payout_funding_request`).
- **UDT amount**: the cell data is the swap amount (1000 atoms) as a 16-byte
  little-endian u128; the lock args `amount` field carries the same value.
- **Type script**: exactly the generated `SIMPLE_UDT_TYPE_SCRIPT`.
- **Lock args** (152 bytes): payment hash, blake2b of the client claimant
  lock, blake2b of the provider refund (funding) lock, refund since
  (`expires_at + 20s`), the swap amount, and the trailing `asset_type_hash`
  equal to `SIMPLE_UDT_SCRIPT_HASH` (the blake2b hash of the UDT type script
  molecule; zeros for CKB swaps).
- The eventual claim output (deferred, see the deviation above) would spend
  this cell into a `0x2328` (9000 shannons) cell under the claimant lock with
  the UDT type script and amount preserved
  (`build_loop_out_claim_transaction`).

Swap amount and fee caps are defined once in request 10's pre-request script
(`SWAP_AMOUNT_HEX`, `MAX_PROVIDER_FEE_HEX`, `MAX_ROUTING_FEE_HEX`); every
later assertion derives from the quote/registry responses. The provider fee
expectation is derived from the registry response
(`base_fee + amount * proportional_fee_ppm / 1_000_000`, = 1 UDT atom for the
suite parameters), never duplicated.
