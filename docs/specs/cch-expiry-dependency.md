# CCH Expiry Dependency

For CCH to complete a swap successfully, the incoming payment must leave sufficient expiry time for the outgoing payment to settle.

## Configuration Options

CCH config provides the following options:

- **`btc_final_tlc_expiry_delta_blocks`**: The minimum final CLTV expiry used when creating the incoming invoice in LND.
- **`ckb_final_tlc_expiry_delta_seconds`**: The minimum final TLC expiry used when creating the incoming invoice in Fiber.
- **`min_outgoing_invoice_expiry_delta_seconds`**: The minimum remaining time before the outgoing invoice expires. CCH will reject swaps if the outgoing invoice expires sooner than this threshold.

## Default Behavior

The default values for the first two options ensure a 20-hour window to settle the outgoing payment. This relies on LND and Fiber verifying that the final TLC expiry time meets the configured threshold specified in the invoice.
