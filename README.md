# Fiber Network Node

Fiber Network Node (FNN) is a reference implementation of Fiber Network Protocol (FNP). In the current stage, it's not a production-ready implementation, but a prototype to demonstrate the feasibility of FNP. It's capable of:

* Establishing connections with other FNN
* Creating and closing fiber channel
* Payments over fiber channel (via [fiber-scripts])
* Cross-chain asset transfer

Please note that the implementation is still under development, there are many limitations and known issues, you may find or report them in the issue tracker.

But as a prototype, it's a good starting point for developers to understand the FNP and try out the integration with their applications.

## Build and run a testnet node

1. Build the project, if you are using the released binary, you can skip this step:

```
cargo build --release
```

2. Create a data folder for the node, then copy the built binary and testnet config file to it:

```
mkdir /folder-to/my-fnn
// if you are using the released binary, replace target/release/fnn with the path of released binary
cp target/release/fnn /folder-to/my-fnn
cp config/testnet/config.yml /folder-to/my-fnn
cd /folder-to/my-fnn
```

3. FNN has the built-in wallet functionality to sign funding transactions, let's create or import a private key first. The private key is stored in the data folder and named `ckb/key`. You may use the ckb-cli to generate a new key or export an existing key:

```
mkdir ckb
ckb-cli account export --lock-arg <lock_arg> --extended-privkey-path ./ckb/exported-key
// ckb-cli exports master private key and chain code, FNN only needs the private key part
head -n 1 ./ckb/exported-key > ./ckb/key
```

4. Start the node, by default it will output logs to the console, you may redirect it to a file:

```
RUST_LOG=info ./fnn -c config.yml -d .
```

## Testnet compatibility issues

The current state of the FNN is not stable, the protocol and storage format may changed between versions. We strongly recommend you to close the channel before upgrading the node, otherwise, you may lose the channel state:

1. [list all channels](./src/rpc/README.md#channel-list_channels) and [close](./src/rpc/README.md#channel-shutdown_channel) them via RPC.

2. Stop the node and remove the storage of the node:

```
rm -rf /folder-to/my-fnn/fiber/store
```

3. Repalce the fnn binary with the new version and start the node again.


If you want to keep the channel state, you may try to migrate the storage format manually:

1. Stop the node.

2. Backup the storage folder `/folder-to/my-fnn/fiber/store`.

3. Run the fnn-migrate (it can be found in the release binary package) to migrate the storage format:

```
fnn-migrate -p /folder-to/my-fnn/fiber/store
```

4. Repalce the fnn binary with the new version and start the node again.

## Documentation

* [Light Paper](./docs/light-paper.md)
* [RPC Documentation](./src/rpc/README.md)
* [P2P Message Protocol](./docs/specs/p2p-message.md)
* [Invoice Protocol](./docs/specs/payment-invoice.md)

**We are in a actively developing stage, don't hesitate to [report issues](https://github.com/nervosnetwork/fiber/issues) or ask for help in the [channel of the Nervos dev community](https://discord.gg/c5gntbFd).**

## Testnet deployment information

* TODO: Add testnet deployed nodes information *

[fiber-scripts]: https://github.com/nervosnetwork/fiber-scripts
