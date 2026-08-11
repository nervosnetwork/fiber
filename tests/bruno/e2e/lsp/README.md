# Hosted LSP lifecycle and payments

This workflow starts Node2 as a Public Trampoline node with the hosted LSP service and Biscuit
authentication enabled. Operator requests use an LSP administration token. Hosted wallet
requests use a token whose authority block contains `tenant("u1")` and call the standard Fiber
channel, invoice, and payment RPC methods without a `tenant_id` parameter.

The workflow registers and activates a tenant, then the tenant calls `open_channel` against
Public T with an explicit tenant-side `funding_amount`. It evicts and reactivates the tenant to
verify that the same private in-process channel is reestablished. It also exercises both payment
directions: the tenant creates a standard invoice which Public T registers with an LSP hint,
Node1 pays the offline hosted tenant while Public T buffers and later completes the delivery,
then the hosted tenant uses `send_payment` to pay Node1 through Public T.

The Biscuit public key and test-only bearer tokens are fixed fixtures in `tests/nodes/start.sh`
and `tests/bruno/environments/test.bru`. They are not production credentials.
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
