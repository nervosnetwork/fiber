# Fiber Network Node

Fiber Network Node (FNN) is a reference implementation of Fiber Network Protocol (FNP). In the current stage, it's not a production-ready implementation, but a prototype to demonstrate the feasibility of FNP. It's capable of:

* Establishing connections with other cfn nodes
* Creating and closing fiber channel
* Payments over fiber channel (via [cfn-scripts])
* Cross-chain asset transfer

Please note that the implementation is still under development, there are two major features not implemented yet:

* Watchtower service to monitor and revoke on-chain transactions
* Multihop payments

But as a prototype, it's a good starting point for developers to understand the cfn-protocol and try out the integration with their applications.

## Build and run a testnet node

1. Build the project:

```
cargo build --release
```

2. Create a data folder for the node, then copy the built binary and testnet config file to it:

```
mkdir /folder-to/my-cfn-node
cp target/release/cfn-node /folder-to/my-cfn-node
cp config/testnet/config.yml /folder-to/my-cfn-node
cd /folder-to/my-cfn-node
```

3. FNN has the built-in wallet functionality to sign funding transactions, let's create or import a private key first. The private key is stored in the data folder and named `fiber/key`. You may use the ckb-cli to generate a new key or export an existing key:

```
mkdir ckb
ckb-cli account export --lock-arg <lock_arg> --extended-privkey-path ./fiber/exported-key
// ckb-cli exports master private key and chain code, cfn node only needs the private key part
head -n 1 ./fiber/exported-key > ./fiber/key
```

4. Start the node, by default it will output logs to the console, you may redirect it to a file:

```
RUST_LOG=info ./cfn-node -c config.yml -d .
```

## Documentation

* [RPC Documentation](./src/rpc/README.md)
* [P2P Message Protocol](./docs/specs/p2p-message.md)
* [Invoice Protocol](./docs/specs/payment-invoice.md)

## Testnet deployment information

* TODO: Add testnet deployed nodes information *

[cfn-scripts]: https://github.com/nervosnetwork/cfn-scripts
