# Hosted LSP lifecycle and payments

This workflow starts Node2 as a Public Trampoline node with the hosted LSP service and Biscuit
authentication enabled. The SDK agent uses the LSP operator token for the nonce/registration
bootstrap and exposes the RootSigner-derived tenant id and issued tenant token to Bruno.
Operator requests manage tenant runtime lifecycle through the LSP administration RPCs. Invoice
creation and outgoing payment use the standard Fiber `new_invoice` and `send_payment` methods;
the tenant Biscuit selects the actor and Store namespace without a `tenant_id` request parameter.

The workflow registers and activates a tenant, then its SDK agent opens an externally funded
private U-T channel. The Node1 fixture wallet signs the funding transaction, and the tenant
submits it with its own token. Public T auto-accepts; hosted tenants reject inbound offers. It
evicts and reactivates the tenant to verify that the same channel is reestablished. It also
exercises both payment directions: the tenant calls `new_invoice`, which automatically
registers the invoice and Public T trampoline hint using the service buffer policy;
Node1 pays the offline hosted tenant while Public T buffers and later completes the delivery;
then the tenant calls `send_payment` to pay Node1 through Public T. Tenant-token RPCs verify the
resulting channel, invoice, and payment state in the tenant namespace. The test explicitly
authorizes the hold invoice in the SDK, waits for receipt, and releases its preimage only
after the SDK verifies recovery against CKB. Outgoing intent authorizes exactly 100000 plus
the configured 500 routing fee budget before calling `send_payment`. After the recipient
invoice is paid, its fixture preimage explicitly authorizes the outgoing TLC settlement.

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
