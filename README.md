# Fiber Network Node

Fiber Network Node (FNN) is a reference node implementation of Fiber Network Protocol (FNP). Fiber network is a peer-to-peer payment/swap network (like [Lightning Network](https://lightning.network/)). We believe payment and swap are the two foundational primitives of finance.

Features

- **Multiple assets support**, e.g. stable coins, RGB++ assets issued on Bitcoin ledger, and UDT assets issued on CKB ledger;
- **Extremely low-cost micropayments**, e.g. 0.0001 cent payment with 0.00000001 cent fee;
- **Instant swap between any asset pairs**, as long as there's available channel paths;
- **Cross-network asset payment/swap**, e.g. from Lightning network to Fiber network, and vice versa;
- **Watchtower support**, make it easier for node operators;
- **Multi-hop payment**, anyone can facilitate payments and earn payment fees by running a fiber node and becoming a hop on payment paths;
- **Low latency**, e.g. 0.0001 cent payment in your p2p connection latency, e.g. 20ms;
- **High throughput**, because transactions are only processed by involved peers, no network consensus required;
- **High privacy**, your transactions are only seen by involved peers;
- **Based on more advanced cryptographic techniques** to ensure security and privacy, e.g. hash-based HTLC payment contracts today, with PTLC planned as a future direction;
- **Composable** with other contracts/scripts on CKB;

!!! The implementation is still a work-in-progress and will have iterations even after mainnet, please use it with caution and report any issues/limitations in the issue tracker.

TODO

- [x] Establishing connections with other fiber nodes
- [x] Creating and closing fiber channel
- [x] Payments over fiber channel (via [fiber-scripts])
- [x] Basic Multi-Path Payment (MPP)
- [x] Cross-network asset transfer
- [x] Web-browser friendly runtime (WASM/[fiber-js], productization in progress)
- [ ] Programmable conditional payment — official roadmap item covering PTLC, hold-invoice application protocols, on-chain contract conditions, and agent fair exchange
- [ ] Advanced channel liquidity management — under active development (Loop In/Out and provider mode), not yet released
- [ ] Atomic multi-path payment — long-term research direction (Basic MPP already released)

## Quick Install (One-liner)

```bash
# Install to default location (~/.fiber)
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.sh | bash

# Install mainnet non-interactively with a trusted CKB RPC endpoint
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.sh | INSTALL_DIR=/opt/fiber FNN_VERSION=0.9.0 NETWORK=mainnet CKB_RPC_URL=https://your-trusted-mainnet-ckb-rpc.example bash
```

Mainnet installations do not select a public CKB RPC endpoint automatically.
The guided installer prompts for one; bootstrap or non-interactive installations require
`CKB_RPC_URL` to be set explicitly.

## Build from source and run a testnet node

1. Build the project, if you are using the released binary, you can skip this step:

```bash
cargo build --release
```

2. Create a data folder for the node, then copy the built binary and testnet config file to it:

```bash
mkdir /folder-to/my-fnn
// if you are using the released binary, replace target/release/fnn with the path of released binary
cp target/release/fnn /folder-to/my-fnn
cp config/testnet/config.yml /folder-to/my-fnn
cd /folder-to/my-fnn
```

3. FNN has the built-in wallet functionality to sign funding transactions, let's create or import a private key first. The private key is stored in the data folder and named `ckb/key`. You may use the ckb-cli to generate a new key or export an existing key:

```bash
mkdir ckb
ckb-cli account export --lock-arg <lock_arg> --extended-privkey-path ./ckb/exported-key
// ckb-cli exports master private key and chain code, FNN only needs the private key part
head -n 1 ./ckb/exported-key > ./ckb/key
// the exported key file can be deleted after the private key is extracted
rm ./ckb/exported-key
```

4. Start the node, by default it will output logs to the console, you may redirect it to a file. Before starting, you must set the `FIBER_SECRET_KEY_PASSWORD` environment variable to encrypt the wallet private key file (which was stored in plain text during the previous step). Additionally, you can configure logging verbosity with the `RUST_LOG` environment variable—use predefined levels like `info`, `debug`, or `trace`, or specify granular logging with patterns such as `info,fnn=debug` to enable debug logs only for the FNN module.

```bash
FIBER_SECRET_KEY_PASSWORD='YOUR_PASSWORD' RUST_LOG='info' ./fnn -c config.yml -d .
```

## Run with Docker

Fiber release tags publish official container images to Docker Hub at `nervos/fiber` and mirror the same tags to GHCR at `ghcr.io/nervosnetwork/fiber`. Detailed Docker usage, including startup, local builds, `fnn-cli`, and migration, is documented in [docker/README.md](./docker/README.md).

## Testnet compatibility issues

FNN is still under development, the protocol and storage format may changed between versions. We strongly recommend you to close the channel before upgrading the node, otherwise, you may lose channel data and funds!

1. [list all channels](./crates/fiber-lib/src/rpc/README.md#channel-list_channels) and [close](./crates/fiber-lib/src/rpc/README.md#channel-shutdown_channel) them via RPC.

2. Stop the node and remove the storage of the node:

```bash
rm -rf /folder-to/my-fnn/fiber/store
```

3. Replace the fnn binary with the new version and start the node again.


If you want to keep the channel state, check whether your upgrade path supports
in-place migration before replacing binaries. Since v0.9.0, supported storage
migrations are integrated into the `fnn` startup process, and release bundles no
longer include a standalone `fnn-migrate` binary.

1. Stop the node.

2. Backup the storage folder `/folder-to/my-fnn/fiber/store`.

3. Replace the `fnn` binary with the new version and start the node again.

If the new `fnn` reports that the database is too old for the built-in migration
path, use the legacy `fnn-migrate` tool from the matching v0.8.x release first.
For the v0.7.x to v0.8.0 legacy migration flow, see
[v0.8.0 Migration Guide](./docs/notes/v0.8.0-migration-guide.md).

## Documentation

* [Light Paper](./docs/light-paper.md)
* [Glossary](./docs/glossary.md)
* [RPC Documentation](./crates/fiber-lib/src/rpc/README.md)
* [P2P Message Protocol](./docs/specs/p2p-message.md)
* [Invoice Protocol](./docs/specs/payment-invoice.md)

**We are in an actively developing stage, don't hesitate to [report issues](https://github.com/nervosnetwork/fiber/issues) or ask for help in the [channel of the Nervos dev community](https://discord.gg/BF9AJ4fzs6).**

## Network node documentation

* [Public Nodes User Manual](docs/public-nodes.md)
* [Fiber Network Nodes](docs/network-nodes.md)
* [Fiber WSS Configuration Manual](docs/fiber-node-wss.md)

[fiber-scripts]: https://github.com/nervosnetwork/fiber-scripts
[fiber-js]: https://www.npmjs.com/package/@nervosnetwork/fiber-js
