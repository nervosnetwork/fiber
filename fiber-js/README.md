# Fiber JS

This is a JavaScript wrapper over Fiber wasm, to make Fiber wasm usable in JavaScript projects.

## Build

In the root of fiber, run:
```
npm install
npm build -ws
```
After that, `fiber-js` will be ready to use in npm.

## APIs

`fiber-js` provide the same API ad Fiber RPC, see `fiber-js/src/index.ts` for details. For documentation, please refer to the docs of Fiber RPC.

For `open_channel_with_external_funding`, the JS wrapper also accepts the optional
`funding_source_extra_cell_deps` field so callers can provide extra lock deps for
custom funding-source scripts.

## Example

```js

import { Fiber, randomSecretKey } from "@nervosnetwork/fiber-js";

const fiber = new Fiber();

const testnetConfig = `
# This configuration file only contains the necessary configurations for the testnet deployment.
# All options' descriptions can be found via fnn --help and be overridden by command line arguments or environment variables.
fiber:
  # standalone_watchtower_rpc_url: http://127.0.0.1:23456
  # disable_built_in_watchtower: true
  listening_addr: "/ip4/127.0.0.1/tcp/8228"
  bootnode_addrs:
    - "/dns4/thrall.fiber.channel/tcp/443/wss/p2p/Qmes1EBD4yNo9Ywkfe6eRw9tG1nVNGLDmMud1xJMsoYFKy"
    - "/dns4/onyxia.fiber.channel/tcp/443/wss/p2p/QmdyQWjPtbK4NWWsvy8s69NGJaQULwgeQDT5ZpNDrTNaeV"
  announce_listening_addr: false
  announced_addrs:
    # If you want to announce your fiber node public address to the network, you need to add the address here, please change the ip to your public ip accordingly.
    # - "/ip4/YOUR-FIBER-NODE-PUBLIC-IP/tcp/8228"
  chain: testnet
  # lock script configurations related to fiber network
  # https://github.com/nervosnetwork/fiber-scripts/blob/main/deployment/testnet/migrations/2025-02-28-111246.json
  scripts:
    - name: FundingLock
      script:
        code_hash: 0x6c67887fe201ee0c7853f1682c0b77c0e6214044c156c7558269390a8afa6d7c
        hash_type: type
        args: 0x
      cell_deps:
        - type_id:
            code_hash: 0x00000000000000000000000000000000000000000000000000545950455f4944
            hash_type: type
            args: 0x3cb7c0304fe53f75bb5727e2484d0beae4bd99d979813c6fc97c3cca569f10f6
        - cell_dep:
            out_point:
              tx_hash: 0x5a5288769cecde6451cb5d301416c297a6da43dc3ac2f3253542b4082478b19b # ckb_auth
              index: 0x0
            dep_type: code
    - name: CommitmentLock
      script:
        code_hash: 0x740dee83f87c6f309824d8fd3fbdd3c8380ee6fc9acc90b1a748438afcdf81d8
        hash_type: type
        args: 0x
      cell_deps:
        - type_id:
            code_hash: 0x00000000000000000000000000000000000000000000000000545950455f4944
            hash_type: type
            args: 0xf7e458887495cf70dd30d1543cad47dc1dfe9d874177bf19291e4db478d5751b
        - cell_dep:
            out_point:
              tx_hash: 0x5a5288769cecde6451cb5d301416c297a6da43dc3ac2f3253542b4082478b19b #ckb_auth
              index: 0x0
            dep_type: code

rpc:
  # By default RPC only binds to localhost, thus it only allows accessing from the same machine.
  # Allowing arbitrary machines to access the JSON-RPC port is dangerous and strongly discouraged.
  # Please strictly limit the access to only trusted machines.
  listening_addr: "127.0.0.1:8227"

ckb:
  rpc_url: "https://testnet.ckbapp.dev/"
  udt_whitelist:
    - name: RUSD
      script:
        code_hash: 0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a
        hash_type: type
        args: 0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b
      cell_deps:
        - type_id:
            code_hash: 0x00000000000000000000000000000000000000000000000000545950455f4944
            hash_type: type
            args: 0x97d30b723c0b2c66e9cb8d4d0df4ab5d7222cbb00d4a9a2055ce2e5d7f0d8b0f
      auto_accept_amount: 1000000000

