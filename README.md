# CKB Fiber Network Node

CKB Fiber Network Node (cfn-node) is a reference implementation of a CKB Fiber Network protocol (cfn-protocol). In the current stage, it's not a production-ready implementation, but a prototype to demonstrate the feasibility of the cfn-protocol. It's capable of:

* Establishing connections with other cfn nodes
* Creating and closing fiber channel
* Payments over fiber channel (via [cfn-scripts])
* Cross-chain asset transfer

Please note that the implementation is still under development, there are two major features not implemented yet:

* Watchtower service to monitor and revoke on-chain transactions
* Multihop payments

But as a prototype, it's a good starting point for developers to understand the cfn-protocol and try out the integration with their applications.

## Build and Run

## RPC

## Testnet Deployment Information

* TODO: Add testnet deployed nodes information *

[cfn-scripts]: https://github.com/nervosnetwork/cfn-scripts
