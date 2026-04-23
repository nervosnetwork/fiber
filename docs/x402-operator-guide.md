# x402 Operator Guide

This document explains how to enable the current x402 facilitator MVP on a Fiber node, what HTTP endpoints it exposes, and what behavior operators should expect.

## Overview

Fiber now exposes a minimal x402-compatible facilitator on the existing HTTP RPC listener when the `x402` RPC module is enabled. This MVP supports invoice-based exact payments only.

The current facilitator exposes three HTTP endpoints on the same address as RPC:

- `GET /supported`
- `POST /verify`
- `POST /settle`

These routes are handled alongside the JSON-RPC server. They do not require a separate listener or service configuration.

## Enable x402

Enable x402 by adding `x402` to `rpc.enabled_modules`.

Example config:

```yaml
rpc:
  listening_addr: "127.0.0.1:8227"
  enabled_modules:
    - cch
    - channel
    - graph
    - payment
    - info
    - invoice
    - peer
    - x402
```

Equivalent environment variable form:

```bash
export RPC_ENABLED_MODULES="cch,channel,graph,payment,info,invoice,peer,x402"
```

## Listener Behavior

The x402 facilitator runs on the same HTTP server as JSON-RPC. If your RPC listener is `127.0.0.1:8227`, then:

- `http://127.0.0.1:8227/supported`
- `http://127.0.0.1:8227/verify`
- `http://127.0.0.1:8227/settle`

are served by the facilitator.

JSON-RPC remains available on the same listener.

If Biscuit auth is configured for RPC, keep in mind that x402 currently shares the same listener. The current implementation is intended for private or controlled deployments while the x402 surface is still MVP-level.

## Supported Network Strings

The facilitator advertises one x402 network string based on the local Fiber chain:

- `mainnet` -> `fiber:mainnet`
- `testnet` -> `fiber:testnet`
- any other chain -> `fiber:dev`

The current implementation supports only:

- `scheme = "exact"`
- `asset = "ckb"`

## Supported Flow

The current facilitator does not create or relay payments on behalf of clients.

Instead, the supported flow is:

1. The merchant creates a normal Fiber invoice.
2. The client pays that invoice over Fiber.
3. The client submits `invoice + paymentPreimage` to the facilitator.
4. The facilitator verifies the proof against the merchant's local invoice store.
5. The facilitator returns a receipt-like settlement response.

This means the merchant node must already know the invoice and must already observe it as paid before `/settle` succeeds.

## /supported

`GET /supported` returns the currently advertised x402 capabilities.

The current MVP returns one `kinds` entry for the local network and one `signers` entry keyed by `fiber:*` with the merchant public key hex.

Example response shape:

```json
{
  "kinds": [
    {
      "x402Version": 2,
      "scheme": "exact",
      "network": "fiber:testnet"
    }
  ],
  "extensions": [],
  "signers": {
    "fiber:*": ["<merchant-pubkey-hex>"]
  }
}
```

## /verify

`POST /verify` accepts an x402 V2 verification request.

For the current MVP, the facilitator expects the proof payload to contain:

- `invoice`: the full Fiber invoice string
- `paymentPreimage` or `payment_preimage`: the payment preimage hex string

Verification succeeds only if all of the following are true:

- `x402Version == 2`
- `scheme == "exact"`
- `network` matches the local chain mapping
- `asset == "ckb"`
- `amount` matches the invoice amount
- `payTo` matches the configured merchant public key hex
- the invoice payee also matches that merchant
- the preimage hashes to the invoice payment hash
- the invoice exists in the merchant store
- the merchant store marks the invoice as `Paid`

If verification fails, the facilitator returns `200 OK` with `isValid: false` and an `invalidReason`.

## /settle

`POST /settle` re-runs the same exact verification logic.

For the MVP, settlement does not trigger a second network-side payment. If the invoice is already paid and the proof is valid, the facilitator returns a deterministic receipt-like response immediately.

Example success shape:

```json
{
  "success": true,
  "transaction": "fiber-receipt:<payment-hash-hex>",
  "network": "fiber:testnet",
  "amount": "1000"
}
```

If validation fails, the facilitator returns `success: false` with `errorReason` and `errorMessage`.

## Operational Limits

Current MVP limitations:

- no dedicated x402 listener; it shares the RPC HTTP listener
- no TypeScript `@x402/fiber` package yet
- no delegated payment authorization flow
- no hold-invoice flow
- no multi-asset x402 expression
- no x402 extension support
- no payer identity extraction in responses
- no settlement replay protection beyond the existing invoice-paid check

## Recommended Deployment Scope

Use the current x402 MVP for:

- local testing
- private testnet deployments
- controlled integrations where the merchant operates both the Fiber node and the resource server relationship

Treat public exposure as experimental until the x402 surface gains more explicit configuration, auth policy, and documentation coverage.
