# Real Liquidity Refund E2E Suite

Real-process Bruno suite exercising the CKB lock-time refund semantics of a
Loop In liquidity-lock cell whose swap was never accepted by the provider.
The suite funds a real lock from the client node's wallet, attempts the
runtime-shaped refund transaction one second before its absolute timestamp
`since` maturity (CKB must reject it as immature), verifies maturity through
CKB RPC, then commits the exact refund transaction the runtime would
broadcast and asserts the lock cell is dead with the UDT amount conserved in
the refund output.

Run from `tests/bruno` via the suite runner (starts the lock-mutator
sidecar; the dev chain and both FNN nodes must already be running):

```bash
tests/bruno/e2e/liquidity/refund/run.sh
```

The runner performs the same command the other suites document:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/refund -r --env test
```

## Refund flow semantics in the runtime (M0)

Which records can be refunded, and how the refund reaches the chain:

- `LoopOutRefundTxPlan::from_record`
  (`crates/fiber-lib/src/liquidity/chain.rs`) only refunds
  `(LoopOut, Provider)` and `(LoopIn, Client)` records, and only from the
  `RefundPending` state with a persisted payout outpoint and the encoded
  `refund_after_lock_time` since.
- `broadcast_refund` (same file) deterministically rebuilds the refund
  transaction from the persisted swap record (secp256k1 RFC6979 signatures,
  though the liquidity-lock refund path itself needs no signature), verifies
  the hash against any persisted refund chain-tx record, and sends it via
  the CKB chain actor. `watch_refund` reports the confirmation back to the
  actor, which flips the record to `Confirmed` and the swap to `Refunded`
  (`LiquidityActorMessage::RefundConfirmed` -> `mark_refund_confirmed`).
- **Trigger path (M0 gap, why this suite drives the refund at the CKB
  level):** no production code currently transitions a swap into
  `RefundPending`. `RecoveryAction::RefundProviderPayout` fires only from
  `resume_non_terminal`, which runs once when the liquidity actor starts
  (`crates/fiber-lib/src/rpc/mod.rs`) and only for swaps already persisted
  as `RefundPending` (`resume_refund_pending_swap` in
  `crates/fiber-lib/src/liquidity/actor.rs`). There is no maturity scanner
  and no public resume RPC, so a live swap that ages past its refund
  maturity stays in `OnchainLocked` until a future supervisor adds the
  state transition (planned as follow-up runtime work; restart-driven
  recovery is a separate suite). The rebroadcast/watch/confirm machinery
  itself is covered by the actor unit tests
  (`refund_pending_recovery_broadcasts_refund_and_schedules_watch`,
  `loop_in_client_refund_confirmed_marks_refund_tx_confirmed_and_swap_refunded`,
  and siblings in `crates/fiber-lib/src/liquidity/actor.rs`).

Consequently request 23 pins the observable M0 behavior after an external
refund settled on chain: the swap record stays `OnchainLocked`, the lock
chain-tx record stays `Confirmed`, and no refund chain-tx record exists.

## Since semantics

- The Loop In quote stores `refund_after_lock_time =
  absolute_timestamp_since(expires_at)`:
  `0x4000_0000_0000_0000 | ceil(expires_at / 1000)` - an absolute CKB
  `since` with the timestamp metric, no relative bit, and the quote expiry
  rounded up to whole seconds
  (`crates/fiber-lib/src/liquidity/quote.rs`). Unlike Loop Out quotes, no
  extra grace period is added for Loop In.
- CKB verifies an absolute timestamp `since` against the median of the past
  37 block timestamps and rejects immature transactions on submission with
  `Immature` (RFC 0017). Request 19 proves the one-second-before-maturity
  variant fails `test_tx_pool_accept` and never reaches the chain; request
  20 proves the exact-since variant passes once
  `get_block_median_time(tip)` covers the maturity.

## Timing model

- The client controls the Loop In quote expiry via `expires_after_seconds`;
  the suite requests 60 seconds (`0x3c`). The pre-maturity rejection in
  request 19 runs well inside that window (the 37-block median time lags
  wall clock behind the quote expiry), and the maturity wait in request 20
  is bounded to roughly one minute of wall clock plus at most 12 generated
  dev-chain epochs.
- Maturity advancing never uses a fixed sleep: the suite polls
  `get_tip_header` timestamps and `get_block_median_time`, generating one
  epoch (10 blocks on the dev chain) per attempt only while the median is
  still below the maturity.

## Lock-mutator sidecar (refund mode)

Bruno cannot build molecule-encoded CKB transactions, so the suite drives
the Rust helper `tests/liquidity-lock-mutator` with `"kind": "refund"`
payloads. The helper builds the exact
`build_loop_in_client_refund_transaction` shape - input since, terminal
output locked by the client refund lock with the UDT type script and gross
amount, refund witness marker - plus the genesis-derived liquidity-lock and
simple-UDT code cell deps. No private key is involved: the refund path of
the liquidity-lock contract requires no secp256k1 signature. A
`since_offset` of `-1` produces the pre-maturity variant.

## Node role mapping

Same node role assignment as the `udt-loop-in` suite:

| Suite role | Node | Environment variables used by this suite |
| ---------- | ---- | ---------------------------------------- |
| Client (invoice holder, quote importer, lock funder wallet, refund owner) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE1_ADDR` |
| Provider (provider mode, asset registry, quote; never accepts) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY`, `NODE2_ADDR` |

## Flow

1. Connect node1 to node2, enable provider mode on node2, register the
   simple UDT asset and relax its limits (`update_liquidity_asset`).
2. Open a UDT channel node2 -> node1 (auto-accepted), mine blocks, wait for
   `ChannelReady`. The channel exists so the setup mirrors the loop-in
   happy path; the provider never accepts, so it stays unused.
3. Create the client invoice on node1 (random preimage keeps the quote
   unique across re-runs), capture node1's refund lock and node2's claimant
   lock, `quote_loop_in` on node2 with a 60 second expiry, and
   `import_liquidity_quote` on node1. The quote's `refund_after_lock_time`
   is decoded into the exact maturity second.
4. Execute `loop_in` on node1 (`local-wallet` funding), capture the lock
   outpoint, mine blocks, and assert the swap reaches `OnchainLocked` with
   the lock cell live and its args carrying the exact refund since.
5. Pre-maturity rejection: build the refund transaction with
   `since_offset: -1`, require `test_tx_pool_accept` to fail, require the
   transaction hash to be unknown to the chain, and require the lock cell
   to stay live with the swap still `OnchainLocked`.
6. Maturity: bounded `generate_epochs` polling until CKB RPC reports
   `get_block_median_time(tip) >= maturity`.
7. Mature refund: build the exact transaction (`since_offset: 0`), submit
   via `send_transaction`, mine, and wait for it to commit.
8. Settlement: the original lock cell is dead; the refund output is live
   with unchanged capacity, the client refund lock, the simple UDT type
   script, and the conserved gross UDT amount.
9. Runtime records: the swap stays `OnchainLocked` with a confirmed lock
   record and no refund record (M0 semantics documented above).

## Prerequisites

- CKB dev chain with the liquidity-lock contract and the simple UDT
  contract deployed (the generator exports `LIQUIDITY_LOCK_*` and
  `SIMPLE_UDT_*` environment variables) and the indexer enabled
  (`ckb run --indexer`).
- Nodes 1 and 2 running with the `liquidity` RPC module enabled and the
  exact `SIMPLE_UDT_*` script in node1's UDT whitelist
  (`auto_accept_amount` 1000).
- The node1 wallet holds funded simple UDT cells (the generator provisions
  them via `init_udt_accounts`); the lock consumes one UDT cell set from
  that wallet.
