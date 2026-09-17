# Hosted LSP remote-signer watchtower after evict

Same setup as `e2e/lsp-remote-signer-watchtower`, but the tenant runtime is
evicted before Public T force-closes the private U-T channel. The SDK agent
stays up and answers `get_watchtower_signing_status` /
`submit_watchtower_signature` while PeriodicCheck uses the last host watch
snapshot.

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp-remote-signer-watchtower-evict
```

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp-remote-signer-watchtower-evict -r --env test
```
