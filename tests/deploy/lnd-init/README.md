# lnd init

Setup 1 bitcoind and 2 lnd nodes.

Install [bitcoind](https://bitcoin.org/en/download), [lnd](https://github.com/lightningnetwork/lnd), and [jq](https://jqlang.github.io/jq/download/). Ensure that the executables are in your PATH.

The nodes will have their own data directories:

- `bitcoind`: bitcoind node.
- `lnd-bob`: lnd node for the BTC user.
- `lnd-ingrid`: lnd node for the cross-chain hub operator.
