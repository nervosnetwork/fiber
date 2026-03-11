# Fuzz Testing & Deterministic Testing Plan for Fiber Network Node

## 1. Fuzz Testing with cargo-fuzz / libFuzzer

### Setup

Create a `fuzz/` directory under `crates/fiber-lib/` with a dedicated `Cargo.toml`:

```
crates/fiber-lib/fuzz/
├── Cargo.toml
├── fuzz_targets/
│   ├── fuzz_fiber_message.rs
│   ├── fuzz_gossip_message.rs
│   ├── fuzz_invoice.rs
│   ├── fuzz_onion_packet.rs
│   ├── fuzz_store_deserialize.rs
│   ├── fuzz_path_finding.rs
│   └── fuzz_tlc_state_machine.rs
└── corpus/
    ├── fuzz_fiber_message/
    ├── fuzz_invoice/
    └── ...
```

`fuzz/Cargo.toml`:
```toml
[package]
name = "fnn-fuzz"
version = "0.0.0"
publish = false
edition = "2021"

[package.metadata]
cargo-fuzz = true

[dependencies]
libfuzzer-sys = "0.4"
fnn = { path = "..", features = ["sample"] }
arbitrary = { version = "1", features = ["derive"] }

[[bin]]
name = "fuzz_fiber_message"
path = "fuzz_targets/fuzz_fiber_message.rs"
doc = false

[[bin]]
name = "fuzz_gossip_message"
path = "fuzz_targets/fuzz_gossip_message.rs"
doc = false

[[bin]]
name = "fuzz_invoice"
path = "fuzz_targets/fuzz_invoice.rs"
doc = false

[[bin]]
name = "fuzz_onion_packet"
path = "fuzz_targets/fuzz_onion_packet.rs"
doc = false

[[bin]]
name = "fuzz_store_deserialize"
path = "fuzz_targets/fuzz_store_deserialize.rs"
doc = false
```

### Fuzz Target Priority (3 Tiers)

#### Tier 1 — Pure Function Deserialization (Highest Value, Easiest)

These are stateless, pure parsing functions. Crash = bug.

| Target | Entry Point | Why |
|--------|-------------|-----|
| `fuzz_fiber_message` | `FiberMessage::from_molecule_slice(&data)` | P2P message parsing — untrusted network input |
| `fuzz_gossip_message` | `GossipMessage::from_molecule_slice(&data)` | Gossip protocol parsing — untrusted network input |
| `fuzz_invoice` | `CkbInvoice::from_str(s)` | bech32m invoice decoding — user-provided input |
| `fuzz_onion_packet` | `PeeledOnionPacket::deserialize(...)` | Onion routing — untrusted forwarded data |
| `fuzz_store_deserialize` | `bincode::deserialize::<T>(&data)` for each store type | Persistence layer — corrupted data resilience |

Example — `fuzz_fiber_message.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use fnn::fiber::types::FiberMessage;

fuzz_target!(|data: &[u8]| {
    let _ = FiberMessage::from_molecule_slice(data);
});
```

Example — `fuzz_invoice.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use fnn::invoice::CkbInvoice;
use std::str;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = str::from_utf8(data) {
        let _ = CkbInvoice::from_str(s);
    }
});
```

#### Tier 2 — Structured Fuzz with `arbitrary` (Medium Effort)

Use the `Arbitrary` derive macro to generate structured inputs for more complex surfaces.

| Target | Entry Point | Why |
|--------|-------------|-----|
| Path finding | `find_path(source, target, amount, ...)` | Complex graph algorithm with edge cases |
| Serialization roundtrip | Encode → decode → assert equality | Catch asymmetries in molecule/bincode codecs |
| Channel parameter validation | `OpenChannel` / `AcceptChannel` field ranges | Ensure parameter bounds are enforced |

Example — roundtrip fuzz:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use arbitrary::Arbitrary;
use fnn::fiber::types::FiberMessage;

fuzz_target!(|data: &[u8]| {
    // Parse message from fuzz data
    if let Ok(msg) = FiberMessage::from_molecule_slice(data) {
        // Re-serialize and re-parse
        let bytes = msg.to_molecule_bytes();
        let msg2 = FiberMessage::from_molecule_slice(&bytes)
            .expect("roundtrip should succeed");
        // Could also assert structural equality if PartialEq is derived
    }
});
```

#### Tier 3 — Stateful Fuzz / Property-Based Testing (Highest Effort, Highest Impact)

Use `proptest` or `arbitrary` to generate sequences of operations and test state machine invariants.

**Best candidate**: The existing `TlcActor` in `tests/tlc_op.rs` is already a standalone state machine simulator that processes sequences of `Apply(TlcOp)` and `Sign` commands. This can be directly fuzzed.

```rust
// proptest strategy for TLC operation sequences
use proptest::prelude::*;
use proptest::collection::vec;