services:
  - fiber
  - rpc
  - ckb
`
const mainnetConfig = `
# This configuration file only contains the necessary configurations for the mainnet deployment.
# All options' descriptions can be found via fnn --help and be overridden by command line arguments or environment variables.
fiber:
  # standalone_watchtower_rpc_url: http://127.0.0.1:23456
  # disable_built_in_watchtower: true
  listening_addr: "/ip4/127.0.0.1/tcp/8228"
  bootnode_addrs:
    - "/dns4/garrosh.fiber.channel/tcp/443/wss/p2p/QmZ2gCTfEF6vKsiYFF2STPeA2rRLRim9nMtzfwiE7uMQ4v"
    - "/dns4/sylvanas.fiber.channel/tcp/443/wss/p2p/QmcMLnWraRyxd7PFRgvn1QeYRQS2DGsP6fPFCQjtfMs5b2"
  announce_listening_addr: false
  announced_addrs:
    # If you want to announce your fiber node public address to the network, you need to add the address here, please change the ip to your public ip accordingly.
    # - "/ip4/YOUR-FIBER-NODE-PUBLIC-IP/tcp/8228"
  chain: testnet
  # lock script configurations related to fiber network
  # https://github.com/nervosnetwork/fiber-scripts/blob/main/deployment/testnet/migrations/2025-02-28-111246.json
  scripts:
    - name: FundingLock
      script:
        code_hash: 0x6c67887fe201ee0c7853f1682c0b77c0e6214044c156c7558269390a8afa6d7c
        hash_type: type
        args: 0x
      cell_deps:
        - type_id:
            code_hash: 0x00000000000000000000000000000000000000000000000000545950455f4944
            hash_type: type
            args: 0x3cb7c0304fe53f75bb5727e2484d0beae4bd99d979813c6fc97c3cca569f10f6
        - cell_dep:
            out_point:
              tx_hash: 0x5a5288769cecde6451cb5d301416c297a6da43dc3ac2f3253542b4082478b19b # ckb_auth
              index: 0x0
            dep_type: code
    - name: CommitmentLock
      script:
        code_hash: 0x740dee83f87c6f309824d8fd3fbdd3c8380ee6fc9acc90b1a748438afcdf81d8
        hash_type: type
        args: 0x
      cell_deps:
        - type_id:
            code_hash: 0x00000000000000000000000000000000000000000000000000545950455f4944
            hash_type: type
            args: 0xf7e458887495cf70dd30d1543cad47dc1dfe9d874177bf19291e4db478d5751b
        - cell_dep:
            out_point:
              tx_hash: 0x5a5288769cecde6451cb5d301416c297a6da43dc3ac2f3253542b4082478b19b #ckb_auth
              index: 0x0
            dep_type: code

rpc:
  # By default RPC only binds to localhost, thus it only allows accessing from the same machine.
  # Allowing arbitrary machines to access the JSON-RPC port is dangerous and strongly discouraged.
  # Please strictly limit the access to only trusted machines.
  listening_addr: "127.0.0.1:8227"

ckb:
  rpc_url: "https://testnet.ckbapp.dev/"
  udt_whitelist:
    - name: RUSD
      script:
        code_hash: 0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a
        hash_type: type
        args: 0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b
      cell_deps:
        - type_id:
            code_hash: 0x00000000000000000000000000000000000000000000000000545950455f4944
            hash_type: type
            args: 0x97d30b723c0b2c66e9cb8d4d0df4ab5d7222cbb00d4a9a2055ce2e5d7f0d8b0f
      auto_accept_amount: 1000000000

services:
  - fiber
  - rpc
  - ckb
`

await fiber.start(testnetConfig, randomSecretKey(), randomSecretKey(), undefined, "info", "/wasm");
console.log(await fiber.invokeCommand("list_peers", []));

```
