# Hosted LSP multi-asset channels

This workflow registers one hosted tenant and opens three private channels from
the same tenant to Public T:

- CKB
- SIMPLE_UDT
- XUDT

It also opens matching payer channels from Node1 to Public T, then creates one
hosted invoice per asset and pays each invoice through Public T. The final step
checks that the tenant has three online channels.

The SDK agent opens the tenant channels through the production external-signing
flow. `POST /prepare-channel` allocates the next channel key after the previous
channel has been bound, so the same tenant can open more than one channel.

The current UDT auto-accept path contributes 0 on Public T's side. After opening the two
UDT tenant channels, the workflow pays a Public T hold invoice on each channel. The fixture
keeps each preimage, authorizes the exact outgoing amount in the corresponding SDK channel,
and waits until Public T receives the payment. It then authorizes fulfillment before settling
Public T's invoice. Waiting for `Paid` before authorization would deadlock this direct channel:
Public T needs the tenant's signature to finish settlement. Both payment success and invoice
payment are checked before advancing to the next asset.
This gives Public T outbound UDT liquidity for the incoming invoices and exercises the
tenant-to-Public-T direction without bypassing the signer's payment policy.

Node1 explicitly connects to Public T before opening its payer channels. For each channel,
the workflow waits for the finalized funding transaction to enter the transaction pool before
generating confirmation epochs; `open_channel` returns before funding is broadcast.
Each hosted incoming
invoice is authorized on the matching asset channel and held until the SDK releases the
fixture preimage. The workflow checks both the payer's successful payment and the tenant's
paid invoice for all three assets.

Run it locally with clean Fiber and CKB state:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp-multi-asset
```

In another terminal:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp-multi-asset -r --env test
```
