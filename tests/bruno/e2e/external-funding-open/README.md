# E2E test for external funding channel success flow (node2-funded)

Start nodes with a clean state:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/external-funding-open
```

This test validates a success-only scenario:

- Initialize node1/node2/node3 funding scripts and CKB balances before open.
- Node1 opens an external-funded channel to node3 via:
  - `open_channel_with_external_funding`
  - `sign_external_funding_tx` (node2 signs via dev RPC)
  - `submit_signed_funding_tx`
- Wait funding tx `committed`, then wait channel state to become `ChannelReady`.
- Check balances after open:
  - node1 unchanged;
  - node3 unchanged or lower (temporary reserved capacity is possible);
  - node2 decreased (funding amount + fee).
- Node3 generates an invoice, node1 pays node3 over fiber.
- Node1 initiates `shutdown_channel` with node2 `close_script`.
- Wait channel is `CLOSED` on node3.
- Check balances after close:
  - node1 unchanged from initial;
  - node2 increased vs post-open, but still lower than initial;
  - node3 increased vs initial.

The Bruno collection now covers the full success flow end to end:

- `01` initializes scripts/balances, connects node1 to node3, and opens the channel directly.
- `02` signs the unsigned funding tx via `sign_external_funding_tx`.
- `03` submits the signed funding tx.
- `04` to `15` verify the channel funding, readiness, payment, shutdown, and post-close balances.

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

### CI status

This scenario is part of the Bruno e2e suite now. It is intended to be run as a normal end-to-end case rather than kept as a manual-only flow.

### Prerequisites

- Required commands:
  - `node`, `npm`
- Default Bruno environment:
  - `tests/bruno/environments/test.bru`
- Required environment variable in that file:
  - `NODE2_PRIVKEY`

### Funding amount selection

By default, `01-open-channel-with-external-funding.bru` auto-tries these funding amounts from high to low until `open_channel_with_external_funding` succeeds:

- `0x8bb2c9700` (375 CKB)
- `0x6fc23ac00` (300 CKB)
- `0x5d21dba00` (250 CKB)
- `0x4a817c800` (200 CKB)
- `0x37e11d600` (150 CKB)
- `0x2540be400` (100 CKB)
- `0x12a05f200` (50 CKB)

If you want to force a fixed amount, pass `EXTERNAL_FUNDING_AMOUNT` to Bruno:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/external-funding-open -r --env test \
  --env-var EXTERNAL_FUNDING_AMOUNT=0x6fc23ac00
```

## Troubleshooting

- `Missing NODE2_PRIVKEY environment variable`: add it to `tests/bruno/environments/test.bru`, or pass it with `--env-var NODE2_PRIVKEY=...`.
- Failed to open with fixed amount: lower `EXTERNAL_FUNDING_AMOUNT`.
