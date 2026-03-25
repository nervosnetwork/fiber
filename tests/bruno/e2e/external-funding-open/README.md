# E2E test for external funding open/close flow (node2-funded)

This collection covers a minimal external-funding happy path with three dev nodes:

- node1 connects to node3 and opens the channel;
- node2 provides the CKB funding inputs and signs the funding transaction for node1's channel open;
- the channel is cooperatively closed back to node2's lock script.

The flow verifies that node1 acts as the channel initiator without spending its own CKB, and that the external funding is returned to node2 after close.

## Key Files

For a quicker read through the main external-funding flow, these are good files to start with:

- [`08-open-channel-with-external-funding.bru`](tests/bruno/e2e/external-funding-open/08-open-channel-with-external-funding.bru): the main entry point; defines `funding_amount`, injects node2's funding/shutdown script, and captures the unsigned funding tx plus channel id.
- [`09-sign-external-funding-tx.bru`](tests/bruno/e2e/external-funding-open/09-sign-external-funding-tx.bru): signs the unsigned funding tx with `EXTERNAL_FUNDING_PRIVKEY`.
- [`10-submit-signed-funding-tx.bru`](tests/bruno/e2e/external-funding-open/10-submit-signed-funding-tx.bru): submits the signed funding tx and records the on-chain funding tx hash.
- [`12-wait-channel-ready.bru`](tests/bruno/e2e/external-funding-open/12-wait-channel-ready.bru): the ready-state polling logic after funding submission.
- [`16-shutdown-channel-from-node1.bru`](tests/bruno/e2e/external-funding-open/16-shutdown-channel-from-node1.bru): starts cooperative shutdown using node2's close script.
- [`18-wait-channel-closed-and-capture-shutdown-tx.bru`](tests/bruno/e2e/external-funding-open/18-wait-channel-closed-and-capture-shutdown-tx.bru): the closed-state polling logic and shutdown tx capture.
- [`19-inspect-shutdown-tx.bru`](tests/bruno/e2e/external-funding-open/19-inspect-shutdown-tx.bru): checks the shutdown refund output returned to node2 and derives the shutdown fee.

The other `.bru` files are mostly support steps for setup, balance snapshots, and per-node assertions.

## Flow overview

This test validates the following sequence:

- Initialize node1/node2/node3 funding scripts and CKB balances before open.
- Node1 opens an external-funded channel to node3 via:
  - `open_channel_with_external_funding`
  - `sign_external_funding_tx` (node2 signs via dev RPC)
  - `submit_signed_funding_tx`
- Wait channel state to become `ChannelReady`.
- Check balances after open and confirm:
  - node1 balance stays unchanged;
  - node2 balance decreases by at least `funding_amount`;
  - node3 balance decreases by about 100 CKB for the auto-accept side.
- Node1 initiates `shutdown_channel` with node2 `close_script`.
- Wait channel is `Closed` and capture the cooperative shutdown transaction hash.
- Check balances after close and verify:
  - node2 receives the close output back to its script;
  - the returned amount is `funding_amount - shutdown_fee`;
  - node2 wallet balance increase after close matches that shutdown output exactly;
  - node1 balance stays unchanged across the entire flow;
  - node3 recovers part of its open-time spend, but still pays the non-refundable open fee.

The Bruno collection now follows a more request-oriented layout:

- `01` connects node1 to node3.
- `02` to `07` fetch the three nodes' funding scripts and pre-open CKB balances.
- `08` opens the external-funded channel.
- `09` signs the unsigned funding tx via `sign_external_funding_tx`.
- `10` submits the signed funding tx.
- `11` and `12` poll until the channel reaches `ChannelReady`.
- `13` to `15` check node1/node2/node3 balances after open.
- `16` initiates cooperative shutdown from node1.
- `17` and `18` poll until the channel reaches `Closed` and capture the shutdown tx hash.
- `19` inspects the shutdown transaction and records the refund/fee split.
- `20` to `22` check node1/node2/node3 balances after close.

## Running

Start dev nodes first:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/external-funding-open
```

Then run the Bruno collection directly:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/external-funding-open -r --env test
```

### Prerequisites

- Required commands:
  - `node`, `npm`
- Default Bruno environment:
  - `tests/bruno/environments/test.bru`
- Required environment variable in that file:
  - `EXTERNAL_FUNDING_PRIVKEY`

### Funding amount selection

[`08-open-channel-with-external-funding.bru`](tests/bruno/e2e/external-funding-open/08-open-channel-with-external-funding.bru) makes a single open attempt with the `funding_amount` defined in the JSON-RPC request body. In the current checked-in version, that value is `0xba43b7400` (500 CKB).

`EXTERNAL_FUNDING_AMOUNT` is not an input parameter for this collection. Step `08` reads the `funding_amount` from the request body and stores it in the Bruno variable `EXTERNAL_FUNDING_AMOUNT` only so later steps can reuse the same value for assertions.

If you want to test a different amount, update the `funding_amount` field in [`08-open-channel-with-external-funding.bru`](tests/bruno/e2e/external-funding-open/08-open-channel-with-external-funding.bru).

## Troubleshooting

- `Missing EXTERNAL_FUNDING_PRIVKEY environment variable`: add it to `tests/bruno/environments/test.bru`, or pass it with `--env-var EXTERNAL_FUNDING_PRIVKEY=...`.
- Failed to open with the selected amount: lower `funding_amount` in [`08-open-channel-with-external-funding.bru`](tests/bruno/e2e/external-funding-open/08-open-channel-with-external-funding.bru).
