---
title: Integer Overflow in Onion Packet Deserialization
category: security-issues
tags: [security, integer-overflow, rust, onion-routing, packet-parsing]
module: fiber
symptoms:
  - Integer overflow when deserializing malicious onion packets with u64::MAX length value
  - On 64-bit systems: wraps to 7 after overflow, causing incorrect parsing behavior
  - On 32-bit systems: truncation during cast causes similar incorrect parsing
  - Panic (denial of service) when subsequent slice operations fail bounds checks
  - Incorrect packet parsing when overflowed length passes initial validation
root_cause: Unsafe integer arithmetic in `get_hop_data_len` function - casting u64 to usize without bounds checking, then adding HOP_DATA_HEAD_LEN (8) without overflow protection. When untrusted input contains u64::MAX (0xFFFFFFFFFFFFFFFF), the addition overflows.
severity: high
date_documented: 2026-02-03
related_files:
  - crates/fiber-lib/src/fiber/types.rs
  - crates/fiber-lib/src/fiber/tests/types.rs
related_docs:
  - docs/specs/p2p-message.md
  - docs/glossary.md
---

# Integer Overflow in Onion Packet Deserialization

## Problem Statement

An integer overflow vulnerability was discovered in the onion packet deserialization code. When parsing untrusted network input, the `get_hop_data_len` function read a `u64` length value from bytes, cast it directly to `usize`, and added `HOP_DATA_HEAD_LEN` (8) without overflow protection.

### Attack Vector

An attacker could send a malicious onion packet with `0xFFFFFFFFFFFFFFFF` (u64::MAX) in the 8-byte length header, causing:

1. **On 64-bit platforms**: Integer overflow when adding 8, wrapping to 7
2. **On 32-bit platforms**: Truncation during the `as usize` cast
3. **Result**: Panic (DoS) from failed bounds checks, or incorrect parsing if the overflowed length happens to pass validation

Note: Rust's safe slice operations prevent actual out-of-bounds memory access; invalid indices result in a panic rather than memory corruption.

### Vulnerable Code

```rust
// VULNERABLE: types.rs:4079-4091 (before fix)
fn get_hop_data_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < HOP_DATA_HEAD_LEN {
        return None;
    }
    Some(
        u64::from_be_bytes(
            buf[0..HOP_DATA_HEAD_LEN].try_into().expect("u64 from slice"),
        ) as usize        // ← Unsafe cast!
            + HOP_DATA_HEAD_LEN,  // ← Unchecked addition!
    )
}
```

## Solution

### Fix 1: `get_hop_data_len_v0` Function

**Location**: `crates/fiber-lib/src/fiber/types.rs:4172-4184`

The function was refactored to support versioned hop data formats. The v0 format uses the original u64 BE length header:

```rust
/// Returns the total length of v0 hop data: u64 BE length + HOP_DATA_HEAD_LEN.
fn get_hop_data_len_v0(buf: &[u8]) -> Option<usize> {
    if buf.len() < HOP_DATA_HEAD_LEN {
        return None;
    }
    let len = u64::from_be_bytes(
        buf[0..HOP_DATA_HEAD_LEN]
            .try_into()
            .expect("u64 from slice"),
    );
    // Safe conversion: check value fits in usize and addition won't overflow.
    // Note: Caller (fiber-sphinx) is responsible for validating len against packet bounds.
    usize::try_from(len).ok()?.checked_add(HOP_DATA_HEAD_LEN)
}
```

**Why this works:**

1. `usize::try_from(len)` - Returns `None` if value exceeds `usize::MAX` (handles 32-bit platforms)
2. `.ok()?` - Propagates the `None` on conversion failure
3. `.checked_add(HOP_DATA_HEAD_LEN)` - Returns `None` if addition would overflow

### Fix 2: `deserialize` Method

**Location**: `crates/fiber-lib/src/fiber/types.rs:4069-4079`

The deserialize method now includes version validation and uses `checked_add` for overflow protection:

```rust
// Ensure backward compatibility
let mut shared_secret = NO_SHARED_SECRET;
if let (Some(rb_plus_32), Some(rb_plus_packet)) = (
    read_bytes.checked_add(32),
    read_bytes.checked_add(PACKET_DATA_LEN),
) {
    if data.len() >= rb_plus_32 && data.len() != rb_plus_packet {
        shared_secret.copy_from_slice(&data[read_bytes..rb_plus_32]);
        read_bytes = rb_plus_32;
    }
}
```

