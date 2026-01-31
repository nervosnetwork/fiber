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