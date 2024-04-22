# E2E test for a happy path ckb pcn channel open and closure

Below is a complete happy path process to open, transact over and close a payment channel.
We intend to create a [bruno](https://www.usebruno.com/) collection to execute all these operations.
The whole process is something like:

1. Connect to peer from NODE3 to NODE1
2. Send a test message from NODE3 to NODE1 (not necessary for pcn per se)
3. Send `open_channel` from NODE3 to NODE1 (thus NODE3 is the opener and NODE1 the acceptor)
4. NODE1 automatically accepts channel and replies `accept_channel` message
5. NODE1 sends a `tx_add` to NODE3 to fund part of the funding transaction
6. NODE3 sends a `tx_add` to NODE1 to fund part of the funding transaction
7. NODE1 sends a `tx_remove` to NODE3 to remove part of the funding from the funding transaction
8. NODE3 sends a `tx_complete` to NODE1 to express his intention of complete the funding process
9. NODE1 sends a `tx_complete` to NODE3 to express his intention of complete the funding process
10. Both NODE1 and NODE1 send a `commitment_signed` to sign a spending transaction of the yet to exist funding transaction
11. The node with less funds in the funding transaction sends a `tx_signatures` to its counterparty
12. The counterparty replies a `tx_signatures`
13. The opener NODE3 sends a `channel_ready` to indicate the channel is ready for him
14. The acceptor waits a few blocks after the funding transaction is broadcasted and replies a `channel_ready`
15. NODE3 sends an `add_tlc` to pay NODE1, calling this payment payment1
16. NODE3 sends an `add_tlc` to pay NODE1 again, calling this payment payment2
17. NODE1 sends an `add_tlc` to pay NODE3, calling this payment payment3
18. NODE1 sends NODE3 a `remove_tlc` to complete a payment2
19. NODE3 sends NODE1 to fail payment3
20. NODE3 sends a `commitment_signed` to commit the inflight transactions
21. NODE1 acknowledges the commitment transaction by sending a `revoke_and_ack`
22. NODE1 initiates a `shutdown` to close the channel
23. NODE3 replies a `shutdown` to acknowledge the closure of this channel
24. NODE3 sends a `closing_signed` to make a NODE1 immediately broadcastable closure transaction 
24. NODE1 sends a `closing_signed` to make a NODE3 immediately broadcastable closure transaction 

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