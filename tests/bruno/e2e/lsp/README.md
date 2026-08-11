# Hosted LSP lifecycle

This workflow starts Node2 as a Public Trampoline node with the hosted LSP service enabled. It
registers and activates a tenant, opens a private in-process channel between Public T and that
tenant, then evicts and reactivates the tenant to verify that the same channel is reestablished.

Run it locally with clean Fiber and CKB state:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp
```

In another terminal:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp -r --env test
```
