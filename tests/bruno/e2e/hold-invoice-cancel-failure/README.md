# Hold Invoice Cancel Failure

This workflow covers delayed final-hop failure decoding:

1. Node1 opens a direct channel to Node2.
2. Node2 creates a hold invoice with `payment_hash` and no `payment_preimage`.
3. Node1 pays the invoice.
4. Node2 waits until the invoice is `Received`, then cancels it.
5. Node1 checks the failed payment error.

Node1 must report the decoded final-hop error (`InvoiceCancelled` or `HoldTlcTimeout`) instead of `InvalidOnionError`.
