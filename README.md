# Fiber Network Node

Fiber Network Node (FNN) is a reference implementation of Fiber Network Protocol (FNP). In the current stage, it's not a production-ready implementation, but a prototype to demonstrate the feasibility of FNP. It's capable of:

* Establishing connections with other FNN
* Creating and closing fiber channel
* Payments over fiber channel (via [fiber-scripts])
* Cross-chain asset transfer

Please note that the implementation is still under development, there are two major features not implemented yet:

* Watchtower service to monitor and revoke on-chain transactions
* Multihop payments

But as a prototype, it's a good starting point for developers to understand the FNP and try out the integration with their applications.

## Build and run a testnet node

1. Build the project:

```
cargo build --release
```

2. Create a data folder for the node, then copy the built binary and testnet config file to it:

```
mkdir /folder-to/my-fnn
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

## Documentation

* [RPC Documentation](./src/rpc/README.md)
* [P2P Message Protocol](./docs/specs/p2p-message.md)
* [Invoice Protocol](./docs/specs/payment-invoice.md)

## Testnet deployment information

* TODO: Add testnet deployed nodes information *

[fiber-scripts]: https://github.com/nervosnetwork/fiber-scripts
