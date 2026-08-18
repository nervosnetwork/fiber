# Hosted LSP lifecycle and payments

This workflow starts Node2 as a Public Trampoline node with the hosted LSP service and Biscuit
authentication enabled. Operator requests use an LSP administration token. Hosted wallet
requests use the tenant token issued on first `lsp_register_tenant` and call the standard Fiber
channel, invoice, and payment RPC methods without a `tenant_id` parameter.

The workflow registers and activates a tenant, then the tenant calls `open_channel` against
Public T with an explicit tenant-side `funding_amount`. It evicts and reactivates the tenant to
verify that the same private in-process channel is reestablished. It also exercises both payment
directions: the tenant creates a standard invoice which Public T registers with an LSP hint,
Node1 pays the offline hosted tenant while Public T buffers and later completes the delivery,
then the hosted tenant uses `send_payment` to pay Node1 through Public T.

The Biscuit key pair and operator bearer token are test-only fixtures. The workflow does not
carry a fixed tenant token: `lsp_register_tenant` issues it on first registration, and Bruno
stores the response for the remaining tenant-scoped requests. They are not production credentials.
For this payment topology, Public T auto-accepts each CKB channel with 500 CKB so that both the
U-T and Node1-T channels have usable outbound liquidity in both directions.

Run it locally with clean Fiber and CKB state:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp
```

In another terminal:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp -r --env test
```
