# Note [x402 facilitator MVP]

This note records the current x402 facilitator architecture in Fiber and the main tradeoffs behind the MVP.

## Overview

Fiber's x402 support is currently implemented as a thin HTTP layer on top of the existing RPC server and invoice store.

The implementation adds three non-JSON-RPC HTTP routes to the RPC listener:

- `GET /supported`
- `POST /verify`
- `POST /settle`

These are intercepted before requests fall through to `jsonrpsee` JSON-RPC handling.

## Why the MVP uses invoice plus preimage

The current design does not ask the facilitator to originate a payment. Instead, it accepts an already-paid Fiber invoice plus the payment preimage as proof.

This was chosen because Fiber already has the right primitives for this flow:

- signed invoices with stable payment hashes
- merchant-side invoice state in the store
- paid-invoice status tracking
- payment preimage availability after successful payment

That means the facilitator can stay small and stateless relative to chain execution. It only needs to parse the invoice, verify the preimage binding, and confirm that the merchant has already observed the invoice as paid.

## Route Placement

The current x402 routes are implemented in `crates/fiber-lib/src/rpc/mod.rs` rather than a separate listener.

Benefits:

- minimal new service lifecycle wiring
- reuses existing config and listener management
- reuses the existing store and `FiberConfig` already available to RPC

Tradeoff:

- x402 currently shares the RPC listener, which is operationally convenient but not yet ideal for long-term separation of concerns

## Verification Semantics

Pure verification lives in `crates/fiber-lib/src/x402/facilitator.rs`.

The current verifier checks:

- x402 V2 only
- `exact` scheme only
- local CAIP-like network mapping only
- invoice parse succeeds
- invoice payee matches the configured merchant key
- `payTo` matches the same merchant key
- asset and amount match the invoice
- preimage hashes to the invoice payment hash
- invoice exists in the merchant store
- invoice status is `Paid`

This keeps the logic small and explicit instead of introducing a larger scheme registry or generalized settlement engine before the integration contract is proven.

## Settlement Semantics

The current `/settle` behavior is deliberately receipt-like.

If verification passes and the invoice is already paid, settlement returns a deterministic string:

`fiber-receipt:<payment-hash-hex>`

This avoids the incorrect model of trying to "re-settle" a standard invoice after the payment already happened on the network.

## Current Limits

Important deferred items:

- no `@x402/fiber` package yet
- no hold-invoice or escrow-style flow
- no delegated authorization flow
- no extension support
- no multi-asset expression
- no dedicated x402 service config or separate listener
- no richer settlement replay handling beyond invoice-paid checks

## Likely Follow-up Work

Natural next steps are:

1. add explicit docs and examples for merchant-side integration
2. decide whether x402 should remain on the RPC listener or move to its own service config
3. add invalid `/settle` integration tests and additional pure verifier cases
4. design a client helper package for exact Fiber proofs
5. evaluate hold-invoice or delegated settlement flows for stronger purchase semantics
