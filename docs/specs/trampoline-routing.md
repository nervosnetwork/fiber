# Trampoline Routing Specification (Summary)

## What it does (high‑level)
- The sender only needs to reach the first trampoline node.
- The rest of the route is encoded inside an inner trampoline onion and forwarded hop‑by‑hop by trampoline nodes.
- The final recipient only sees the final payload and settles the payment.

## How to use it (RPC)
- Use `trampoline_hops` in SendPayment RPC to provide explicit trampoline nodes.

## Fee behavior (summary)
- A recommended min/max fee is estimated using the default fee rate as a guardrail.
- The sender attempts to deliver `final_amount + max_fee_amount` to the first trampoline.
- Any fee spent on the outer route reduces what the first trampoline receives.
- The remaining fee budget is split evenly across trampoline hops and passed as `build_max_fee_amount`.

# Trampoline Routing Specification

## Summary

### What it does
- The sender only needs a route to the first trampoline node.
- The remaining route is encoded in an inner trampoline onion and forwarded hop‑by‑hop.
- The final recipient only sees the final payload and settles the payment.

### How to use it (RPC)
- Set `trampoline_hops` in SendPayment RPC to explicitly list trampoline nodes.
  - Reference: [../../crates/fiber-lib/src/rpc/payment.rs](../../crates/fiber-lib/src/rpc/payment.rs#L131-L148)

### Fee behavior
- A recommended min/max fee is estimated using the default fee rate as a guardrail.
- The sender routes to the first trampoline with `final_amount + max_fee_amount`.
- Any fee spent on the outer route reduces what the first trampoline receives.
- The remaining fee budget is split evenly across trampoline hops and carried as `build_max_fee_amount`.
  - Construction: [../../crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1294-L1453)

The forwarding fee calculation follows the standard TLC fee formula:

$$
fee = \left\lceil \frac{amount\_to\_forward \times fee\_rate}{1{,}000{,}000} \right\rceil
$$

- Implementation: [../../crates/fiber-lib/src/fiber/fee.rs](../../crates/fiber-lib/src/fiber/fee.rs#L115-L142)

### Expiry behavior
- Each remaining trampoline hop adds a default expiry delta.
- The total delta must not exceed `tlc_expiry_limit`.
  - Implementation: [../../crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1481-L1507)

### Forwarding behavior
- A trampoline node peels the inner onion, validates amounts, and builds a new payment to the next hop.
- It checks that the incoming amount covers `amount_to_forward` and that the available fee matches `build_max_fee_amount`.
  - Implementation: [../../crates/fiber-lib/src/fiber/network.rs](../../crates/fiber-lib/src/fiber/network.rs#L2438-L2550)

### Known constraints
- Max trampoline hops: `MAX_TRAMPOLINE_HOPS_LIMIT` (5)
  - [../../crates/fiber-lib/src/fiber/payment.rs](../../crates/fiber-lib/src/fiber/payment.rs#L46-L50)
- Trampoline packet size: `TRAMPOLINE_PACKET_DATA_LEN`
  - [../../crates/fiber-lib/src/fiber/types.rs](../../crates/fiber-lib/src/fiber/types.rs#L3830-L3846)
- `max_fee_amount` must be provided when `trampoline_hops` is set.
  - [../../crates/fiber-lib/src/fiber/payment.rs](../../crates/fiber-lib/src/fiber/payment.rs#L365-L385)

### Key payloads
- Trampoline hop payloads: [../../crates/fiber-lib/src/fiber/types.rs](../../crates/fiber-lib/src/fiber/types.rs#L3773-L3818)
- Trampoline onion packet: [../../crates/fiber-lib/src/fiber/types.rs](../../crates/fiber-lib/src/fiber/types.rs#L3830-L3846)

### Tests to validate
- Graph‑level construction tests: [../../crates/fiber-lib/src/fiber/tests/graph.rs](../../crates/fiber-lib/src/fiber/tests/graph.rs)
- End‑to‑end trampoline tests: [../../crates/fiber-lib/src/fiber/tests/trampoline.rs](../../crates/fiber-lib/src/fiber/tests/trampoline.rs)

## Detailed Specification

### Overview
Trampoline routing lets a sender with limited routing information delegate pathfinding to intermediate trampoline nodes. The sender builds a normal outer onion to the first trampoline and embeds an inner trampoline onion that encodes the remaining hops.

```
Sender → [Outer Onion] → First Trampoline → [Inner Onion] → Next Trampoline → ... → Final Recipient
```

### Sender: route construction
1. Validate `trampoline_hops` and fee budget, and ensure all hops support trampoline routing.
2. Build a route to the first trampoline for `final_amount + max_fee_amount`.
3. Allocate the remaining fee budget across trampoline hops as `build_max_fee_amount`.
4. Build the inner trampoline onion and embed it in the last hop of the outer route.

Implementation details:
- Route construction and fee allocation: [../../crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1294-L1453)
- Default‑fee estimation: [../../crates/fiber-lib/src/fiber/graph.rs](../../crates/fiber-lib/src/fiber/graph.rs#L1457-L1478)

### Trampoline forwarding
When a trampoline node receives a payment with an embedded trampoline onion, it:
1. Peels the inner onion.
2. Validates the forward payload (amount, fee budget, expiry constraints).
3. Starts a new payment to the next hop with `build_max_fee_amount` as its routing budget.

Implementation:
- [../../crates/fiber-lib/src/fiber/network.rs](../../crates/fiber-lib/src/fiber/network.rs#L2438-L2550)

### Final recipient processing
The final recipient validates the trampoline final payload and settles the TLC with the provided preimage.

Implementation:
- [../../crates/fiber-lib/src/fiber/channel.rs](../../crates/fiber-lib/src/fiber/channel.rs#L1182-L1195)

