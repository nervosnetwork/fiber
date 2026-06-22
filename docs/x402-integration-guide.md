# x402 Integration Guide

This document describes how an x402-compatible resource server can use Fiber's current facilitator MVP.

## What Fiber Provides

Fiber currently provides a minimal x402 facilitator surface with:

- `GET /supported`
- `POST /verify`
- `POST /settle`

The supported payment model is invoice-based exact payment. The client pays a normal Fiber invoice first, then proves payment by sending the invoice string together with the payment preimage.

## Current Payment Model

This is not a delegated settlement model where the facilitator sends the payment for the client.

Instead:

1. The merchant creates a standard Fiber invoice.
2. The client pays it using Fiber.
3. The client submits an x402 proof payload containing the invoice and payment preimage.
4. The facilitator verifies that proof against merchant-side invoice state.
5. `/settle` returns a deterministic receipt-like response once the invoice is already paid.

## Discover Capabilities

Call `GET /supported` to learn the supported `(scheme, network)` pairs.

Current MVP expectations:

- `x402Version = 2`
- `scheme = "exact"`
- `network` is one of `fiber:mainnet`, `fiber:testnet`, or `fiber:dev`
- `signers["fiber:*"]` contains the merchant public key hex

## Payment Requirements

For the current Fiber facilitator, `payTo` should be the merchant public key hex advertised by `/supported`.

The current MVP expects:

- `scheme = "exact"`
- `network = fiber:<chain>`
- `asset = "ckb"`
- `amount = <invoice amount as decimal string>`
- `payTo = <merchant pubkey hex>`

## Payment Payload Proof

The current proof payload shape is:

```json
{
  "invoice": "<fiber invoice string>",
  "paymentPreimage": "<32-byte hex preimage>"
}
```

`payment_preimage` is also accepted for compatibility with snake_case producers.

## Verify Request Example

```json
{
  "x402Version": 2,
  "paymentPayload": {
    "x402Version": 2,
    "accepted": {
      "scheme": "exact",
      "network": "fiber:testnet",
      "asset": "ckb",
      "amount": "1000",
      "payTo": "<merchant-pubkey-hex>",
      "maxTimeoutSeconds": 60,
      "extra": {}
    },
    "payload": {
      "invoice": "<fiber invoice string>",
      "paymentPreimage": "<preimage hex>"
    }
  },
  "paymentRequirements": {
    "scheme": "exact",
    "network": "fiber:testnet",
    "asset": "ckb",
    "amount": "1000",
    "payTo": "<merchant-pubkey-hex>",
    "maxTimeoutSeconds": 60,
    "extra": {}
  }
}
```

## Verification Semantics

The facilitator verifies:

- x402 version is 2
- scheme is `exact`
- network matches the local Fiber chain
- invoice parses successfully
- invoice payee matches the configured merchant public key
- `payTo` matches the same merchant public key
- amount matches the invoice amount
- asset matches `ckb`
- `sha256-or-configured-hash(preimage) == invoice.payment_hash`
- the invoice exists in the merchant store
- the merchant store marks it as `Paid`

The current MVP returns `200 OK` for verification responses, including invalid ones.

## Settlement Semantics

`POST /settle` does not submit another payment. It is a receipt step for already-paid invoices.

If the proof is valid and the invoice is already paid, the facilitator returns:

- `success = true`
- `transaction = "fiber-receipt:<payment-hash-hex>"`
- `network = fiber:<chain>`
- `amount = <invoice amount>`

If the proof is invalid, settlement returns `success = false` with `errorReason` and `errorMessage`.

## Current MVP Constraints

- no x402 client SDK package yet
- no TypeScript `@x402/fiber` helper yet
- no multi-asset pricing expression
- no extensions support
- no delegated authorization flow
- no hold-invoice workflow
- no richer payer identity reporting

## Recommended Integration Strategy

Use this facilitator when:

- your merchant service already creates Fiber invoices
- your client can pay those invoices directly
- your resource server wants a simple x402 verification/receipt layer

Keep the integration narrow for now: exact payments, one asset, one merchant signer, and already-paid invoice proofs.
