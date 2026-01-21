# Trampoline Routing Specification

## Overview

Trampoline routing is a payment routing mechanism that allows nodes with limited routing information to make payments by delegating pathfinding to intermediate "trampoline" nodes. This document describes the implementation of trampoline routing in Fiber Network, inspired by the [Lightning Network BOLT proposal](https://github.com/lightning/bolts/blob/master/proposals/trampoline.md).

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
pub struct TrampolineOnionPacket {
    data: Vec<u8>,
}
```

- Uses Sphinx onion construction (same as outer payment onion)
- Smaller packet size: `TRAMPOLINE_PACKET_DATA_LEN = 1300` bytes
- Embedded in the `trampoline_onion` field of outer payment hop payload

#### 2. Trampoline Hop Payload

Two types of payloads ([`types.rs`](../../crates/fiber-lib/src/fiber/types.rs#L3778)):

**Forward Payload** (for trampoline nodes):
```rust
TrampolineHopPayload::Forward {
    next_node_id: Pubkey,              // Next hop in trampoline route
    amount_to_forward: u128,           // Amount to forward (excluding fees)
    build_max_fee_amount: Option<u128>, // Fee budget for outer route construction
    tlc_expiry_delta: u64,             // Expiry delta for next hop
    tlc_expiry_limit: u64,             // Upper bound on expiry
    max_parts: Option<u64>,            // MPP support flag
}
```

**Final Payload** (for recipient):
```rust
TrampolineHopPayload::Final {
    final_amount: u128,                // Amount recipient should receive
    final_tlc_expiry_delta: u64,       // Final expiry delta
    payment_preimage: Option<Hash256>, // For keysend payments
    custom_records: Option<PaymentCustomRecords>, // MPP and custom data
}
```

## Payment Flow

### 1. Sender Initiates Trampoline Payment

The sender specifies trampoline nodes via RPC ([`payment.rs`](../../crates/fiber-lib/src/rpc/payment.rs#L143)):

```rust
pub struct SendPaymentCommand {
    // ... other fields ...
    pub trampoline_hops: Option<Vec<TrampolineHopParams>>,
}
```

Each trampoline hop specifies:
- `pubkey`: Trampoline node public key
- `fee_rate`: Optional service fee rate in ppm (default: `DEFAULT_FEE_RATE * 2`)
- `tlc_expiry_delta`: Optional expiry delta (default: `DEFAULT_TLC_EXPIRY_DELTA`)

### 2. Trampoline Route Construction

The route building process ([`graph.rs`](../../crates/fiber-lib/src/fiber/graph.rs#L1313)) consists of:

#### Step 1: Validate Trampoline Hops

- Must be non-empty
- Maximum 5 hops (`MAX_TRAMPOLINE_HOPS_LIMIT`)
- Cannot contain target node
- No duplicate nodes
- All nodes must support trampoline routing feature

#### Step 2: Calculate Trampoline Service Fees

Service fees are calculated backwards from final recipient:

```rust
let mut next_amount_to_forward = final_amount;
for hop in trampoline_hops.iter().rev() {
    forward_amounts[idx] = next_amount_to_forward;

    let fee_rate_ppm = hop.fee_rate
        .unwrap_or(DEFAULT_FEE_RATE * 2) as u128;
    let fee = calculate_tlc_forward_fee(next_amount_to_forward, fee_rate_ppm)?;

    next_amount_to_forward = next_amount_to_forward.saturating_add(fee);
}
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

## Fee Model

### Service Fees vs Routing Fees

**Trampoline Service Fee**: Fee charged by trampoline node for pathfinding service
- Configured per-hop via `fee_rate` (ppm)
- Default: `DEFAULT_FEE_RATE * 2`
- Calculated and reserved upfront

**Routing Fee**: Fee paid to intermediate channel hops in outer payment onion
- Dynamically calculated during pathfinding
- Budget allocated from `max_fee_amount - service_fee_total`

### Fee Budget Calculation

When `max_fee_amount` is not specified, a default budget is calculated:

```rust
let single_forward_fee = calculate_tlc_forward_fee(
    amount_to_first_trampoline,
    DEFAULT_FEE_RATE as u128,
)?;

let default_min_fee = trampoline_service_fee_total
    + trampoline_hops.len() as u128 * single_forward_fee;

let default_max_fee = trampoline_service_fee_total
    + trampoline_hops.len() as u128 * single_forward_fee * 10;
```

If user-specified `max_fee_amount` is too low:
```
Error: max_fee_amount too low for trampoline service fees:
       recommend_minimal_fee={}, maximal_fee={}, current_fee={}
```

## Expiry Delta Handling

Trampoline routing accumulates expiry deltas across hops:

```rust
fn trampoline_forward_expiry_delta(
    &self,
    base_final: u64,
    remaining_trampoline_hops: &[TrampolineHop],
    tlc_expiry_limit: u64,
) -> Result<u64, PathFindError> {
    let slack = remaining_trampoline_hops
        .iter()
        .map(|h| h.tlc_expiry_delta.unwrap_or(DEFAULT_TLC_EXPIRY_DELTA))
        .try_fold(0u64, |acc, d| acc.checked_add(d))?;

    let total = base_final.checked_add(slack)?;

    if total > tlc_expiry_limit {
        return Err(PathFindError::ExpiryLimitExceeded);
    }

    Ok(total)
}
```

## Multi-Part Payment (MPP) Support

Trampoline routing supports MPP when:
- `max_parts > 1` is specified
- The `max_parts` value is propagated through trampoline hops
- Each trampoline node can split payments when building outer routes

```rust
let max_parts = if payment_data.allow_mpp() {
    Some(payment_data.max_parts() as u64)
} else {
    None
};
```

## Settlement and Error Handling

### Forward Settlement

When a trampoline forwards a payment, it tracks:
```rust
pub struct TrampolineContext {
    remaining_trampoline_onion: Vec<u8>,
    previous_tlcs: Vec<PrevTlcInfo>,
}
```

The `previous_tlcs` links are used to settle HTLCs backwards when payment succeeds.

### Error Propagation

If forwarding fails, the trampoline returns:
```rust
Err(TlcErr::new_node_fail(
    TlcErrorCode::TemporaryNodeFailure,  // Or FeeInsufficient, etc.
    state.get_public_key(),
))
```

Errors propagate back through the payment chain, settling or failing HTLCs accordingly.

## Privacy Considerations

### Sender Privacy
- Sender only needs to know first trampoline node
- Intermediate trampoline nodes don't know sender identity (via outer onion)
- Route beyond first trampoline is hidden from sender

### Recipient Privacy
- Trampoline nodes know the final recipient
- But don't know the original sender
- Amount privacy depends on fee randomization

### Trampoline Node Privacy
- Each trampoline node only knows:
  - Previous hop (from outer onion)
  - Next hop (from inner onion)
- Full route is not visible to any single trampoline

## Feature Detection

Nodes advertise trampoline support via feature flags:

```rust
fn is_node_support_trampoline_routing(&self, pubkey: &Pubkey) -> bool {
    // Check if node has trampoline routing feature bit set
    self.get_node_features(pubkey)
        .map(|f| f.supports_trampoline())
        .unwrap_or(false)
}
```

## Limitations and Constraints

### Hard Limits
- Maximum trampoline hops: 5 (`MAX_TRAMPOLINE_HOPS_LIMIT`)
- Trampoline packet size: 1300 bytes (`TRAMPOLINE_PACKET_DATA_LEN`)
- Outer payment onion size: 6500 bytes (`PACKET_DATA_LEN`)

### Validation Rules
1. Trampoline hops must be non-empty when specified
2. Cannot contain duplicate nodes
3. Cannot contain target node
4. All nodes must support trampoline routing
5. Total expiry delta must not exceed `tlc_expiry_limit`

### Fee Requirements
- `max_fee_amount` must cover all trampoline service fees
- Remaining budget must be sufficient for routing fees
- Recommended minimum fee is calculated automatically

## Comparison with Standard Routing

| Aspect | Standard Routing | Trampoline Routing |
|--------|------------------|-------------------|
| Network Graph | Full graph required | Only need to know trampolines |
| Pathfinding | At sender only | Delegated to trampolines |
| Privacy | Route known to sender | Route partially hidden |
| Computational Cost | High (full graph) | Low (partial graph) |
| Fee Overhead | Channel fees only | Service fees + channel fees |
| Light Client Support | Difficult | Excellent |

## Implementation Files

Key implementation files:
- [`types.rs`](../../crates/fiber-lib/src/fiber/types.rs): `TrampolineOnionPacket`, `TrampolineHopPayload`
- [`graph.rs`](../../crates/fiber-lib/src/fiber/graph.rs): Route construction with trampoline support
- [`network.rs`](../../crates/fiber-lib/src/fiber/network.rs): `forward_trampoline_packet()` handler
- [`payment.rs`](../../crates/fiber-lib/src/fiber/payment.rs): `SendPaymentData`, `TrampolineHop`, `TrampolineContext`
- [`channel.rs`](../../crates/fiber-lib/src/fiber/channel.rs): Final recipient processing

## Test Coverage

Comprehensive tests in:
- [`tests/graph.rs`](../../crates/fiber-lib/src/fiber/tests/graph.rs): Route construction tests
- [`tests/trampoline.rs`](../../crates/fiber-lib/src/fiber/tests/trampoline.rs): End-to-end trampoline payment tests
- [`tests/types.rs`](../../crates/fiber-lib/src/fiber/tests/types.rs): Onion packet serialization tests

## Future Enhancements

Potential improvements:
1. **Dynamic Trampoline Selection**: Auto-select trampoline nodes based on network conditions
2. **Adaptive Fee Budgets**: Learn optimal fee allocations over time
3. **Trampoline Reputation**: Track success rates and adjust routing preferences
4. **Rendezvous Routing**: Combine trampoline with recipient-initiated routing
5. **Payment Metadata**: Support arbitrary data in trampoline payload (encrypted)

## References

- [Lightning Network Trampoline Proposal](https://github.com/lightning/bolts/blob/master/proposals/trampoline.md)
- [Eclair Trampoline Implementation](https://github.com/ACINQ/eclair/blob/master/docs/Trampoline.md)
- [Sphinx Onion Routing](https://cypherpunks.ca/~iang/pubs/Sphinx_Oakland09.pdf)
