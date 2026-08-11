# Hosted LSP lifecycle and payments

This workflow starts Node2 as a Public Trampoline node with the hosted LSP service enabled. It
registers and activates a tenant, opens a private in-process channel between Public T and that
tenant, then evicts and reactivates the tenant to verify that the same channel is reestablished.
It also exercises both payment directions: Node1 pays an offline hosted tenant while Public T
buffers and later completes the delivery, then the hosted tenant uses that inbound liquidity to
pay Node1 through Public T.

Run it locally with clean Fiber and CKB state:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp
```

In another terminal:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp -r --env test
```
