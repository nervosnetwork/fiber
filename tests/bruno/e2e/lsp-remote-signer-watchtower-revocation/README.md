# Hosted LSP remote-signer watchtower revocation

Opens an external-signer U-T channel, registers wallet-owned invoice terms with
the SDK, and creates a hold invoice without disclosing its preimage. After
receipt, the agent checks safe preimage release and calls `settle_invoice`.
Completing the inbound payment advances the
commitment number beyond 1. The driver first asserts complete SDK revocation
evidence for the old peer state, then Public T broadcasts commitment
`0x1` (T's own stale state). The tenant watch row should spend that output
with the pre-aggregated revocation signature stored when T's `RevokeAndAck`
was applied.

A `Settlement snapshot hash does not match commitment lock` warning from
Public T's own watch row is expected: that row's key order does not match T's
broadcastable commitment lock. The justice path is the tenant row.

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp-remote-signer-watchtower-revocation
```

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp-remote-signer-watchtower-revocation -r --env test
```
