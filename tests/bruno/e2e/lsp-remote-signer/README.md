# Hosted LSP remote signer E2E

This workflow starts `fiber-lsp-sdk-agent` as a separate process and exercises the real
`fiber-lsp-sdk` over JSON-RPC. The agent owns its RootKey and channel keys, registers a
RootSigner-derived tenant, opens a private hosted channel with external funding, polls
and answers node signing requests with `prepare_bound`, and persists its SDK snapshot.

The workflow deliberately restarts the agent after the channel reaches `ChannelReady`,
checks that the RootSigner identity, tenant token, and channel binding are restored, and
then completes a cooperative close using signatures produced by the restarted process.
The funding wallet signature remains a separate test fixture because it is outside the
channel signer boundary. Watchtower external signing is not part of this phase.

Run it with:

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp-remote-signer
```

Then, in another terminal:

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp-remote-signer -r --env test
```
