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
UDT tenant channels, the workflow sends a small UDT keysend from the tenant to Public T on
each channel. This gives Public T outbound UDT liquidity so it can deliver the incoming UDT
invoice. It also exercises the tenant-to-Public-T direction for each UDT.

Run it locally with clean Fiber and CKB state:


```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp-multi-asset
```

In another terminal:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp-multi-asset -r --env test
```
