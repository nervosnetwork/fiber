# UDT Loop In Rejection E2E Suite

Real-process Bruno suite exercising the `provider_accept_loop_in` rejection
paths against committed-but-invalid liquidity-lock cells. Every mutated cell
is a real signed transaction spending a real dev-chain wallet input; the
suite submits it through the CKB JSON-RPC, mines it, and passes the outpoint
to the provider accept, which must reject the cell without persisting any
swap state.

Run from `tests/bruno` via the suite runner (starts the lock-mutator sidecar):

```bash
tests/bruno/e2e/liquidity/udt-loop-in-rejection/run.sh
```

The runner performs the same command the other suites document:

```bash
npx @usebruno/cli@1.20.0 run e2e/liquidity/udt-loop-in-rejection -r --env test
```

### Lock-mutator sidecar (integration point for the suite harness)

Bruno cannot execute Rust or spawn processes, and the lock-argument
components (payment hash, refund since, gross amount, capacity) only exist
at suite runtime once the quote was created and imported. The suite therefore
drives the Rust helper `tests/liquidity-lock-mutator` as a local HTTP sidecar:

- the runner (or the harness supervisor) builds it with
  `cargo build --locked` and starts
  `liquidity-lock-mutator --serve 38117 --rpc-url $CKB_RPC_URL
  --privkey-path tests/nodes/1/ckb/plain_key`;
- the suite POSTs the quote terms plus a mutation selector to
  `{{CKB_MUTATOR_URL}}` (`test.bru`: `http://127.0.0.1:38117`) and receives
  `{tx, tx_hash, outpoint}`;
- the suite then sends the signed transaction via CKB `send_transaction`,
  mines with `generate_epochs`, and waits (bounded) for `get_transaction`
  committed before the accept runs.

The private key never travels through Bruno: the sidecar reads it from the
node1 dev wallet file passed on the command line. The unit-tested tx
assembly lives in `tests/liquidity-lock-mutator/src/lib.rs` (cargo test in
that standalone workspace).

## Prerequisites

- CKB dev chain with the liquidity-lock contract and the simple UDT contract
  deployed (the generator exports `LIQUIDITY_LOCK_*` and `SIMPLE_UDT_*`
  environment variables) and the indexer enabled (`ckb run --indexer`).
- Nodes 1 and 2 running with the `liquidity` RPC module enabled and the
  exact `SIMPLE_UDT_*` script in their UDT whitelist (`auto_accept_amount`
  1000).
- The node1 wallet holds funded simple UDT cells (the generator provisions
  them via `init_udt_accounts`); each mutation case consumes one UDT cell
  set from that wallet.

## Node role mapping

Same node role assignment as the `udt-loop-in` suite:

| Suite role | Node | Environment variables used by this suite |
| ---------- | ---- | ---------------------------------------- |
| Client (invoice holder, quote importer, lock funder wallet) | node1 | `NODE1_RPC_URL`, `NODE1_PUBKEY`, `NODE1_ADDR` |
| Provider (provider mode, asset registry, quote, accept) | node2 | `NODE2_RPC_URL`, `NODE2_PUBKEY`, `NODE2_ADDR` |

The suite uses the neutral `NODE1_*`/`NODE2_*` variables exclusively (same
reasoning as `udt-loop-in`).

## Flow

1. Connect node1 to node2, enable provider mode on node2, register the
   simple UDT asset and relax its limits with `update_liquidity_asset` (the
   lock cell data carries the gross UDT amount).
2. Open a UDT channel node2 -> node1 (auto-accepted), mine blocks, wait for
   `ChannelReady` on both nodes. The positive control at the end pays the
   client invoice through this channel.
3. Create the client invoice on node1 (random preimage keeps the quote
   unique across re-runs), capture node1's refund lock and node2's claimant
   lock (`node_info` + molecule encoding), `quote_loop_in` on node2 and
   `import_liquidity_quote` on node1.
4. Assert pre-accept invariants: no swap for the quote, invoice `Open`, no
   provider payment record. Every rejected accept re-asserts the same
   invariants (requests 18/20/22/24/26/28/30/32/34).
5. Mutation cases: one request pair per mutated field. The build request
   POSTs the quote terms to the sidecar, publishes the signed transaction,
   mines and waits for the cell to commit; the accept request asserts the
   exact validation failure and the untouched invariants:

   | Case | Mutation | Expected rejection phrase |
   | ---- | -------- | ------------------------- |
   | payment hash | lock args bytes 0..32 flipped | `observed lock payment_hash mismatch` |
   | claimant script | lock args bytes 32..64 flipped | `observed lock claimant_lock_hash mismatch` |
   | refund script | lock args bytes 64..96 flipped | `observed lock refund_lock_hash mismatch` |
   | args amount | lock args amount +1 atom | `observed lock amount mismatch` |
   | refund since | lock args since +1 second | `observed lock refund_after_lock_time mismatch` |
   | asset hash | lock args bytes 120..152 flipped | `observed lock asset_type_hash mismatch for UDT asset` |
   | type script | cell type script args replaced | `observed loop in lock UDT type script mismatch` |
   | data length | cell data 0/15/17 bytes (one case, three cells) | `observed loop in lock UDT data length ...` |
   | wrong UDT amount | cell data amount +1 atom | `observed loop in lock UDT amount mismatch` |

   The refusals mirror `validate_liquidity_lock_args` /
   `validate_liquidity_live_cell` in `crates/fiber-lib/src/liquidity/chain.rs`
   (the RPC message wraps them in `chain operation failed:` prefixes, so the
   suite asserts with `includes`).

   Mutation semantics that keep the funding transactions committable:
   - the type-script case gives the lock cell a different UDT script hash
     with empty data and conserves the full gross amount in a change output
     under the real UDT script (the type script comparison fires before the
     data checks);
   - the data-length cases rely on the simple UDT zero-padding rule: a
     15-byte gross amount is still conserved, and the missing/17-byte
     variants conserve via the change output;
   - the wrong-UDT-amount case collects inputs for the mutated amount and
     conserves it in the change output.
6. Positive control (requests 35-41): the helper commits a fully valid lock
   cell from the same quote terms; the provider accept must succeed
   (`OnchainLockPending`/`OnchainLocked`), pay the client invoice through
   the channel, broadcast the claim, and reach `Success` after mining. The
   client invoice must end `Paid`. This proves the rejections above were
   caused by the mutations and not by the environment.

## Rerunnable by design

- The client invoice uses a random preimage, so the quote (and every
  mutated cell) is unique per run.
- Registry registration is idempotent ("already exists" accepted) and
  `update_liquidity_asset` raises the caps again.
- Rejected accepts persist nothing (`validate_observed_loop_in_lock` runs
  before the store writes), so every quote stays consumable within the run
  and across re-runs.
- Each committed mutated cell is never spent; every case funds fresh cells
  from the node1 wallet.