fn tlc_op_strategy() -> impl Strategy<Value = TlcOp> {
    prop_oneof![
        (any::<u64>(), any::<[u8; 32]>()).prop_map(|(amount, hash)| {
            TlcOp::AddTlc { amount, payment_hash: Hash256::from(hash) }
        }),
        any::<u64>().prop_map(|id| TlcOp::RemoveTlc { id }),
    ]
}

fn command_strategy() -> impl Strategy<Value = TlcActorCommand> {
    prop_oneof![
        tlc_op_strategy().prop_map(TlcActorCommand::Apply),
        Just(TlcActorCommand::Sign),
        Just(TlcActorCommand::RevokeAndAck),
    ]
}

proptest! {
    #[test]
    fn test_tlc_state_machine_invariants(
        commands in vec(command_strategy(), 1..100)
    ) {
        let mut actor = TlcActor::new();
        for cmd in commands {
            let _ = actor.handle(cmd); // should never panic
        }
        // Assert invariants:
        // - committed TLC count >= 0
        // - no duplicate TLC IDs
        // - total value within bounds
    }
}
```

---

## 2. Actor-Based State Machine Testing

### Current Test Infrastructure

The codebase already has good testing infrastructure for actors:

- **`MockChainActor`**: Simulates the CKB chain actor for channel tests
- **`NetworkNode`**: Builder pattern for spinning up full node instances in tests
- **`TlcActor`** (in `tests/tlc_op.rs`): Standalone TLC state machine that doesn't require ractor at all — **best target for deterministic testing**

### Strategy: Extract Core Logic from Actors

The most practical approach for making actor logic testable:

1. **Separate pure state transitions from actor message handling**:
   ```rust
   // Pure function — easy to test/fuzz
   fn process_open_channel(
       state: &mut ChannelState,
       msg: &OpenChannel,
   ) -> Result<AcceptChannel, Error> { ... }

   // Actor handler — just delegates
   async fn handle(&self, msg: ChannelActorMessage, state: &mut State) {
       match msg {
           ChannelActorMessage::PeerMessage(OpenChannel(m)) => {
               match process_open_channel(&mut state.channel, &m) {
                   Ok(response) => self.send(response),
                   Err(e) => self.close_with_error(e),
               }
           }
       }
   }
   ```

2. **Create a `ChannelStateMachine` struct** similar to the existing `TlcActor`:
   ```rust
   struct ChannelStateMachine {
       local_state: ChannelState,
       remote_state: ChannelState,
   }

   impl ChannelStateMachine {
       fn apply(&mut self, who: Role, msg: ChannelMessage) -> Result<Vec<ChannelMessage>, Error>;
       fn check_invariants(&self) -> Result<(), String>;
   }
   ```

3. **Fuzz the state machine** with `proptest` sequences of valid/invalid messages.

---

## 3. Deterministic Testing Strategies

### Ractor Limitations

Ractor (the actor framework used by Fiber) has **no built-in support** for:
- Deterministic scheduling
- Virtual clocks
- Simulation testing
- Mock actors / test harnesses

This means deterministic testing must be achieved through other means.

### Approach A: Message Record / Replay (Medium Effort)

Record all actor messages during a test run, then replay deterministically:

```rust
#[derive(Clone, Serialize, Deserialize)]
struct MessageLog {
    entries: Vec<LogEntry>,
}

struct LogEntry {
    timestamp: u64,
    from: ActorId,
    to: ActorId,
    message: SerializedMessage,
}

// Wrapper around actor that logs all messages
struct InstrumentedActor<A: Actor> {
    inner: A,
    log: Arc<Mutex<MessageLog>>,
}
```

**Pros**: Works with existing ractor, captures real execution traces.
**Cons**: Non-deterministic recording, replay may diverge if logic depends on timing.

### Approach B: Extract Pure Functions (Recommended, Low Effort)

Factor out all decision-making logic into pure functions that can be tested without actors:

```rust
// ❌ Logic buried in actor handler
async fn handle(&self, msg: NetworkMsg, state: &mut State) {
    match msg {
        NetworkMsg::PeerConnected(peer) => {
            if state.peers.len() >= MAX_PEERS { return; }
            state.peers.insert(peer.id, peer);
            // ... complex logic
        }
    }
}

// ✅ Logic extracted into testable function
fn handle_peer_connected(
    peers: &mut HashMap<PeerId, PeerInfo>,
    peer: PeerInfo,
    max_peers: usize,
) -> Result<Vec<SideEffect>, Error> {
    if peers.len() >= max_peers {
        return Err(Error::TooManyPeers);
    }
    peers.insert(peer.id, peer.clone());
    Ok(vec![SideEffect::SendHandshake(peer.id)])
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_peer_connected_max_limit() {
        let mut peers = HashMap::new();
        // Fill to max
        for i in 0..MAX_PEERS {
            let peer = make_peer(i);
            handle_peer_connected(&mut peers, peer, MAX_PEERS).unwrap();
        }
        // Next should fail
        let result = handle_peer_connected(&mut peers, make_peer(999), MAX_PEERS);
        assert!(result.is_err());
    }
}
```

**Pros**: Fully deterministic, fast, easy to fuzz with proptest.
**Cons**: Requires refactoring actor handlers (can be done incrementally).

### Approach C: Turmoil for Network Simulation (High Effort, Future)

[Turmoil](https://github.com/tokio-rs/turmoil) (by tokio-rs) provides deterministic single-threaded simulation of networked systems:

```rust
use turmoil::Builder;

