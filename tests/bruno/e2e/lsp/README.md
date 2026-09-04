# Hosted LSP lifecycle and payments

This workflow starts Node2 as a Public Trampoline node with the hosted LSP service and Biscuit
authentication enabled. The SDK agent uses the LSP operator token for the nonce/registration
bootstrap and exposes the RootSigner-derived tenant id and issued tenant token to Bruno.
Operator requests manage tenant runtime lifecycle through the LSP administration RPCs. Invoice
creation and outgoing payment use the standard Fiber `new_invoice` and `send_payment` methods;
the tenant Biscuit selects the actor and Store namespace without a `tenant_id` request parameter.

The workflow registers and activates a tenant, then Public T calls the standard `open_channel`
RPC to create a private in-process U-T channel with outbound liquidity toward the tenant. It
evicts and reactivates the tenant to verify that the same channel is reestablished. It also
exercises both payment directions: the tenant calls `new_invoice` with
`lsp_buffer_duration_ms`, which automatically registers the invoice and Public T trampoline hint;
Node1 pays the offline hosted tenant while Public T buffers and later completes the delivery;
then the tenant calls `send_payment` to pay Node1 through Public T. Tenant-token RPCs verify the
resulting channel, invoice, and payment state in the tenant namespace.

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
