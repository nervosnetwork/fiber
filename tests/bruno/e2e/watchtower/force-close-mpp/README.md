# MPP force-close reconciliation

This workflow reproduces the payer-side MPP reconciliation gap reported in
PR #1512 using the project's real three-node CKB dev-chain environment.

- Node1 opens two 1,000 CKB channels to Node2.
- Node2 opens one 2,000 CKB channel to Node3.
- Node3 creates a 1,001 CKB MPP hold invoice, forcing Node1 to use both
  Node1-Node2 channels.
- Node2 force-closes one of those channels before Node3 settles the invoice.
- Before the closing transaction is mined, the test observes the fulfilled
  split as `Outbound.RemoteRemoved` on Node1 and `Inbound.LocalRemoved` on
  Node2, while the payer payment is still `Inflight`.
- After mining the force-close transaction, Node1 explicitly runs the same
  shutdown check used by the existing `shutdown-force` E2E instead of waiting
  for the five-minute background scan.
- The dev chain is advanced through watchtower settlement and unlock. The
  invoice must become `Paid`, the payer payment must become `Success`, and the
  other split must finish off-chain.

Run it with a clean local dev-chain state:

```bash
REMOVE_OLD_STATE=1 ./tests/nodes/start.sh \
  e2e/watchtower/force-close-mpp
```

Then, in another terminal:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run \
  e2e/watchtower/force-close-mpp -r --env test
```
