# Force close with pending TLCs and one stopped watchtower

Node1 and Node2 open a channel and each adds a TLC whose preimage is unknown.
Before Node1 force-closes, the workflow stops monitoring on Node2's side.
Node1's watchtower must recover Node1's balance, apart from transaction fees;
Node2's channel funding must remain unclaimed.

For this case, `tests/nodes/start.sh` disables Node2's built-in watchtower and
uses Node3's watchtower RPC instead. Step 11 checks that Node2's watch exists on
Node3, removes it, and checks that it is gone. Node3 has no local channel actor
for this channel, so removal does not bypass the production guard that protects
live channels on their own nodes. Removing the watch directly on Node2 would
leave that protection running, even though the RPC returns success.

The final checks share a 120-second deadline and mine more blocks while waiting
for settlement. Spending the original commitment output alone does not mean
that all subsequent settlement transactions have confirmed. Balance assertions
retain the 5,000-shannon fee allowance and also ensure that Node2 did not reclaim
its 350-CKB contribution through an accidentally active watchtower.
