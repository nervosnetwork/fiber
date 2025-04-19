# E2E test for a happy path fiber network channel open and closure

Below is a complete happy path process to open, transact over and close a fiber channel.
We intend to create a [bruno](https://www.usebruno.com/) collection to execute all these operations.
The whole process is something like:

1. Connect to peer from NODE3 to NODE1
2. Send `open_channel` from NODE3 to NODE1 (thus NODE3 is the opener and NODE1 the acceptor)
3. NODE1 automatically accepts channel and replies `accept_channel` message
4. NODE1 sends a `tx_update` to NODE3 add input1 with 100000000000 ckb to inputs, and outputs 50000000000 to the funding transaction.
5. NODE3 adds input2 with 200000000000 ckb to tx1 and sends a `tx_update` to NODE1, and fund the channel with 100000000000 ckb (so now we have 150000000000 ckb in the funding output).
6. NODE1 remove input1 from tx2 and add input3 to tx2 with the balance of funding transaction output unchanged.
7. NODE3 sends a `tx_complete` to NODE1 to express his intention of complete the funding process
8. NODE1 sends a `tx_complete` to NODE3 to express his intention of complete the funding process
9. Both NODE1 and NODE3 send a `commitment_signed` to sign a spending transaction of the yet to exist funding transaction
10. The node with less funds in the funding transaction sends a `tx_signatures` to its counterparty
11. The counterparty replies a `tx_signatures`
12. The opener NODE3 sends a `channel_ready` to indicate the channel is ready for him
13. The acceptor waits a few blocks after the funding transaction is broadcasted and replies a `channel_ready`
14. NODE3 sends an `add_tlc` to pay NODE1 1000000000 (payment1).
15. NODE1 sends an `add_tlc` to pay NODE3 to pay 2000000000 (payment2), and then NODE3 sends 3000000000 to NODE1 (payment3).
16. NODE1 sends an `add_tlc` to pay NODE3 to pay 2000000000 (payment4), and then NODE3 sends 3000000000 to NODE1 (payment5).
17. NODE1 sends NODE3 a `remove_tlc` to complete a payment3.
18. NODE3 sends NODE1 a `remove_tlc` to fail payment2. By this time, NODE1 has 110000000000 ckb ready to use, and NODE3 has 43000000000 ckb ready to use.
19. NODE1 initiates a `shutdown` to close the channel
20. NODE3 replies a `shutdown` to acknowledge the closure of this channel
21. There are still payment1, payment4 and payment5 waiting for resolution. We remove them payment1 and payment5 by failing them from NODE1, and complete payment4 from NODE3. NODE1 has 101000000000 ckb ready to use, and NODE3 has 49000000000 ckb ready to use.
22. NODE3 and NODE1 automatically send a `closing_signed` to their counterparty on all tlc finished.

Note that the channel is opened with a delay of 3.5 epochs (14 hours), 3.5 epochs is [presented](https://github.com/nervosnetwork/rfcs/blob/master/rfcs/0017-tx-valid-since/e-i-l-encoding.png) as 3(E) and 1(I)/2(L): (L << 40) | (I << 24) | E = 0x20001000003,

## Starting nodes

```
./tests/nodes/start.sh
```

## Running tests

Currently only a few tests are implemented in [the current directory](./) as not all these functionalities are implemented.

```
cd tests/bruno
npm exec -- @usebruno/cli run e2e/open-use-close-a-channel -r --env test
```
