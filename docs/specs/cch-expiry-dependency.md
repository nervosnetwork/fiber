# CCH Expiry Dependency

For CCH to complete a swap successfully, the incoming payment must leave sufficient expiry time for the outgoing payment to settle.

## Configuration Options

CCH config provides the following options:

- **`btc_final_tlc_expiry_delta_blocks`**: The minimum final CLTV expiry used when creating the incoming invoice in LND.
- **`ckb_final_tlc_expiry_delta_seconds`**: The minimum final TLC expiry used when creating the incoming invoice in Fiber.
- **`min_outgoing_invoice_expiry_delta_seconds`**: The minimum remaining time before the outgoing invoice expires. CCH will reject swaps if the outgoing invoice expires sooner than this threshold.

## Default Behavior

The default values for the first two options ensure a 30-hour window to settle the outgoing payment. This relies on LND and Fiber verifying that the final TLC expiry time meets the configured threshold specified in the invoice.

## Outgoing Invoice Final Expiry Validation

CCH validates that the outgoing invoice's final CLTV/TLC expiry is less than half of the configured expiry for the incoming invoice. This ensures the CCH operator has sufficient time to settle the incoming side before the outgoing side expires.

- **SendBTC** (Fiber → Lightning): The BTC invoice's `min_final_cltv_expiry_delta` (converted to seconds assuming 10 min/block) must be less than `ckb_final_tlc_expiry_delta_seconds / 2`.
- **ReceiveBTC** (Lightning → Fiber): The Fiber invoice's `final_tlc_minimum_expiry_delta` must be less than `btc_final_tlc_expiry_delta_blocks / 2` (converted to milliseconds).

If this validation fails, the swap is rejected with an error indicating the final expiry delta exceeds the safe limit for cross-chain swaps.

## Outgoing Payment Route Expiry Check

The static validation above only compares the **final** expiry deltas, which represent the minimum expiry at the last hop. However, when the outgoing payment is routed through multiple intermediate nodes, each hop adds its own expiry delta. The total route expiry at the first hop can be significantly larger than the final expiry delta alone.

To prevent fund loss from this scenario, CCH performs a dynamic expiry check when dispatching the outgoing payment (after the incoming payment is accepted):

1. **Compute remaining incoming time**: `incoming_final_expiry_delta - (now - order.created_at)`. This is a conservative lower bound on the incoming TLC/HTLC's remaining time, since the TLC was accepted at or after order creation.

2. **Allocate half for outgoing**: The maximum allowed outgoing route expiry is `remaining / 2`. The other half is reserved for CCH to settle the incoming payment after receiving the preimage.

3. **Validate sufficiency**: If the allocated time is less than the outgoing invoice's minimum final expiry delta, the order is failed immediately — routing is impossible without exceeding the safe limit.

4. **Cap the outgoing route**: The computed limit is enforced on the outgoing payment:
   - **BTC (LND)**: Set `cltv_limit` on `SendPaymentRequest` to `max_outgoing_seconds / 600` blocks.
   - **CKB (Fiber)**: Set `tlc_expiry_limit` on `SendPaymentCommand` to `max_outgoing_seconds * 1000` milliseconds.

This ensures that even in the worst case — where the outgoing payment settles at the very last moment before its route expiry — CCH still has enough time to settle the incoming payment.