#[test]
fn test_channel_open_under_partition() {
    let mut sim = Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .build();

    sim.host("alice", || async {
        let node = start_fiber_node(config_alice()).await;
        node.open_channel("bob", 1000).await?;
        Ok(())
    });

    sim.host("bob", || async {
        let node = start_fiber_node(config_bob()).await;
        // Accept channel
        Ok(())
    });

    // Partition network after channel open
    sim.partition("alice", "bob");
    sim.run().unwrap();
}
```

**Pros**: True deterministic simulation, network fault injection, reproducible failures.
**Cons**: Requires replacing real TCP with turmoil's simulated TCP. Ractor uses internal tokio channels — significant integration work needed.

### Framework Comparison

| Framework | Deterministic | Network Sim | Ractor Compatible | Effort |
|-----------|:---:|:---:|:---:|:---:|
| **proptest** (state machine) | ✅ | ❌ | ✅ (no actors needed) | Low |
| **cargo-fuzz** (libfuzzer) | ❌ | ❌ | ✅ (no actors needed) | Low |
| **turmoil** (tokio-rs) | ✅ | ✅ | ⚠️ (high integration) | High |
| **madsim** (madeng) | ✅ | ✅ | ❌ (replaces tokio) | Very High |
| **Message replay** | Partial | ❌ | ✅ | Medium |

---

## 4. Recommended Implementation Order

### Phase 1 — Quick Wins (1-2 days)
1. Set up `crates/fiber-lib/fuzz/` with cargo-fuzz
2. Implement Tier 1 fuzz targets:
   - `fuzz_fiber_message` (P2P message parsing)
   - `fuzz_gossip_message` (gossip parsing)
   - `fuzz_invoice` (invoice decoding)
   - `fuzz_onion_packet` (onion routing)
3. Seed corpus from existing test fixtures and serialized protocol messages
4. Run locally for a few hours, fix any crashes

### Phase 2 — Property-Based Testing (3-5 days)
1. Add `proptest` as a dev-dependency
2. Fuzz the `TlcActor` state machine (already standalone, perfect candidate)
3. Add serialization roundtrip property tests for all molecule types
4. Add `arbitrary` derives to key types behind a feature flag

### Phase 3 — Actor Logic Extraction (Ongoing)
1. Identify the most complex actor handlers (channel state transitions, payment forwarding)
2. Extract core logic into pure functions (`fn handle_X(state, input) -> Result<(new_state, effects)>`)
3. Add property-based tests for the extracted functions
4. Use `proptest` state machine testing to verify sequences of operations

### Phase 4 — CI Integration
1. Add a nightly CI job that runs fuzz targets for N minutes each
2. Archive corpus in the repo (committed to `fuzz/corpus/`)
3. Track coverage with `cargo-fuzz coverage` and report regressions
4. Add `proptest` tests to the regular CI test suite

```yaml
# .github/workflows/fuzz.yml
name: Fuzz Testing
on:
  schedule:
    - cron: '0 2 * * *'  # Nightly at 2am
  workflow_dispatch:

jobs:
  fuzz:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        target:
          - fuzz_fiber_message
          - fuzz_gossip_message
          - fuzz_invoice
          - fuzz_onion_packet
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
      - run: cargo install cargo-fuzz
      - run: |
          cd crates/fiber-lib
          cargo fuzz run ${{ matrix.target }} -- -max_total_time=600
      - uses: actions/upload-artifact@v4
        if: failure()
        with:
          name: fuzz-artifacts-${{ matrix.target }}
          path: crates/fiber-lib/fuzz/artifacts/
```

---

## 5. Key Considerations

### WASM Compatibility
- Fuzz targets only need to build for native (x86_64), not WASM
- The `fuzz/` directory is a separate crate, won't affect WASM builds
- If extracting logic for testing, ensure extracted functions remain WASM-compatible

### Existing Test Pattern: TlcActor
The `TlcActor` in `tests/tlc_op.rs` is the gold standard in this codebase for deterministic state machine testing. It:
- Runs without ractor (pure state machine)
- Simulates both sides of a channel
- Processes sequences of `Apply(TlcOp)` and `Sign` commands
- Validates invariants after each step

**This pattern should be replicated for other subsystems** (channel state machine, payment forwarding, gossip protocol).

### Store/Persistence Fuzzing
With the `StoreSample` trait already implemented for all store types, creating corpus seeds is straightforward:
```rust
let sample = ChannelActorState::sample();
let bytes = bincode::serialize(&sample).unwrap();
std::fs::write("fuzz/corpus/fuzz_store_deserialize/seed_channel_state", bytes).unwrap();
```
