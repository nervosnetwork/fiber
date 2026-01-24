# Trampoline Routing Specification (Summary)

## Who this is for
This is a short, tester‑oriented overview of trampoline routing in Fiber. It focuses on behavior and inputs, not implementation details.

## What it does (high‑level)
- The sender only needs to reach the first trampoline node.
- The rest of the route is encoded inside an inner trampoline onion and forwarded hop‑by‑hop by trampoline nodes.
- The final recipient only sees the final payload and settles the payment.

## How to use it (RPC)
- Use `trampoline_hops` in SendPayment RPC to provide explicit trampoline nodes.
- Each hop includes `pubkey` and optional hints `fee_rate`, `tlc_expiry_delta` (currently not used in route building).
- `max_fee_amount` is required when `trampoline_hops` is provided.

RPC definition:
- [crates/fiber-lib/src/rpc/payment.rs](../../crates/fiber-lib/src/rpc/payment.rs#L143)

## Fee behavior (summary)
- A recommended min/max fee is estimated using the default fee rate as a guardrail.
- The sender attempts to deliver `final_amount + max_fee_amount` to the first trampoline.
- Any fee spent on the outer route reduces what the first trampoline receives.
- The remaining fee budget is split evenly across trampoline hops and passed as `build_max_fee_amount`.

Implementation reference:
- [crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1313)

## Expiry behavior (summary)
- Each remaining trampoline hop contributes a default expiry delta.
- Total delta must not exceed `tlc_expiry_limit`.

Implementation reference:
- [crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1468)

## Forwarding behavior (summary)
- Trampoline node peels the inner onion, validates amounts, and builds a new payment toward the next hop.
- The hop’s `build_max_fee_amount` bounds the next outer route’s fee.

Implementation reference:
- [crates/fiber-lib/src/fiber/network.rs](../../crates/fiber-lib/src/fiber/network.rs#L2432)

## Key payloads (links)
- Trampoline onion packet: [crates/fiber-lib/src/fiber/types.rs](../../crates/fiber-lib/src/fiber/types.rs#L3833)
- Trampoline hop payloads: [crates/fiber-lib/src/fiber/types.rs](../../crates/fiber-lib/src/fiber/types.rs#L3778)

## Tests to validate
- Graph‑level construction tests: [crates/fiber-lib/src/fiber/tests/graph.rs](../../crates/fiber-lib/src/fiber/tests/graph.rs)
- End‑to‑end trampoline tests: [crates/fiber-lib/src/fiber/tests/trampoline.rs](../../crates/fiber-lib/src/fiber/tests/trampoline.rs)

## Known constraints
- Max trampoline hops: `MAX_TRAMPOLINE_HOPS_LIMIT` (5)
- Trampoline packet size: `TRAMPOLINE_PACKET_DATA_LEN`
- `max_fee_amount` must be provided when `trampoline_hops` is set

Constants reference:
- [crates/fiber-lib/src/fiber/payment.rs](../../crates/fiber-lib/src/fiber/payment.rs#L38)# Trampoline Routing Specification (Summary)

## Who this is for
This is a short, tester‑oriented overview of trampoline routing in Fiber. It focuses on behavior and inputs, not implementation details.

## What it does (high‑level)
- The sender only needs to reach the first trampoline node.
- The rest of the route is encoded inside an inner trampoline onion and forwarded hop‑by‑hop by trampoline nodes.
- The final recipient only sees the final payload and settles the payment.

## How to use it (RPC)
- Use `trampoline_hops` in SendPayment RPC to provide explicit trampoline nodes.
- Each hop includes `pubkey` and optional hints `fee_rate`, `tlc_expiry_delta` (currently not used in route building).
- `max_fee_amount` is required when `trampoline_hops` is provided.

RPC definition:
- [crates/fiber-lib/src/rpc/payment.rs](../../crates/fiber-lib/src/rpc/payment.rs#L143)

## Fee behavior (summary)
- A recommended min/max fee is estimated using the default fee rate as a guardrail.
- The sender attempts to deliver `final_amount + max_fee_amount` to the first trampoline.
- Any fee spent on the outer route reduces what the first trampoline receives.
- The remaining fee budget is split evenly across trampoline hops and passed as `build_max_fee_amount`.

Implementation reference:
- [crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1313)

## Expiry behavior (summary)
- Each remaining trampoline hop contributes a default expiry delta.
- Total delta must not exceed `tlc_expiry_limit`.

Implementation reference:
- [crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1468)

## Forwarding behavior (summary)
- Trampoline node peels the inner onion, validates amounts, and builds a new payment toward the next hop.
- The hop’s `build_max_fee_amount` bounds the next outer route’s fee.

Implementation reference:
- [crates/fiber-lib/src/fiber/network.rs](../../crates/fiber-lib/src/fiber/network.rs#L2432)

## Key payloads (links)
- Trampoline onion packet: [crates/fiber-lib/src/fiber/types.rs](../../crates/fiber-lib/src/fiber/types.rs#L3833)
- Trampoline hop payloads: [crates/fiber-lib/src/fiber/types.rs](../../crates/fiber-lib/src/fiber/types.rs#L3778)

## Tests to validate
- Graph‑level construction tests: [crates/fiber-lib/src/fiber/tests/graph.rs](../../crates/fiber-lib/src/fiber/tests/graph.rs)
- End‑to‑end trampoline tests: [crates/fiber-lib/src/fiber/tests/trampoline.rs](../../crates/fiber-lib/src/fiber/tests/trampoline.rs)

## Known constraints
- Max trampoline hops: `MAX_TRAMPOLINE_HOPS_LIMIT` (5)
- Trampoline packet size: `TRAMPOLINE_PACKET_DATA_LEN`
- `max_fee_amount` must be provided when `trampoline_hops` is set

Constants reference:
- [crates/fiber-lib/src/fiber/payment.rs](../../crates/fiber-lib/src/fiber/payment.rs#L38)# Trampoline Routing Specification

## Overview

Trampoline routing is a payment routing mechanism that allows nodes with limited routing information to make payments by delegating pathfinding to intermediate "trampoline" nodes. This document describes the implementation of trampoline routing in Fiber Network, inspired by the [Lightning Network BOLT proposal](https://github.com/lightning/bolts/blob/trampoline-routing/proposals/trampoline.md).

## Motivation

Traditional onion routing requires the sender to:
1. Maintain a complete network graph
2. Compute the entire payment path from source to destination
3. Have sufficient computational resources for pathfinding

Trampoline routing addresses these limitations by:
- Allowing light clients to make payments without full network graph
- Delegating complex pathfinding to well-connected trampoline nodes
- Reducing computational burden on the sender
- Enabling privacy-preserving routing through nested onion packets

## Architecture

### Two-Layer Onion Structure

Trampoline routing uses a nested onion packet structure:

1. **Outer Payment Onion**: Standard payment onion that routes to the first trampoline node
2. **Inner Trampoline Onion**: Embedded within the outer onion, contains the trampoline route chain

```
Sender → [Outer Onion] → First Trampoline → [Inner Onion] → Next Trampoline → ... → Final Recipient
```

### Key Components

#### 1. Trampoline Onion Packet

Defined in [`types.rs`](../../crates/fiber-lib/src/fiber/types.rs#L3833):

```rust
- All nodes must support trampoline routing feature

#### Step 2: Validate Fee Budget

Trampoline routing currently requires `max_fee_amount` to be explicitly set. A minimal
and maximal recommended fee budget is estimated using the default fee rate (not per-hop
`fee_rate`) to guard against obviously insufficient budgets.

```rust
let default_min_fee = estimate_trampoline_fee(final_amount, 1, hops)?;
if max_fee_amount < default_min_fee {
    let default_max_fee = estimate_trampoline_fee(final_amount, 10, hops)?;
    return Err("max_fee_amount is too low for trampoline routing: recommend_minimal_fee=...".into());
}
```

#### Step 3: Build Outer Route to First Trampoline

```rust
let route_to_trampoline = self.find_path(
    source,
    first_trampoline,
    Some(final_amount + max_fee_amount),
    None,
    // ... other parameters
)?;
```

The sender tries to deliver `final_amount + max_fee_amount` to the first trampoline.
Any routing fee consumed on this outer route reduces the amount received by the first trampoline.

#### Step 4: Allocate Routing Fee Budget Among Trampoline Hops

```rust
let first_hop_fee = final_amount
    .saturating_sub(route_to_trampoline.last().map_or(0, |h| h.amount_received));

if first_hop_fee > max_fee_amount {
    return Err("max_fee_amount is too low for trampoline routing: first_hop_fee=...".into());
}

let remaining_fee = max_fee_amount.saturating_sub(first_hop_fee);
let slots = hops.len() as u128;
let base = remaining_fee / slots;
let remainder = (remaining_fee % slots) as usize;
fees.fill(base);
for fee in fees.iter_mut().take(remainder) {
    *fee = fee.saturating_add(1);
}

if fees.iter().any(|&f| f == 0) {
    return Err("max_fee_amount is too low for trampoline routing: fee for a trampoline hop is zero".into());
}
```

The routing budget is evenly split across the trampoline hops (no extra segment for the sender).
Each hop gets `fees[idx]` as its `build_max_fee_amount`.

#### Step 5: Construct Inner Trampoline Onion

```rust
let trampoline_onion = TrampolineOnionPacket::create(
    session_key,
    trampoline_path,  // [t1, t2, ..., final]
    payloads,         // Forward/Final payloads
    Some(payment_hash),
    &secp_ctx,
)?;
```

The outer route embeds the trampoline onion in its final hop:
```rust
PaymentHopData {
    // ... standard fields ...
    trampoline_onion: Some(trampoline_onion_bytes),
}
```

### 3. Trampoline Forwarding
```

Formula:

$$
fee = round\\_up\\left(\\frac{amount\\_to\\_forward \\times fee\\_rate}{1{,}000{,}000}\\right)
$$

#### Step 3: Allocate Fee Budget

Total fee budget is split between:
- Trampoline service fees (computed above)
- Routing fees (for outer payment onion construction)

$$
remaining\\_budget = max\\_fee\\_amount - trampoline\\_service\\_fee\\_total
$$

The remaining budget is divided equally among routing segments:
- Segment 0: Sender → First Trampoline
- Segment i+1: Trampoline[i] → Next Hop

$$
base = \\left\\lfloor \\frac{remaining\\_budget}{segments} \\right\\rfloor
$$

#### Step 4: Build Outer Route to First Trampoline

```rust
let route_to_trampoline = self.find_path(
    source,
    first_trampoline,
    Some(amount_to_trampoline),
    payer_routing_budget,
    // ... other parameters
)?;
```

#### Step 5: Construct Inner Trampoline Onion

```rust
let trampoline_onion = TrampolineOnionPacket::create(
    session_key,
    trampoline_path,  // [t1, t2, ..., final]
    payloads,         // Forward/Final payloads
    Some(payment_hash),
    &secp_ctx,
)?;
```

The outer route embeds the trampoline onion in its final hop:
```rust
PaymentHopData {
    // ... standard fields ...
    trampoline_onion: Some(trampoline_onion_bytes),
}
```

### 3. Trampoline Forwarding

When a trampoline node receives a payment with embedded trampoline onion ([`network.rs`](../../crates/fiber-lib/src/fiber/network.rs#L2432)):

```rust
async fn forward_trampoline_packet(
    &self,
    state: &mut NetworkActorState<S, C>,
    trampoline_bytes: &[u8],
    previous_tlc: Option<PrevTlcInfo>,
    payment_hash: Hash256,
    trampoline_outer_shared_secret: Option<[u8; 32]>,
    incoming_amount: u128,
) -> Result<(), TlcErr>
```

#### Step 1: Peel Trampoline Onion

```rust
let trampoline_packet = TrampolineOnionPacket::new(trampoline_bytes.to_vec());
let peeled_trampoline = trampoline_packet.peel(
    &state.private_key,
    Some(payment_hash.as_ref()),
    &Secp256k1::new(),
)?;
```

#### Step 2: Validate Forward Payload

```rust
match peeled_trampoline.current {
    TrampolineHopPayload::Forward {
        next_node_id,
        amount_to_forward,
        build_max_fee_amount,
        tlc_expiry_delta,
        tlc_expiry_limit,
        max_parts,
    } => {
        // Verify incoming amount covers forwarding amount
        if incoming_amount < amount_to_forward {
            return Err(TlcErr::new_node_fail(
                TlcErrorCode::FeeInsufficient,
                state.get_public_key(),
            ));
        }
        // ... continue forwarding
    }
}
```

#### Step 3: Build Payment to Next Hop

The trampoline node constructs a new payment session:

```rust
let payment_data = SendPaymentDataBuilder::new(
    next_node_id,
    amount_to_forward,
    payment_hash
)
.final_tlc_expiry_delta(tlc_expiry_delta)
.tlc_expiry_limit(tlc_expiry_limit)
.max_fee_amount(build_max_fee_amount)  // Use allocated routing budget
.max_parts(max_parts)
.udt_type_script(udt_type_script)
.trampoline_context(Some(TrampolineContext {
    remaining_trampoline_onion,        // Remaining layers
    previous_tlcs: vec![prev_tlc],     // For settlement
}))
.allow_mpp(max_parts.is_some_and(|v| v > 1))
.build()?;
```

#### Step 4: Launch Payment Actor

```rust
self.start_payment_actor(
    state.network.clone(),
    state,
    payment_hash,
    PaymentActorMessage::SendPayment(payment_data, rpc_reply),
).await
```

This actor will:
1. Find a path to `next_node_id` using the allocated `build_max_fee_amount`
2. Construct a new outer payment onion (with remaining trampoline onion embedded)
3. Forward the payment via channels

### 4. Final Recipient Processing

When the final recipient receives the payment ([`channel.rs`](../../crates/fiber-lib/src/fiber/channel.rs#L1092)):

```rust
if let Some(TrampolineHopPayload::Final {
    final_amount,
    final_tlc_expiry_delta,
    payment_preimage,
    custom_records,
    ..
}) = last_hop_inner_onion {
    // Validate amount and expiry
    // Settle TLC with preimage
}
```
