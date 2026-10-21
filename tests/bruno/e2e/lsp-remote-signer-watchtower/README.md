# Hosted LSP remote-signer watchtower E2E

Reuses the `e2e/lsp-remote-signer` setup (independent SDK agent + hosted
private channel). Bruno does not call watchtower RPCs. The tenant Fiber
registers the watch on `RemoteTxComplete`, and the agent polls
`get_watchtower_signing_status` / `submit_watchtower_signature`.

This suite force-closes the channel from Public T, mines through the
commitment delay, and checks that watchtower settlement spent the
commitment output and the tenant channel is `Closed`. Before force-closing, the
driver explicitly authorizes settlement to Public T’s sponsor wallet with a
10,000-shannon total fee cap. The final assertion requires an accepted SDK
watchtower signature; a chain-only settlement cannot make this test pass.

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp-remote-signer-watchtower
```

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp-remote-signer-watchtower -r --env test
```
