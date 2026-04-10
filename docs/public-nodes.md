# Public Nodes User Manual

> **Version note:** This document is for fnn `v0.8.0` and later. It is **not** compatible with earlier versions (e.g. `v0.7.1`), because [PR #1154](https://github.com/nervosnetwork/fiber/pull/1154) replaced `PeerId` with `Pubkey` across all RPC interfaces — the `peer_id` parameter in `connect_peer`, `open_channel`, `list_channels`, etc. was renamed to `pubkey`. For earlier versions, please see [docs/testnet-nodes.md@v0.7.1](https://github.com/nervosnetwork/fiber/blob/v0.7.1/docs/testnet-nodes.md).

## Public Nodes

### Mainnet

#### node1

- pubkey: `03a8d7da8d0934363dbc17f52c872e8d833016415266eabb3527439c5dd17adc6b`

#### node2

- pubkey: `033a69e5be369dab43aefa96fa729d83c571ccb066f312136c6ab2d354fcc028f9`

### Testnet

#### node1

- pubkey: `02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71`

#### node2

- pubkey: `0291a6576bd5a94bd74b27080a48340875338fff9f6d6361fe6b8db8d0d1912fcc`



## Overview

In this guide, we will deploy two local nodes (nodeA and nodeB) and connect them to the public relay nodes (node1 and node2) to perform multi-hop payments. The overall topology is:

```
┌───────┐        ┌───────┐        ┌───────┐        ┌───────┐
│ nodeA │ ─────▶ │ node1 │ ─────▶ │ node2 │ ─────▶ │ nodeB │
│:8227  │        │public │        │public │        │:8237  │
└───────┘        └───────┘        └───────┘        └───────┘
  sender        relay node 1     relay node 2      receiver
```

In this demo, nodeA and nodeB run on the same machine for convenience. In a real-world scenario, they would be on separate machines, each connecting only to a public relay node. The benefit is that local nodes do not need to expose a publicly reachable address — they can send and receive payments through the public relay nodes.



## Channel Capacity

When opening a channel, each side must reserve 99 CKB (98 CKB for [commitment lock occupied capacity](https://github.com/nervosnetwork/fiber/blob/2ab20ffb50243c25109a62ef2ac18b7e4f1a9e70/crates/fiber-lib/src/fiber/config.rs#L23) + 1 CKB for [shutdown transaction fee](https://github.com/nervosnetwork/fiber/blob/2ab20ffb50243c25109a62ef2ac18b7e4f1a9e70/crates/fiber-lib/src/fiber/config.rs#L18)) to ensure sufficient funds for on-chain settlement when the channel closes. This reserved amount is not available for off-chain payments.

For example, if nodeA funds 499 CKB and the public node contributes 250 CKB:

- nodeA available: 499 - 99 = 400 CKB
- public node available: 250 - 99 = 151 CKB



## Local Node Deployment

1. Download fnn

   Please visit the [releases page](https://github.com/nervosnetwork/fiber/releases) to download and use the latest version of fnn.

   ```bash
   mkdir tmp && cd tmp
   tar xzvf fnn-latest.tar.gz
   ```



2. Export the account private key to the fiber node's ckb directory

   Here, ckb-cli is used to create accounts, which will later be used to fund channel openings between local nodes and public nodes. If ckb-cli is not installed, please download it from the [releases](https://github.com/nervosnetwork/ckb-cli/releases).

   First, set your working directory based on the target network:

   ```bash
   # For mainnet:
   FNNDIR=mainnet-fnn
   # For testnet (uncomment and comment out the line above):
   # FNNDIR=testnet-fnn
   ```

   Then create directories and export keys:

   ```bash
   # Create local node directories
   mkdir -p $FNNDIR/nodeA/ckb $FNNDIR/nodeB/ckb
   
   # Create accounts for nodeA and nodeB (replace <lock_arg_a/b> with the lock_arg from each output)
   ./ckb-cli account new  # -> nodeA account
   ./ckb-cli account export --lock-arg <lock_arg_a> --extended-privkey-path exported-key-a
   head -n 1 ./exported-key-a > $FNNDIR/nodeA/ckb/key && chmod 600 $FNNDIR/nodeA/ckb/key
   
   ./ckb-cli account new  # -> nodeB account
   ./ckb-cli account export --lock-arg <lock_arg_b> --extended-privkey-path exported-key-b
   head -n 1 ./exported-key-b > $FNNDIR/nodeB/ckb/key && chmod 600 $FNNDIR/nodeB/ckb/key
   
   # Verify keys
   ./ckb-cli util key-info --privkey-path $FNNDIR/nodeA/ckb/key
   ./ckb-cli util key-info --privkey-path $FNNDIR/nodeB/ckb/key
   ```



3. Copy and configure config.yml

   **Mainnet**

   ```bash
   cp config/mainnet/config.yml $FNNDIR/nodeA
   cp config/mainnet/config.yml $FNNDIR/nodeB
   ```

   As the comment says, "[use a trusted CKB RPC node](https://github.com/nervosnetwork/fiber/blob/2ab20ffb50243c25109a62ef2ac18b7e4f1a9e70/config/mainnet/config.yml#L55)" should be changed to the RPC endpoint of a CKB node you trust. For convenience, I used [Public JSON RPC nodes](https://github.com/nervosnetwork/ckb/wiki/Public-JSON-RPC-nodes) here.

   ```bash
   sed -i.bak 's|rpc_url:.*|rpc_url: "https://mainnet.ckbapp.dev/"|' $FNNDIR/nodeA/config.yml
   grep rpc_url $FNNDIR/nodeA/config.yml
   ```

   For nodeB, also modify `rpc_url` and change the listening ports to avoid conflicts with nodeA:

   ```bash
   sed -i.bak 's|rpc_url:.*|rpc_url: "https://mainnet.ckbapp.dev/"|' $FNNDIR/nodeB/config.yml
   sed -i.bak 's|/ip4/0.0.0.0/tcp/8228|/ip4/0.0.0.0/tcp/8238|' $FNNDIR/nodeB/config.yml
   sed -i.bak 's|127.0.0.1:8227|127.0.0.1:8237|' $FNNDIR/nodeB/config.yml
   grep -wE 'rpc_url|listening_addr' $FNNDIR/nodeB/config.yml
   ```

   **Testnet**

   ```bash
   cp config/testnet/config.yml $FNNDIR/nodeA
   cp config/testnet/config.yml $FNNDIR/nodeB
   ```

   The testnet config already has `rpc_url` set to `https://testnet.ckbapp.dev/`, so only nodeB's listening ports need to be changed:

   ```bash
   sed -i.bak 's|/ip4/0.0.0.0/tcp/8228|/ip4/0.0.0.0/tcp/8238|' $FNNDIR/nodeB/config.yml
   sed -i.bak 's|127.0.0.1:8227|127.0.0.1:8237|' $FNNDIR/nodeB/config.yml
   grep -wE 'rpc_url|listening_addr' $FNNDIR/nodeB/config.yml
   ```



4. Fund the accounts

   **Mainnet**

   Transfer CKB to both nodeA's and nodeB's addresses. Each node needs enough CKB to open a channel. The funding amount is 499 CKB, but additional CKB is required for the change cell (a standard secp256k1-blake160 cell occupies a minimum of 61 CKB) and the on-chain funding transaction fee. We recommend transferring at least 561 CKB per node (499 + 61 + 1 fee).

   **Testnet**

   Use the faucets to fund both nodeA's and nodeB's addresses:

    - CKB: https://faucet.nervos.org
    - RUSD (for UDT channels): https://testnet0815.stablepp.xyz/faucet

   The RUSD faucet cannot directly fill an address, so you can first claim RUSD through a wallet like [JoyID](https://testnet.joyid.dev/), then transfer it to your node's address.



5. Start the nodes

   You need to set the `FIBER_SECRET_KEY_PASSWORD` environment variable in the startup command. This is a user-chosen password used to encrypt and decrypt your CKB private key file — you can set it to any value you like. Please use a strong password and remember it, as you will need it every time you start the node.

   ```bash
   FIBER_SECRET_KEY_PASSWORD='your_password' RUST_LOG=info ./fnn -c $FNNDIR/nodeA/config.yml -d $FNNDIR/nodeA > $FNNDIR/nodeA/a.log 2>&1 &
   FIBER_SECRET_KEY_PASSWORD='your_password' RUST_LOG=info ./fnn -c $FNNDIR/nodeB/config.yml -d $FNNDIR/nodeB > $FNNDIR/nodeB/b.log 2>&1 &
   ```



---

## CKB Multi-Hop Payment

The CKB payment flow is identical on both mainnet and testnet. The only differences are the public node pubkeys and the invoice currency code. Set the following variables for your target network before running the commands below:

```bash
# For mainnet:
NODE1_PUBKEY="03a8d7da8d0934363dbc17f52c872e8d833016415266eabb3527439c5dd17adc6b"
NODE2_PUBKEY="033a69e5be369dab43aefa96fa729d83c571ccb066f312136c6ab2d354fcc028f9"
CURRENCY="Fibb"

# For testnet (uncomment and comment out the three lines above):
# NODE1_PUBKEY="02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71"
# NODE2_PUBKEY="0291a6576bd5a94bd74b27080a48340875338fff9f6d6361fe6b8db8d0d1912fcc"
# CURRENCY="Fibt"
```


### Establishing a CKB Channel: nodeA ⟺ node1


1. Connect nodeA to node1

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 1,
       "jsonrpc": "2.0",
       "method": "connect_peer",
       "params": [
           {
               "pubkey": "'$NODE1_PUBKEY'"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":null,"id":1}
   ```



2. Open a channel with 499 CKB: nodeA (400 CKB) ⟺ node1 (151 CKB)

   _Both mainnet and testnet node1 have `open_channel_auto_accept_min_ckb_funding_amount` set to 400 CKB, so please provide 499 CKB or more._

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 2,
       "jsonrpc": "2.0",
       "method": "open_channel",
       "params": [
           {
               "pubkey": "'$NODE1_PUBKEY'",
               "funding_amount": "0xb9e459300",
               "public": true
           }
       ]
   }'
   ```

   Example response:

   ```json
   {"jsonrpc":"2.0","id":2,"result":{"temporary_channel_id":"0x9b6811b43770f476a84662fa66779cc6008b10b0863af308148b3d0c9cbe03b7"}}
   ```



3. Query channels and wait for `ChannelReady`

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 3,
       "jsonrpc": "2.0",
       "method": "list_channels",
       "params": [
           {
               "pubkey": "'$NODE1_PUBKEY'"
           }
       ]
   }'
   ```

   Wait until the `state_name` changes to `ChannelReady`.

   **Note: If you attempt to call `send_payment` immediately after the channel enters the `ChannelReady` state, you may still encounter the error `Failed to build route`. Wait a moment and try again.**

   Example response (testnet):

   ```json
   {"jsonrpc":"2.0","id":3,"result":{"channels":[{"channel_id":"0x8cfd1718dace307c1c7eee756773fee9ded3bae8a3808fc207e078ac5f190413","is_public":true,"is_acceptor":false,"is_one_way":false,"channel_outpoint":"0x31b242023b98f2b460fc83cd0b9c3165c446bc129571dbc269d6e3f22d67b38600000000","pubkey":"02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71","funding_udt_type_script":null,"state":{"state_name":"ChannelReady"},"local_balance":"0x9502f9000","offered_tlc_balance":"0x0","remote_balance":"0x38407b700","received_tlc_balance":"0x0","pending_tlcs":[],"latest_commitment_transaction_hash":"0x586a486509015da605d403f184885c69c0a16e5e651e93d7e03a7f1a5378710a","created_at":"0x19d72fe29f6","enabled":true,"tlc_expiry_delta":"0xdbba00","tlc_fee_proportional_millionths":"0x3e8","shutdown_transaction_hash":null,"failure_detail":null}]}}
   ```

   `local_balance` is `0x9502f9000` (400 CKB) and `remote_balance` is `0x38407b700` (151 CKB). See [Channel Capacity](#channel-capacity) for why these differ from the funded amounts. Take note of these initial balances for comparison after payments.



### Establishing a CKB Channel: nodeB ⟺ node2


1. Connect nodeB to node2

   ```bash
   curl -s --location 'http://127.0.0.1:8237' --header 'Content-Type: application/json' --data '{
       "id": 1,
       "jsonrpc": "2.0",
       "method": "connect_peer",
       "params": [
           {
               "pubkey": "'$NODE2_PUBKEY'"
           }
       ]
   }'
   ```




2. Open a channel with 499 CKB: nodeB (400 CKB) ⟺ node2 (151 CKB)

   _Node2 also has `open_channel_auto_accept_min_ckb_funding_amount` set to 400 CKB, so please provide 499 CKB or more._

   ```bash
   curl -s --location 'http://127.0.0.1:8237' --header 'Content-Type: application/json' --data '{
       "id": 2,
       "jsonrpc": "2.0",
       "method": "open_channel",
       "params": [
           {
               "pubkey": "'$NODE2_PUBKEY'",
               "funding_amount": "0xb9e459300",
               "public": true
           }
       ]
   }'
   ```



3. Query channels and wait for `ChannelReady`

   ```bash
   curl -s --location 'http://127.0.0.1:8237' --header 'Content-Type: application/json' --data '{
       "id": 3,
       "jsonrpc": "2.0",
       "method": "list_channels",
       "params": [
           {
               "pubkey": "'$NODE2_PUBKEY'"
           }
       ]
   }'
   ```

   Wait until the `state_name` changes to `ChannelReady`. Same as nodeA's channel: `local_balance` `0x9502f9000` (400 CKB), `remote_balance` `0x38407b700` (151 CKB).



### Multi-Hop Payment: nodeA → node1 → node2 → nodeB

Now that both channels are established, we can send a multi-hop payment from nodeA to nodeB through the public nodes (see [Overview](#overview)). Since the RPC endpoints of node1 and node2 are not publicly accessible, we can only query balance changes on nodeA (port 8227) and nodeB (port 8237). The total fee charged by the intermediate nodes (node1 + node2) can be inferred from the difference.


1. Generate an invoice on nodeB

   Set the amount to 0x5f5e100 (100,000,000 shannon), equivalent to 1 CKB. The `payment_preimage` should be a unique 32-byte hex value.

   ```bash
   # Generate a 32-byte random number and represent it in hexadecimal
   payment_preimage="0x$(openssl rand -hex 32)"
   echo $payment_preimage
   ```

   ```bash
   curl -s --location 'http://127.0.0.1:8237' --header 'Content-Type: application/json' --data '{
       "id": 1,
       "jsonrpc": "2.0",
       "method": "new_invoice",
       "params": [
           {
               "amount": "0x5f5e100",
               "currency": "'$CURRENCY'",
               "description": "test invoice generated by nodeB",
               "expiry": "0xe10",
               "final_cltv": "0x28",
               "payment_preimage": "'$payment_preimage'",
               "hash_algorithm": "sha256"
           }
       ]
   }'
   ```

   Record the `invoice_address` from the response.

   Example response (testnet):

   ```json
   {"jsonrpc":"2.0","id":1,"result":{"invoice_address":"fibt1000000001p6s4agtplhxgfw0pnm6vknn46ej0nm48uyg8gfz86h906c6ju2sns33h4ugqwguntc0ls000wg5k2uw5agxseteza9r98uwveq5cglqvsfj0ukn8cdtytfswvs65usjnhxhfs5sqfc8hsz9d3sv4k07ssn8z8djpdu06fu8x8flf3gv9pmp4vzzn943k0cqrhm34yvdcfy5va275k9zt7rxl3vl4l8aqghkmvnpzlwypl79knrtksh2hqyfgshfl7hfswv4gw6xgqymrl0g0t64pjm32p9qd80e2nm8ge3fnlpv4mrn5y3nw8mpl04htsv7js39tf3lyu60r4z5yslxcvepgdgmgph03hsz3l7upeqna955sqv34m0s","invoice":{"currency":"Fibt","amount":"0x5f5e100","signature":"041b031f0f080f0b1a1501121b110a0105000d070f190a131b0708191109131f010c151b0313140411130e071b011f0f15170b100c1e121011050b09111f041c1a0f0315021404101f06180c1901080d081b0801170f11171002111f1e1c011900131d0514141000","data":{"timestamp":"0x19d7301e572","payment_hash":"0x7d3966a6cc741c21d1a65e707ba876ca1226e23e7e1335ddb8d2e9b1c5f2cc14","attrs":[{"description":"test invoice generated by nodeB"},{"expiry_time":"0xe10"},{"final_htlc_minimum_expiry_delta":"0x927c00"},{"hash_algorithm":"sha256"},{"payee_public_key":"02e92cbef1c29a8c49eff0148a92e044a484f2d28ffd98aed72c630e441d4a1a25"}]}}}}
   ```

2. Send payment from nodeA to nodeB

   Pass in the previously recorded `invoice_address` to the `send_payment` request on nodeA

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 2,
       "jsonrpc": "2.0",
       "method": "send_payment",
       "params": [
           {
               "invoice": "<invoice_address from step 1>"
           }
       ]
   }'
   ```

   Example response (testnet):

   ```json
   {"jsonrpc":"2.0","id":3,"result":{"payment_hash":"0x7d3966a6cc741c21d1a65e707ba876ca1226e23e7e1335ddb8d2e9b1c5f2cc14","status":"Created","created_at":"0x19d730291e3","last_updated_at":"0x19d730291e3","failed_error":null,"fee":"0x30da4","custom_records":null}
   ```



3. Repeat Steps 1 and 2 two more times

   Perform two additional `new_invoice` (on nodeB) and `send_payment` (on nodeA) requests, keeping the amount set to 0x5f5e100.



4. Query channel balances after payments

   nodeA ⟺ node1

   Balances changed from `{"local_balance":"0x9502f9000","remote_balance":"0x38407b700"}` to `{"local_balance":"0x93e44c414","remote_balance":"0x395f282ec"}`

   nodeB ⟺ node2

   Balances changed from `{"local_balance":"0x9502f9000","remote_balance":"0x38407b700"}` to `{"local_balance":"0x962113300","remote_balance":"0x372261400"}`



This means the channel balances have changed as follows before and after the payments:

- Before payments

  nodeA (40000000000) ⟺ node1 (15100000000)

  nodeB (40000000000) ⟺ node2 (15100000000)

- After payments

  nodeA (39699399700) ⟺ node1 (15400600300)

  nodeB (40300000000) ⟺ node2 (14800000000)

Funds changes:

​	nodeA: 39699399700 - 40000000000 = -300600300

​	nodeB: 40300000000 - 40000000000 = +300000000

​	Total intermediate fees (node1 + node2): 300600300 - 300000000 = 600300

**Conclusion: Three CKB payments of 100,000,000 shannon (1 CKB) each from nodeA → node1 → node2 → nodeB were successfully completed. nodeA paid a total of 300,600,300 shannon, nodeB received 300,000,000 shannon, and the two intermediate relay nodes (node1 + node2) earned a combined fee of 600,300 shannon.**



5. Close the channels

   Replace `<channel_id_a>` and `<channel_id_b>` with the `channel_id` values from the `list_channels` responses above.

   Close nodeA's channel with node1:

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 5,
       "jsonrpc": "2.0",
       "method": "shutdown_channel",
       "params": [
           {
               "channel_id": "<channel_id_a>",
               "close_script": {
                   "code_hash": "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
                   "hash_type": "type",
                   "args": "<lock_arg_a>"
               },
               "fee_rate": "0x3FC"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":null,"id":5}
   ```

   Close nodeB's channel with node2:

   ```bash
   curl -s --location 'http://127.0.0.1:8237' --header 'Content-Type: application/json' --data '{
       "id": 5,
       "jsonrpc": "2.0",
       "method": "shutdown_channel",
       "params": [
           {
               "channel_id": "<channel_id_b>",
               "close_script": {
                   "code_hash": "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
                   "hash_type": "type",
                   "args": "<lock_arg_b>"
               },
               "fee_rate": "0x3FC"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":null,"id":5}
   ```

   After channel closure, the on-chain settlement will reflect the final balances. You can verify on the CKB explorer that:
    - nodeA's address received CKB reflecting its remaining channel balance
    - nodeB's address received CKB reflecting its accumulated payments



---

## Testnet: UDT (RUSD) Payment

Mainnet public nodes do not hold any USDI yet, so UDT channels cannot be created at this time. This section only covers the testnet.

On testnet, node2 exposes a public RPC endpoint (including `new_invoice`), so for simplicity this demo uses the path nodeA → node1 → node2. Only nodeA is needed. Set the node2 RPC URL before running the commands below (check [dashboard](https://dashboard.fiber.channel/nodes) for the current endpoint):

```bash
NODE2_RPC="<node2_rpc_endpoint>"
```


### Establishing a UDT Channel: nodeA ⟺ node1


1. Connect nodeA to node1

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 1,
       "jsonrpc": "2.0",
       "method": "connect_peer",
       "params": [
           {
               "pubkey": "02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71"
           }
       ]
   }'
   ```



2. Open a UDT channel with 20 RUSD: nodeA (20 RUSD) ⟺ node1 (0 RUSD)

   _Node1 has `auto_accept_amount` for RUSD set to 20 RUSD, so please provide 20 RUSD or more as the funding_amount._

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 2,
       "jsonrpc": "2.0",
       "method": "open_channel",
       "params": [
           {
               "pubkey": "02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71",
               "funding_amount": "0x77359400",
               "public": true,
               "funding_udt_type_script": {
                   "code_hash": "0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a",
                   "hash_type": "type",
                   "args": "0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"
               }
           }
       ]
   }'
   ```



3. Query channels and wait for `ChannelReady`

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 3,
       "jsonrpc": "2.0",
       "method": "list_channels",
       "params": [
           {
               "pubkey": "02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71"
           }
       ]
   }'
   ```

   Example response:

   ```json
   {"jsonrpc":"2.0","id":3,"result":{"channels":[{"channel_id":"0x19380f65a48f88bf69dd07336f231655a183e01bba304485d0fed6a428f329d3","is_public":true,"is_acceptor":false,"is_one_way":false,"channel_outpoint":"0x34efee5b56b48b314d3d75870db4921f34b226d2b224e02bb9d89d3ee77d791c00000000","pubkey":"02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71","funding_udt_type_script":{"code_hash":"0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a","hash_type":"type","args":"0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"},"state":{"state_name":"ChannelReady"},"local_balance":"0x77359400","offered_tlc_balance":"0x0","remote_balance":"0x0","received_tlc_balance":"0x0","pending_tlcs":[],"latest_commitment_transaction_hash":"0x9c254a1a8f4be0be28256072568c3490cc1dedb0e2dd1f63ff05b1e5ddf7a10f","created_at":"0x19d72cb19f4","enabled":true,"tlc_expiry_delta":"0xdbba00","tlc_fee_proportional_millionths":"0x3e8","shutdown_transaction_hash":null,"failure_detail":null}]}}
   ```

   Filter the response for entries where `funding_udt_type_script` is not null. You should see `local_balance` of `0x77359400` (20 RUSD) and `remote_balance` of `0x0` (0 RUSD). Take note of these initial balances for comparison after payments.



### Payment: nodeA → node1 → node2


1. Generate an invoice on node2

   Set the amount to 0x5f5e100 (100,000,000), which is equivalent to 1 RUSD.

   Here, a unique payment_preimage is still required. You can generate one using: `payment_preimage="0x$(openssl rand -hex 32)"`

   ```bash
   curl -s --location "$NODE2_RPC" --header 'Content-Type: application/json' --data '{
       "id": 1,
       "jsonrpc": "2.0",
       "method": "new_invoice",
       "params": [
           {
               "amount": "0x5f5e100",
               "currency": "Fibt",
               "description": "test invoice generated by node2",
               "expiry": "0xe10",
               "final_cltv": "0x28",
               "payment_preimage": "'$payment_preimage'",
               "hash_algorithm": "sha256",
               "udt_type_script": {
                   "code_hash": "0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a",
                   "hash_type": "type",
                   "args": "0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"
               }
           }
       ]
   }'
   ```

   Record the `invoice_address` from the response.



2. Send payment from nodeA

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 2,
       "jsonrpc": "2.0",
       "method": "send_payment",
       "params": [
           {
               "invoice": "<invoice_address from step 1>"
           }
       ]
   }'
   ```



3. Repeat Steps 1 and 2 two more times

   Perform two additional `new_invoice` (on node2) and `send_payment` (on nodeA) requests, keeping the amount set to 0x5f5e100.



4. Query channel balances after payments

   nodeA ⟺ node1

   Balances changed from `{"local_balance":"0x77359400","remote_balance":"0x0"}` to `{"local_balance":"0x654f5d20","remote_balance":"0x11e636e0"}`



This means the channel balances have changed as follows:

- Before payments

  nodeA (2000000000) ⟺ node1 (0)

- After payments

  nodeA (1699700000) ⟺ node1 (300300000)

Funds changes:

​	nodeA: 1699700000 - 2000000000 = -300300000

​	node1 earned fee: 300300000 - 300000000 = 300000

​	node2 received: 300000000

**Conclusion: Three UDT payments of 100,000,000 (1 RUSD) each from nodeA → node1 → node2 were successfully completed. The intermediate node (node1) earned a total fee of 300,000 (0.003 RUSD).**



5. Close the UDT channel

   Replace `<channel_id>` with the `channel_id` from the `list_channels` response.

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 5,
       "jsonrpc": "2.0",
       "method": "shutdown_channel",
       "params": [
           {
               "channel_id": "<channel_id>",
               "close_script": {
                   "code_hash": "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
                   "hash_type": "type",
                   "args": "<lock_arg_a>"
               },
               "fee_rate": "0x3FC"
           }
       ]
   }'
   ```

   You can verify on the CKB explorer that nodeA's address received a settlement transaction reflecting its remaining RUSD balance.
