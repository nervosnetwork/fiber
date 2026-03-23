# E2E test for external funding channel success flow (node2-funded)

Start nodes with a clean state:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/external-funding-open
```

This test validates a success-only scenario:

- Initialize node1/node2/node3 funding scripts and CKB balances before open.
- Node1 opens an external-funded channel to node3 via:
  - `open_channel_with_external_funding`
  - `submit_signed_funding_tx` (node2 signs).
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

The Bruno collection is intentionally trimmed to the core path now:

- `01` initializes scripts/balances and consumes the pre-opened channel context from the wrapper.
- `02/04/05/06/08/11/12/13/14` carry the main assertions.
- `03/07/09/10` remain as required transition steps inside the end-to-end flow.

## Running

This flow needs a node2-signed funding transaction. The easiest way is:

```bash
./tests/bruno/e2e/external-funding-open/run-success-flow.sh
```

### CI status

This scenario is currently not included in required PR CI runs.
Reason: it is relatively slow and depends on an external-signing wrapper flow.

### Prerequisites

- Started dev nodes for this testcase (clean state recommended):
  - `REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/external-funding-open`
- Required commands:
  - `curl`, `node`, `npm`, `ckb-cli`
- Default environment file used by the wrapper:
  - `tests/bruno/environments/test.bru`
- Default signer helper used by the wrapper:
  - `tests/bruno/utils/sign-openchannel-response.mjs`
- Optional override:
  - `SIGNER_SCRIPT=/path/to/sign-openchannel-response.mjs`

### Why not run Bruno directly

Do not run `npm exec -- @usebruno/cli run e2e/external-funding-open ...` directly for this success-only flow.
`02-submit-signed-funding-tx.bru` requires `EXTERNAL_FUNDING_SIGNED_TX`, which is injected by `run-success-flow.sh`.

The wrapper script will:

- connect node3 to node1;
- call `open_channel_with_external_funding`;
- sign the returned unsigned funding tx with node2 key (`tests/nodes/2/ckb/plain_key`);
- run the Bruno collection with injected env vars (`EXTERNAL_FUNDING_AMOUNT`, `EXTERNAL_FUNDING_SIGNED_TX`, preopen channel/tx).

The wrapper owns the fragile setup and signing work. The Bruno collection focuses on
submission, confirmation, readiness, payment, close, and post-close balance assertions.

By default, the wrapper auto-tries multiple funding amounts (from higher to lower)
until `open_channel_with_external_funding` succeeds.

You can still force a fixed amount:

```bash
EXTERNAL_FUNDING_AMOUNT=0x6fc23ac00 ./tests/bruno/e2e/external-funding-open/run-success-flow.sh
```

## Troubleshooting

- `Missing node2 key file`: ensure `tests/nodes/2/ckb/plain_key` exists.
- `Missing signer script`: ensure `tests/bruno/utils/sign-openchannel-response.mjs` exists, or set `SIGNER_SCRIPT` to an alternative path.
- Failed to open with fixed amount: lower `EXTERNAL_FUNDING_AMOUNT`.
