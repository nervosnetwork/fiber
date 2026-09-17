# Hosted LSP remote-signer watchtower preimage claim

Opens an external-signer U-T channel, pays a hosted hold invoice so an inbound
TLC is pending, force-closes from Public T, then the tenant pushes the preimage
with `create_preimage`. The SDK agent signs the watchtower settlement that
claims that TLC.

The hold invoice uses a fixed CKB-hash pair:

- preimage `0x2222…22`
- payment_hash `0x09dfb3a3fa5af1b3359b6256945af43e31be9f86f3d1b45779cebc09dc609138`

```bash
REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/lsp-remote-signer-watchtower-preimage
```

```bash
cd tests/bruno
npm exec -- @usebruno/cli@1.20.0 run e2e/lsp-remote-signer-watchtower-preimage -r --env test
```
