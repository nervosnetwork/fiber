# Cross-Chain Hub

## Roles

- Bitcoin user: lnd node lnd-bob
- CKB user: CFN node 1
- Cross-Chain Hub service provider: lnd node lnd-ingrid and CFN node 3

## Run Locally

1. Install [bitcoind](https://bitcoin.org/en/download), [lnd](https://github.com/lightningnetwork/lnd), and [jq](https://jqlang.github.io/jq/download/). Ensure that the executables are in your PATH.
2. Start Bitcoin and LND nodes using `tests/deploy/lnd-init/setup-lnd.sh`.
3. Start CKB and CFN nodes using `tests/nodes/start.sh`.
4. Go to `tests/bruno` and run the command `npm exec -- @usebruno/cli run e2e/cross-chain-hub -r --env test`.
