

```markdown
node1 ---> node2 ---> node3

seq node  rpc
1   node1 connect_peer(node2)
2   node2 connect_peer(node3)
3   node1 open_channel(node2, 200 CKB) auto accept? channel1
4   node2 open_channel(node3, 100 CKB) auto accept? channel2
5   node3 生成 0.1 CKB invoice, 包含 payment_hash (rpc name??)
6   node1 add_tlc(channel1, tlc_id, 0.1 CKB payment_hash, expiry)
7   node2 add_tlc(channel2, tlc_id, 0.1 CKB payment_hash, expiry)
8   node3 remove_tlc(channel2, RemoveTlcFulfill(payment_preimage))
9   node2 remove_tlc(channel1, RemoveTlcFulfill(payment_preimage))
repeat 5 ~ 9 with different amount
10  node3 close_channel(channel2)
11  node2 close_channel(channel1)
```

verify node1, node2, node3 balance by checking on-chain final close tx
