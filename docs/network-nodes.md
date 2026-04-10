# Fiber Network Nodes

> **Important:** Bootnodes are used for **peer discovery only** and cannot be used to create payment channels. To open/create a payment channel, you must connect to a **public node**.

**Why this document only lists pubkeys, not IP addresses:** Since `v0.8.0`, Fiber RPCs identify peers by **pubkey** rather than `peer_id`. In practice, once your node has learned a peer's address from gossip (or from saved peer state), `connect_peer` can resolve the address from the pubkey automatically. Since node IPs may change over time while the pubkey stays stable, only the stable pubkey is listed here.

**More nodes:** [https://dashboard.fiber.channel/nodes](https://dashboard.fiber.channel/nodes)

## Mainnet

### Public Nodes (can create channels)

The public nodes' CKB addresses do not hold any USDI yet, so UDT channels cannot be created at this time.

| Name | Pubkey | CKB Address | open_channel_auto_accept_min_ckb_funding_amount | auto_accept_channel_ckb_funding_amount | UDT: USDI script | UDT: USDI auto_accept_amount |
|------|--------|-------------|--------------------------------------------------|----------------------------------------|-------------------|-------------------------------|
| fiber-mainnet-public-ca | `03a8d7da8d0934363dbc17f52c872e8d833016415266eabb3527439c5dd17adc6b` | `ckb1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqdjsydf39sklhfvtfnk57vvd7d2vn4kxwqpr3prn` | `49900000000` (≥ 499 CKB auto-accepted; 99 CKB collateral) | `25000000000` (250 CKB inbound liquidity) | code_hash: `0xbfa35a9c38a676682b65ade8f02be164d48632281477e36f8dc2f41f79e56bfc`, hash_type: `type`, args: `0xd591ebdc69626647e056e13345fd830c8b876bb06aa07ba610479eb77153ea9f` | `10000000` (≥ 0.1 USDI auto-accepted) |
| fiber-mainnet-public-tokyo | `033a69e5be369dab43aefa96fa729d83c571ccb066f312136c6ab2d354fcc028f9` | `ckb1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqv47n6dc9ay34npwktvlpp5huzjvd07t4qhspqyz` | `49900000000` (≥ 499 CKB auto-accepted; 99 CKB collateral) | `25000000000` (250 CKB inbound liquidity) | code_hash: `0xbfa35a9c38a676682b65ade8f02be164d48632281477e36f8dc2f41f79e56bfc`, hash_type: `type`, args: `0xd591ebdc69626647e056e13345fd830c8b876bb06aa07ba610479eb77153ea9f` | `10000000` (≥ 0.1 USDI auto-accepted) |

## Testnet

### Public Nodes (can create channels)

| Name | Pubkey | CKB Address | open_channel_auto_accept_min_ckb_funding_amount | auto_accept_channel_ckb_funding_amount | UDT: RUSD script | UDT: RUSD auto_accept_amount |
|------|--------|-------------|--------------------------------------------------|----------------------------------------|-------------------|-------------------------------|
| fiber-testnet-public-bottle | `02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71` | `ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqfy4w0gqjsm0ulnq0l4ft6hu6spztrj72sjtcnx4` | `49900000000` (≥ 499 CKB auto-accepted; 99 CKB collateral) | `25000000000` (250 CKB inbound liquidity) | code_hash: `0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a`, hash_type: `type`, args: `0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b` | `2000000000` (≥ 20 RUSD auto-accepted) |
| fiber-testnet-public-bracer | `0291a6576bd5a94bd74b27080a48340875338fff9f6d6361fe6b8db8d0d1912fcc` | `ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqwgkulmcyxtv2vgcmgatupg2r02k8n4mjcmm9f5m` | `49900000000` (≥ 499 CKB auto-accepted; 99 CKB collateral) | `25000000000` (250 CKB inbound liquidity) | code_hash: `0x1142755a044bf2ee358cba9f2da187ce928c91cd4dc8692ded0337efa677d21a`, hash_type: `type`, args: `0x878fcc6f1f08d48e87bb1c3b3d5083f23f8a39c5d5c764f253b55b998526439b` | `2000000000` (≥ 20 RUSD auto-accepted) |