**Why this works:**

- Both `checked_add` calls must succeed before the block executes
- Defense in depth: protects against edge cases even if `get_hop_data_len` passes
- Unknown versions are now explicitly rejected before processing

### Test Coverage Added

**Location**: `crates/fiber-lib/src/fiber/tests/types.rs:291-404`

Tests are now organized by version (v0/v1) with separate functions for independent failure tracking:

| Test Function | Input | Expected |
|---------------|-------|----------|
| `test_peeled_onion_packet_deserialize_empty_input` | `[]` | Error (too short) |
| `test_peeled_onion_packet_deserialize_v0_u64_max_overflow` | `[v0, 0xFF × 8, 0x00]` | Error (overflow) |
| `test_peeled_onion_packet_deserialize_v0_large_claimed_length` | v0 + 1000 bytes claimed | Error (bounds) |
| `test_peeled_onion_packet_deserialize_v0_short_header` | v0 + 7 bytes | Error (need 8) |
| `test_peeled_onion_packet_deserialize_v0_exceeds_buffer` | v0 + 6501 claimed | Error (bounds) |
| `test_peeled_onion_packet_deserialize_v0_near_max_overflow` | v0 + `usize::MAX - 7` | Error (overflow) |
| `test_peeled_onion_packet_deserialize_v1_short_header` | v1 + 3 bytes | Error (need 4) |
| `test_peeled_onion_packet_deserialize_v1_large_claimed_length` | v1 + 1000 bytes claimed | Error (bounds) |
| `test_peeled_onion_packet_deserialize_unknown_version` | version=2 | Error (unknown) |

```rust
#[test]
fn test_peeled_onion_packet_deserialize_v0_u64_max_overflow() {
    // v0 format: [version=0][u64 BE length header with u64::MAX]
    let mut malicious_input = vec![ONION_PACKET_VERSION_V0];
    malicious_input.extend([255u8, 255, 255, 255, 255, 255, 255, 255, 0]);
    let result = PeeledPaymentOnionPacket::deserialize(&malicious_input);
    assert!(result.is_err(), "Should reject input with overflow-causing length");
}

// ... additional version-specific test functions
```

## Prevention Guidelines

### Code Review Checklist

When reviewing Rust network parsing code, flag these patterns:

- [ ] `as usize` after `from_be_bytes`/`from_le_bytes` - Use `usize::try_from()` instead
- [ ] Unchecked arithmetic on parsed values - Use `checked_add`/`checked_sub`/`checked_mul`
- [ ] Slice access using parsed indices - Validate bounds first
- [ ] Missing validation on network input - Always validate before use

### Safe Patterns

```rust
// ❌ VULNERABLE
let len = u64::from_be_bytes(bytes) as usize;
let offset = parsed_value + CONSTANT;

// ✅ SAFE
let len = usize::try_from(u64::from_be_bytes(bytes)).ok()?;
let offset = parsed_value.checked_add(CONSTANT)?;
```

### Testing Recommendations

1. **Fuzz testing**: Use `cargo-fuzz` on all packet parsing functions
2. **Boundary tests**: Test with `u64::MAX`, `usize::MAX`, `0`, near-max values
3. **Platform tests**: Run tests on both 32-bit and 64-bit targets in CI

## Related Patterns to Review

The following locations use similar byte parsing patterns and should be audited:

| Location | Pattern | Risk |
|----------|---------|------|
| `watchtower/actor.rs:267` | `u64::from_be_bytes` | Low (blockchain data) |
| `watchtower/actor.rs:802-804` | `u64::from_le_bytes` | Low (blockchain data) |
| `watchtower/actor.rs:1576` | `u64::from_le_bytes` (htlc_expiry) | Medium (witness data) |
| `store/store_impl/mod.rs:212` | `u64::from_le_bytes` | Low (internal DB keys) |

## References

- **PR**: [#1094](https://github.com/nervosnetwork/fiber/pull/1094)
- **Commit**: `88ce3831 fix: prevent integer overflow in onion packet deserialization`
- **Related specs**: [P2P Message Protocol](../../specs/p2p-message.md)
- **Glossary**: [Onion Routing](../../glossary.md)
