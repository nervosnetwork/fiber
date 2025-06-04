# Testnet Public Nodes User Manual

## Testnet Public Nodes’ Addresses
#### node1

```bash
"/ip4/18.162.235.225/tcp/8119/p2p/QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo"
```

#### node2

```bash
"/ip4/18.163.221.211/tcp/8119/p2p/QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89"
```



## Local Node Deployment

1. Download fnn

   Please visit the [releases page](https://github.com/nervosnetwork/fiber/releases) to download and use the latest version of fnn.

   ```bash
   mkdir tmp && cd tmp
   tar xzvf fnn-latest.tar.gz
   ```



2. Export the account private key to the fiber node’s ckb directory

   Here, the ckb-cli is used to create an account, which will later be used to pay for opening  channels between the local node and the public testnet node. If ckb-cli is not installed, please download it from the [releases](https://github.com/nervosnetwork/ckb-cli/releases).

   ```bash
   # Create a local node directory named nodeA
   mkdir -p testnet-fnn/nodeA/ckb
   ./ckb-cli account new
   ./ckb-cli account export --lock-arg 0xcc015401df73a3287d8b2b19f0cc23572ac8b14d --extended-privkey-path exported-key
   head -n 1 ./exported-key > testnet-fnn/nodeA/ckb/key
   chmod 600 testnet-fnn/nodeA/ckb/key
   # check nodeA key
   ./ckb-cli util key-info --privkey-path testnet-fnn/nodeA/ckb/key
   ```




3. Copy config.yml

   ```bash
   cp config/testnet/config.yml testnet-fnn/nodeA
   ```



4. Fund nodeA’s address with 10000ckb and 20RUSD via faucet

   The RUSD faucet cannot directly fill an address, so you can first claim 20RUSD through a wallet like joyid, then transfer it to nodeA’s address from [the joyid wallet page](https://testnet.joyid.dev/).

   - ckb: https://faucet.nervos.org

   - RUSD: https://testnet0815.stablepp.xyz/faucet



5. Start the node A

   You need to set a `FIBER_SECRET_KEY_PASSWORD` environment variable in the startup command to encrypt your wallet private key file. I used `123` here for demo purposes, but I recommend using a strong password.
   
   ```bash
   FIBER_SECRET_KEY_PASSWORD='123' RUST_LOG=info ./fnn -c testnet-fnn/nodeA/config.yml -d testnet-fnn/nodeA > testnet-fnn/nodeA/a.log 2>&1 &
   ```




## Establishing a CKB Channel with Public Node 1


1. Establish a network connection between nodeA and node1

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 1,
       "jsonrpc": "2.0",
       "method": "connect_peer",
       "params": [
           {
               "address": "/ip4/18.162.235.225/tcp/8119/p2p/QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":null,"id":1}
   ```



2. Establish a channel with 500ckb: nodeA (500ckb) ⟺ node1 (250ckb)

   _Node1 has open_channel_auto_accept_min_ckb_funding_amount set at 438ckb, so please input 500ckb or more._

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 2,
       "jsonrpc": "2.0",
       "method": "open_channel",
       "params": [
           {
               "peer_id": "QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo",
               "funding_amount": "0xba43b7400",
               "public": true
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":{"temporary_channel_id":"0x30089ec4c8ce1e1d4930220c2bff856eec7ab44550e15b76d62489fd42eaafe8"},"id":2}
   ```




3. Query the channels between nodeA and node1

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 3,
       "jsonrpc": "2.0",
       "method": "list_channels",
       "params": [
           {
               "peer_id": "QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo"
           }
       ]
   }'
   ```

   Wait until the state_name changes to `CHANNEL_READY`.

   **Note: When the channel has just changed to the CHANNEL_READY state and you attempt to use send_payment, you may still encounter an error: `Failed to build route`. It is advisable to wait for some time before trying again.**

   ```json
   {"jsonrpc":"2.0","result":{"channels":[{"channel_id":"0x26ce85d57fb4a1a826cbf4862358862317a83b775090625550d8be12c6ce9569","is_public":true,"channel_outpoint":"0x9bb2a8a4bebaf793a235ba2ec87051ae0018b58736b6741df74009ca8101cb8d00000000","peer_id":"QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo","funding_udt_type_script":null,"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0xa32aef600","offered_tlc_balance":"0x0","remote_balance":"0x460913c00","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x18ef541a5a195c0ea4715a7783964b3c4be8fba6bd25542e626f91ef1673e3e4","created_at":"0x195892d237f","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"}]},"id":3
   ```

   **Why is the local_balance 0xa32aef600 (43,800,000,000) and the remote_balance 0x460913c00 (18,800,000,000)?**

	This channel was established with nodeA contributing 500 ckb and node1 contributing 250 ckb.

	Since each cell requires a minimum of 62 ckb, this amount is reserved to ensure that there are sufficient funds to cover cell occupancy costs during on-chain settlement (when the channel closes). These 62 ckb will be returned to their respective nodes at the time of on-chain settlement.

	Actual available funds in the channel:

	nodeA: 500 ckb - 62 ckb = 438 ckb (local_balance is 0xa32aef600)

	node1: 250 ckb - 62 ckb = 188 ckb (remote_balance is 0x460913c00)



4. Call the `new_invoice` API on node2 to generate an invoice

   Set the amount to 0x5f5e100 (100,000,000 shannon), which is equivalent to 1 CKB. The payment_preimage should be a unique 32-byte hexadecimal number.

   ```bash
   # Generate a 32-byte random number and represent it in hexadecimal
   payment_preimage="0x$(openssl rand -hex 32)"
   echo $payment_preimage
   ```

   ```bash
   0xbc03e507befb33cfd5953a2e7046428e69cb8f0ade65c05d3661128aa4b4fff9
   ```

   ```bash
   curl -s --location 'http://18.163.221.211:8227' --header 'Content-Type: application/json' --data '{
       "id": 4,
       "jsonrpc": "2.0",
       "method": "new_invoice",
       "params": [
           {
               "amount": "0x5f5e100",
               "currency": "Fibt",
               "description": "test invoice generated by node2",
               "expiry": "0xe10",
               "final_cltv": "0x28",
               "payment_preimage": "0xbc03e507befb33cfd5953a2e7046428e69cb8f0ade65c05d3661128aa4b4fff9",
               "hash_algorithm": "sha256"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":{"invoice_address":"fibt1000000001peseucdphcxgfw0pnm6vk3uftyc36dakyjchs0p0unk9gaug0h36uhafww9pvy38gcesad084rx48xgx9xts49yp9fn87yfchld3l3qu5n0pfzvvy8c9g7dksrcxyrtk3hymspezmvtx4vg5v6uvt6tyxmq5uhrfejpk0j6wue9ef2pa8mzmrgqaz3wucutujtjcmq2x8f36faxuctg62ny73mhaj7rpwqe0ns0wp5wr4tku7qcl9r4a3swluvd2jqqwmsl7wsz4cwvhhe7p8tr7hz5qkqwr3r38hukckqzjtmntd8zrz0ywux4u8df005hl76thzsp9hz7dyefzk4mqhx4x9el98zjzmhcveqpfeur79","invoice":{"currency":"Fibt","amount":"0x5f5e100","signature":"0e1b101f1e0e100215180e0c1717191e01070b031e1702140016000e0311031107171c1618160002120b1b130b0d070203020f040e1c06151c070d090f0f14171f1e1a0b170210010517021e0d0419090216151b001706150605191f05070212021b17180c190001","data":{"timestamp":"0x1958944fa64","payment_hash":"0xafb604f74c28009732ed4c82983cf1efaddf62ee36442f360fb4a8c79b845432","attrs":[{"Description":"test invoice generated by node2"},{"ExpiryTime":{"secs":3600,"nanos":0}},{"HashAlgorithm":"sha256"},{"PayeePublicKey":"0291a6576bd5a94bd74b27080a48340875338fff9f6d6361fe6b8db8d0d1912fcc"}]}}},"id":4}
   ```

   Record the `invoice_address` from the response.



5. Before nodeA sends the payment, first query the local_balance and remote_balance of each channel

   nodeA ⟺ node1

   As shown in Step 3, the response included: `{"local_balance":"0xa32aef600","remote_balance":"0x460913c00"}`

   node1 ⟺ node2

   ```bash
   curl -s --location 'http://18.162.235.225:8227' --header 'Content-Type: application/json' --data '{
       "id": 5,
       "jsonrpc": "2.0",
       "method": "list_channels",
       "params": [
           {
               "peer_id": "QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89"
           }
       ]
   }'
   ```

    ```json
    {"jsonrpc":"2.0","result":{"channels":[{"channel_id":"0x29a2e93e70fcfcd8b64fd74646b3893247f2a73a9dd8706298b5defa17bfee0a","is_public":true,"channel_outpoint":"0xa065311059be4d2194d9d6dbc428fe794ed3c6d91e08fe1d960d1574c19f88d400000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":{"code_hash":"0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a","hash_type":"type","args":"0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"},"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x173c0e06bb","offered_tlc_balance":"0x1f5","remote_balance":"0xc68e145","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x195e1cbd1dd062752e776a44dd9c12f3b83a69dfdd1e22edff19025572bcbd25","created_at":"0x1944491c154","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0x4cd5bdcac419b203fd5752c4daa00a6f24305123d65f7a7fa6b455df82e97eee","is_public":true,"channel_outpoint":"0xe7d8464be26933021810f31252a98e9b2b1ff00f70173fafb134861ce21bccbb00000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":{"code_hash":"0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a","hash_type":"type","args":"0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"},"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x1748630df7","offered_tlc_balance":"0x0","remote_balance":"0x13da09","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x6bd9890fd65297359079d87a995d794becc90bafd5eca9676ccbfd96abcb3ffd","created_at":"0x194448fc295","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0x9e72e8dbf7409a5aaf456dbe25f61247450f72079249ee508bb23cb14d0408b1","is_public":true,"channel_outpoint":"0x49f5f1cf664d48df66943989ef87d1316f1dffe5aec96db9ee8f1b6879ccac1b00000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":{"code_hash":"0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a","hash_type":"type","args":"0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"},"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0xa38b9d","offered_tlc_balance":"0x0","remote_balance":"0x1747d35c63","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x6ad24a7dda73ed2ff896401ba4487c207362510670295b60288040df4b78884d","created_at":"0x194448ea599","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0x632548057f0f13752e6d55ea666a93aaae450f2cf6e31093142c940071648f88","is_public":true,"channel_outpoint":"0xb0fcf51f0587c3c623377d054874dbb6ff1e8a26950834ace30dc88003af05f900000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":{"code_hash":"0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a","hash_type":"type","args":"0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"},"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0xc505f","offered_tlc_balance":"0x0","remote_balance":"0x17486a97a1","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x62892d0751149af74d6baa7d7e09215427858d8dd047ae62b9179c8779d236e5","created_at":"0x194448dbf4b","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0x0d54942293e7bb2704749e85741fd65e9a3d2f4eb380eb33b0aa0d38f891638f","is_public":true,"channel_outpoint":"0xf846f128450f3319352e8b48a38060feaed09d834f8d1c4d98477069f64ef78100000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":null,"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x45a9b5cf3","offered_tlc_balance":"0x0","remote_balance":"0x916e2dc010d","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x68002aad33179b3d7bd21234ecf7a296f71833ecd4a69632e294c583e73181ff","created_at":"0x1944489267e","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0x4c84c39f5166eb15631fca02dbc1910fa0139ad0ec6732ab2cc51c275d8fc11b","is_public":true,"channel_outpoint":"0x5c871464dc91eaf6fb262157329dc90d00b96cafaf272bd322184cab5d2601fa00000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":null,"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x504f21d045c","offered_tlc_balance":"0x0","remote_balance":"0x4164b5a59a4","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x117b18ac0789b44e3a08504d503a9cbf29acd2a4050069a640f67d7ec8209a00","created_at":"0x1944487f433","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0x728fce53aaedf010b8f7c09497f3ab8527382ada0b6691cc61c04badf4837296","is_public":true,"channel_outpoint":"0x713364717227e24ebcf1b1ddd469f3f278e8b4069ebd23631d2aca12fffa2e1c00000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":null,"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x916d6f044e9","offered_tlc_balance":"0x0","remote_balance":"0x466871917","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x83a0d88fd312bb14f3cf953888257581d859230d1230b21b487a5cda48be7c8d","created_at":"0x1944485b77f","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0xfb27dc9ebc391440afe5e25cd4dc25e302b6fbb089397eda07decdb04db14b9e","is_public":true,"channel_outpoint":"0x56b3edb1dd683f9149286069881395bb878c69fbff638b7dc6d1bca1c83acd6400000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":null,"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x48ab5ace976","offered_tlc_balance":"0x0","remote_balance":"0x49087ca748a","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0xe6a25f3420db9f0a436df0214fa5622a39a8975549f861f941d33cf8fba19e2e","created_at":"0x1944483b877","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"},{"channel_id":"0x92b04366f93500efef5f28ba79704fa3a0e3771148899aa8836f0e5d2fbc38d5","is_public":true,"channel_outpoint":"0x61368447e60f0aa15ef61e13539d19d2f32fbacb20728db4248d8c82fa56079a00000000","peer_id":"QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89","funding_udt_type_script":null,"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x48ab5acd200","offered_tlc_balance":"0x0","remote_balance":"0x49087ca8c00","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x6048cd50eb71aa8fabda1a5d567ceb5fb84181efbb36d839245e253e382bbfaf","created_at":"0x194447dd85a","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"}]},"id":5}
    ```
   Find all entries in the response where `funding_udt_type_script` is null.
   ```json
   {"local_balance":"0x45a9b5cf3","remote_balance":"0x916e2dc010d"}
   {"local_balance":"0x504f21d045c","remote_balance":"0x4164b5a59a4"}
   {"local_balance":"0x916d6f044e9","remote_balance":"0x466871917"}
   {"local_balance":"0x48ab5ace976","remote_balance":"0x49087ca748a"}
   {"local_balance":"0x48ab5acd200","remote_balance":"0x49087ca8c00"}
   ```



6. Send a send_payment request to nodeA to pay node2

   Pass in the previously recorded invoice_address to the `send_payment` request

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 6,
       "jsonrpc": "2.0",
       "method": "send_payment",
       "params": [
           {
               "invoice": "fibt1000000001peseucdphcxgfw0pnm6vk3uftyc36dakyjchs0p0unk9gaug0h36uhafww9pvy38gcesad084rx48xgx9xts49yp9fn87yfchld3l3qu5n0pfzvvy8c9g7dksrcxyrtk3hymspezmvtx4vg5v6uvt6tyxmq5uhrfejpk0j6wue9ef2pa8mzmrgqaz3wucutujtjcmq2x8f36faxuctg62ny73mhaj7rpwqe0ns0wp5wr4tku7qcl9r4a3swluvd2jqqwmsl7wsz4cwvhhe7p8tr7hz5qkqwr3r38hukckqzjtmntd8zrz0ywux4u8df005hl76thzsp9hz7dyefzk4mqhx4x9el98zjzmhcveqpfeur79"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":{"payment_hash":"0xafb604f74c28009732ed4c82983cf1efaddf62ee36442f360fb4a8c79b845432","status":"Created","created_at":"0x1958957cc7d","last_updated_at":"0x1958957cc7d","failed_error":null,"fee":"0x186a0"},"id":6}
   ```



7. Repeat Steps 4 and 6 two more times

   Performing two additional `new_invoice` and `send_payment` requests, keeping the amount set to 0x5f5e100.



8. Query the local_balance and remote_balance of each channel again

   nodeA ⟺ node1

   Balances changed from`{"local_balance":"0xa32aef600","remote_balance":"0x460913c00"}`to`{"local_balance":"0xa2cb78e60","remote_balance":"0x46688a3a0"}`.

   node1 ⟺ node2

   Balances changed from `{"local_balance":"0x48ab5acd200","remote_balance":"0x49087ca8c00"}`to`{"local_balance":"0x48aa3cb2f00","remote_balance":"0x49099ac2f00"}`.

   All other entries remain unchanged.



   This means the channel balances have changed as follows before and after the payments:

   - Before payments

     nodeA (43800000000) ⟺ node1 (18800000000)

     node1 (4993800000000) ⟺ node2 (5018800000000)

   - After payments

     nodeA (43499700000) ⟺ node1 (19100300000)

     node1 (4993500000000) ⟺ node2 (5019100000000)

   Funds changes:

   ​	nodeA: 43499700000 - 43800000000 = -300300000

   ​	node1: 4993500000000 + 19100300000 - 4993800000000 - 18800000000 = 300000

   ​	node2: 5019100000000 - 5018800000000 = 300000000

   **Conclusion: Three CKB payments of 100,000,000 shannon each from nodeA → node1 → node2 were successfully completed. The intermediate node (node1) earned a total fee of 300,000 shannon.**



9. Close the channel between nodeA and node1

   Pass in the channel_id and the receiving address as parameters.

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 9,
       "jsonrpc": "2.0",
       "method": "shutdown_channel",
       "params": [
           {
               "channel_id": "0x26ce85d57fb4a1a826cbf4862358862317a83b775090625550d8be12c6ce9569",
               "close_script": {
                   "code_hash": "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
                   "hash_type": "type",
                   "args": "0xcc015401df73a3287d8b2b19f0cc23572ac8b14d"
               },
               "fee_rate": "0x3FC"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":null,"id":9}
   ```

   You can see on the CKB explorer that nodeA’s address received a new transaction of +496.99699462 CKB.
   This indicates that multiple off-chain CKB transfers through Fiber nodes are eventually settled on-chain upon channel closure via the shutdown_channel request.



## Establishing a UDT Channel with Public Node 1

1. Establish a network connection between nodeA and node1



2. Establish a channel with 20 RUSD: nodeA (20 RUSD) ⟺ node1 (0)

   _Node1 has auto_accept_amount for RUSD set to 20 RUSD, so please input 20 RUSD or more as the funding_amount._

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 2,
       "jsonrpc": "2.0",
       "method": "open_channel",
       "params": [
           {
               "peer_id": "QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo",
               "funding_amount": "0x2540be400",
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

   ```json
   {"jsonrpc":"2.0","result":{"temporary_channel_id":"0xa3137338377b67ea90c2f2c15b7d60ad27b3e891095f4b093772d7db3aa79344"},"id":2}
   ```



3. Query the channels between nodeA and node1

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 3,
       "jsonrpc": "2.0",
       "method": "list_channels",
       "params": [
           {
               "peer_id": "QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":{"channels":[{"channel_id":"0x75dce35923a79086afd0f81b0134ac87619756b6c04a15669ce232aa7db142d8","is_public":true,"channel_outpoint":"0x8e133056792766e1fd34e870fb33990b58c4ebb9615526b38dacdf3686cf6d3f00000000","peer_id":"QmXen3eUHhywmutEzydCsW4hXBoeVmdET2FJvMX69XJ1Eo","funding_udt_type_script":{"code_hash":"0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a","hash_type":"type","args":"0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"},"state":{"state_name":"CHANNEL_READY","state_flags":[]},"local_balance":"0x2540be400","offered_tlc_balance":"0x0","remote_balance":"0x0","received_tlc_balance":"0x0","latest_commitment_transaction_hash":"0x2b0b36c5db14778484358a4641bfe00a4f351660c280255ef8e8538898e399d0","created_at":"0x1958977b7be","enabled":true,"tlc_expiry_delta":"0x5265c00","tlc_fee_proportional_millionths":"0x3e8"}]},"id":3}
   ```




4. Call the `new_invoice` API on node2 to generate an invoice

   Set the amount to 0x5f5e100 (100,000,000), which is equivalent to 1 RUSD.

   Here, a unique payment_preimage is still required. You can generate one using: `echo "0x$(openssl rand -hex 32)"`

   ```bash
   curl -s --location 'http://18.163.221.211:8227' --header 'Content-Type: application/json' --data '{
       "id": 4,
       "jsonrpc": "2.0",
       "method": "new_invoice",
       "params": [
           {
               "amount": "0x5f5e100",
               "currency": "Fibt",
               "description": "test invoice generated by node2",
               "expiry": "0xe10",
               "final_cltv": "0x28",
               "payment_preimage": "0xf7d121b132b4f53bb8301591028b34fccc065f92161bb6e7d41cf6d32ad32a22",
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

   ```json
   {"jsonrpc":"2.0","result":{"invoice_address":"fibt1000000001px88ja42xcmczxzat8lhtuq9f29ga8x244qk737nl4r7lq8aw7y7puhjn6jp50xsd2c6ndfxkmn5wnl4z8clk7fej9trwx0gjlmtvnj2wqwlvcu0eekzqvtehlc42t8lpstmgc7ntskh5ef36f8hgvck8c9pescktlx05fpuaceews94kvyrvgf87gvd9wnmh86puzyz2vp6h6jppt8lsq5u8tc87y6szha9587f90dmlmwt5mtetxz9ekukxu6x7s2fyuuy2re0etzzksqnt8rtr5925qypz2224j5xf56nlscnmtvcvywdxg40hsy5w5xt40d5cdest3kvhqswfftfc3qqs7plhlk7m5n9hyzqws9qlxw2huurg7l6c4q9evyg7fljcl3cqh3h3ecpg3fue3cq4slpxapvc2uye6jl77sfcflc8jf8fvr4qwly9wxuyehqf573hu454qy92wqke0hdgrvm7y83sgspn4a29h69s7ucp4cedle","invoice":{"currency":"Fibt","amount":"0x5f5e100","signature":"161b1405170f18161b1e1c090c0a0a010a190409061c1b1a1107130413040f1a0204080504121212150f1b031b171a040417030102191d010a1317090f1f1c120e1909121b0d041b17101f071e1b020d170a151a15121d0d081d12151816171c0d000215190a1801","data":{"timestamp":"0x1958e785913","payment_hash":"0x6a356ad088b704a9c53728029bd968e894daf5adab1da838bf06f6755239b005","attrs":[{"Description":"test invoice generated by node2"},{"ExpiryTime":{"secs":3600,"nanos":0}},{"UdtScript":"0x550000001000000030000000310000001142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a0120000000878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b"},{"HashAlgorithm":"sha256"},{"PayeePublicKey":"0291a6576bd5a94bd74b27080a48340875338fff9f6d6361fe6b8db8d0d1912fcc"}]}}},"id":4}
   ```



5. Before nodeA sends the payment, first query the local_balance and remote_balance of each channel

   nodeA ⟺ node1

   As shown in Step 3, the response included: `{"local_balance":"0x77359400","remote_balance":"0x0"}`

   node1 ⟺ node2

   ```bash
   curl -s --location 'http://18.162.235.225:8227' --header 'Content-Type: application/json' --data '{
       "id": 5,
       "jsonrpc": "2.0",
       "method": "list_channels",
       "params": [
           {
               "peer_id": "QmbKyzq9qUmymW2Gi8Zq7kKVpPiNA1XUJ6uMvsUC4F3p89"
           }
       ]
   }'
   ```
   Find all entries in the response where `funding_udt_type_script` is not null.
   ```json
   {"local_balance":"0x172a2c63bb","remote_balance":"0x1e4a8445"}
   {"local_balance":"0x1748630df7","remote_balance":"0x13da09"}
   {"local_balance":"0xa38b9d","remote_balance":"0x1747d35c63"}
   {"local_balance":"0xc505f","remote_balance":"0x17486a97a1"}
   ```



6. Send a send_payment request to nodeA to pay node2

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 6,
       "jsonrpc": "2.0",
       "method": "send_payment",
       "params": [
           {
               "invoice": "fibt1000000001px88ja42xcmczxzat8lhtuq9f29ga8x244qk737nl4r7lq8aw7y7puhjn6jp50xsd2c6ndfxkmn5wnl4z8clk7fej9trwx0gjlmtvnj2wqwlvcu0eekzqvtehlc42t8lpstmgc7ntskh5ef36f8hgvck8c9pescktlx05fpuaceews94kvyrvgf87gvd9wnmh86puzyz2vp6h6jppt8lsq5u8tc87y6szha9587f90dmlmwt5mtetxz9ekukxu6x7s2fyuuy2re0etzzksqnt8rtr5925qypz2224j5xf56nlscnmtvcvywdxg40hsy5w5xt40d5cdest3kvhqswfftfc3qqs7plhlk7m5n9hyzqws9qlxw2huurg7l6c4q9evyg7fljcl3cqh3h3ecpg3fue3cq4slpxapvc2uye6jl77sfcflc8jf8fvr4qwly9wxuyehqf573hu454qy92wqke0hdgrvm7y83sgspn4a29h69s7ucp4cedle"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":{"payment_hash":"0x6a356ad088b704a9c53728029bd968e894daf5adab1da838bf06f6755239b005","status":"Created","created_at":"0x1958e7b66b5","last_updated_at":"0x1958e7b66b5","failed_error":null,"fee":"0x186a0"},"id":6}
   ```



7. Repeat Steps 4 and 6 two more times

   Performing two additional `new_invoice` and `send_payment` requests, keeping the amount set to 0x5f5e100.



8. Query the local_balance and remote_balance of each channel again

   nodeA ⟺ node1

   Balances changed from`{"local_balance":"0x77359400","remote_balance":"0x0"}`to`{"local_balance":"0x654f5d20","remote_balance":"0x11e636e0"}`

   node1 ⟺ node2

   Balances changed from`{"local_balance":"0x172a2c63bb","remote_balance":"0x1e4a8445"}`to`{"local_balance":"0x17184ac0bb","remote_balance":"0x302c2745"}`

   All other entries remain unchanged.



   This means the channel balances have changed as follows before and after the payments:

   - Before payments

     nodeA (2000000000) ⟺ node1 (0)

     node1 (99491799995) ⟺ node2 (508200005)

   - After payments

     nodeA (1699700000) ⟺ node1 (300300000)

     node1 (99191799995) ⟺ node2 (808200005)

   Funds changes:

   ​	nodeA: 1699700000 - 2000000000 = -300300000

   ​	node1: 99191799995 + 300300000 - 99491799995 = 300000

   ​	node2: 808200005 - 508200005 = 300000000

   **Conclusion: Three UDT payments of 100,000,000 each from nodeA → node1 → node2 were successfully completed. The intermediate node (node1) earned a total fee of 300,000.**



9. Close the channel between nodeA and node1

   ```bash
   curl -s --location 'http://127.0.0.1:8227' --header 'Content-Type: application/json' --data '{
       "id": 9,
       "jsonrpc": "2.0",
       "method": "shutdown_channel",
       "params": [
           {
               "channel_id": "0x75dce35923a79086afd0f81b0134ac87619756b6c04a15669ce232aa7db142d8",
               "close_script": {
                   "code_hash": "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
                   "hash_type": "type",
                   "args": "0xcc015401df73a3287d8b2b19f0cc23572ac8b14d"
               },
               "fee_rate": "0x3FC"
           }
       ]
   }'
   ```

   ```json
   {"jsonrpc":"2.0","result":null,"id":9}
   ```

   You can see on the CKB explorer that nodeA’s address received a new transaction of +16.997 RUSD.
   This indicates that multiple off-chain UDT transfers through Fiber nodes are eventually settled on-chain upon channel closure via the shutdown_channel request.
