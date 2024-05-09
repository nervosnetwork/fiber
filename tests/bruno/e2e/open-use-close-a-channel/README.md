# E2E test for a happy path ckb pcn channel open and closure

Below is a complete happy path process to open, transact over and close a payment channel.
We intend to create a [bruno](https://www.usebruno.com/) collection to execute all these operations.
The whole process is something like:

1. Connect to peer from NODE3 to NODE1
2. Send a test message from NODE3 to NODE1 (not necessary for pcn per se)
3. Send `open_channel` from NODE3 to NODE1 (thus NODE3 is the opener and NODE1 the acceptor)
4. NODE1 automatically accepts channel and replies `accept_channel` message
5. NODE1 sends a `tx_update` to NODE3 add input1 with 1000 ckb to inputs, and outputs 500 to the funding transaction.
6. NODE3 adds input2 with 2000 ckb to tx1 and sends a `tx_update` to NODE1, and fund the channel with 1000 ckb (so now we have 1500 ckb in the funding output).
7. NODE1 remove input1 from tx2 and add input3 to tx2 with the balance of funding transaction output unchanged.
8. NODE3 sends a `tx_complete` to NODE1 to express his intention of complete the funding process
9. NODE1 sends a `tx_complete` to NODE3 to express his intention of complete the funding process
10. Both NODE1 and NODE3 send a `commitment_signed` to sign a spending transaction of the yet to exist funding transaction
11. The node with less funds in the funding transaction sends a `tx_signatures` to its counterparty
12. The counterparty replies a `tx_signatures`
13. The opener NODE3 sends a `channel_ready` to indicate the channel is ready for him
14. The acceptor waits a few blocks after the funding transaction is broadcasted and replies a `channel_ready`
15. NODE3 sends an `add_tlc` to pay NODE1 10 (payment1).
16. NODE1 sends an `add_tlc` to pay NODE3 to pay 20 (payment2), and then NODE3 sends 30 to NODE1 (payment3).
17. NODE1 sends an `add_tlc` to pay NODE3 to pay 20 (payment4), and then NODE3 sends 30 to NODE1 (payment5).
18. NODE1 sends NODE3 a `remove_tlc` to complete a payment3.
19. NODE3 sends NODE1 a `remove_tlc` to fail payment2. By this time, NODE1 has 1100 ckb ready to use, and NODE3 has 430 ckb ready to use.
20. NODE3 sends a `commitment_signed` to commit the inflight transactions
21. NODE1 acknowledges the commitment transaction by sending a `revoke_and_ack`
22. NODE1 initiates a `shutdown` to close the channel
23. NODE3 replies a `shutdown` to acknowledge the closure of this channel
24. There are still payment1, payment4 and payment5 waiting for resolution. We remove them payment1 and payment5 by failing them from NODE1, and complete payment4 from NODE3. NODE1 has 1010 ckb ready to use, and NODE3 has 490 ckb ready to use.
25. NODE3 and NODE1 automatically send a `closing_signed` to their counterparty on all tlc finished. 

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
