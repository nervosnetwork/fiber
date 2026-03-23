# Cross-Chain Hub (Separate Service)

## Roles

- Bitcoin user: lnd node lnd-bob
- CKB user: FNN 1
- Fiber node: FNN 3 (fiber + ckb services only)
- Cross-Chain Hub service: Separate CCH node connecting to FNN 3 via RPC

## Overview

This test suite exercises the CCH running as a **separate process** from the Fiber node.
The CCH service connects to FNN 3's RPC endpoint to add invoices, send payments, and
receive store change notifications via WebSocket subscription.

## Run Locally

1. Install [bitcoind](https://github.com/bitcoin/bitcoin/releases), [lnd](https://github.com/lightningnetwork/lnd), and [jq](https://jqlang.github.io/jq/download/). Ensure that the executables are in your PATH.
2. Start Bitcoin and LND nodes using `tests/deploy/lnd-init/setup-lnd.sh`.
3. Start CKB and FNN using `tests/nodes/start.sh e2e/cross-chain-hub-separate`.
4. Go to `tests/bruno` and run the command `npm exec -- @usebruno/cli run e2e/cross-chain-hub-separate -r --env test`.
