# Fiber Liquidity Review Fixes Design

## Goal

Make Loop In and UDT Loop Out use one consistent asset and preimage-hash contract across invoice validation, Fiber payment construction, and the liquidity-lock transaction.

## Decisions

Loop In imports accept only invoices whose effective hash algorithm is `HashAlgorithm::CkbHash`. The lock script always validates the CKB hash of the preimage, so accepting SHA-256 invoices would create a payment that settles while its lock cannot be claimed with the same preimage.

UDT Loop Out propagates the quote's UDT type script through `LoopOutPaymentRequest` into `SendPaymentCommand`. CKB Loop Out continues to pass no UDT script. The existing invoice, amount, route, and on-chain UDT validation remain authoritative; this change only removes the lost asset-type information at the payment boundary.

## Data Flow

`validate_loop_in_invoice` validates amount, UDT type script, and effective hash algorithm. A valid quote carries its asset unchanged. When the client pays a Loop Out quote, the payment request carries `quote.asset.udt_type_script`, and the network command receives the same script. Recovery and retry paths must construct the request from the persisted quote rather than inventing a new asset value.

## Error Handling

Non-CKB-hash Loop In invoices return `LiquidityLoopOutError::PaymentFailed`. Missing or malformed UDT metadata remains rejected by existing quote and chain validation. A UDT payment must never silently degrade to a CKB payment.

## Testing

Add quote tests for accepting CKB-hash invoices and rejecting SHA-256 invoices. Add payment adapter tests that inspect the generated `SendPaymentCommand` and verify UDT scripts are propagated for UDT quotes and omitted for CKB quotes. Run targeted tests, formatting, clippy, migration/RPC checks, and Cargo Shear.
