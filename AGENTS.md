# Agent Guidelines for Fiber Network Node (FNN)

This document provides essential guidelines for AI coding agents working on the Fiber Network Node codebase. The Fiber Network is a reference implementation of a peer-to-peer payment/swap network built on CKB blockchain, similar to Lightning Network.

## Build System & Commands

### Primary Language & Toolchain
- **Language**: Rust 1.85.0 (specified in `rust-toolchain.toml`)
- **Build System**: Cargo workspace with 5 member crates
- **Test Runner**: cargo-nextest (preferred over `cargo test`)

### Essential Commands

#### Building
```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo check --locked           # Quick check with Cargo.lock
make check                     # Full check (debug, release, no-default-features)
```

#### Testing
```bash
# Run all tests with nextest (PREFERRED)
cargo nextest run --no-fail-fast

# Run specific package tests
cargo nextest run -p fnn -p fiber-bin

# Run single test by name
cargo nextest run test_name

# Run tests matching pattern
cargo nextest run 'test_pattern'

# Standard cargo test (if nextest unavailable)
RUST_LOG=off cargo test -p fnn -p fiber-bin

# Run benchmarks
cargo criterion --features bench
make benchmark-test
```

**Note**: The `.config/nextest.toml` configures thread requirements for heavy/channel/payment tests. Tests require `RUST_TEST_THREADS: 2` in CI.

#### Linting & Formatting
```bash
# Format code (REQUIRED before commits)
cargo fmt --all

# Check formatting without modifying
cargo fmt --all -- --check
make fmt

# Run clippy (REQUIRED, must pass with no warnings)
cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings
make clippy

# Auto-fix clippy warnings and format
make bless

# Check for typos
typos
typos -w  # Auto-fix typos
```

#### WASM-specific
```bash
# Check WASM crates
cargo clippy -p fiber-wasm -p fiber-wasm-db-worker -p fiber-wasm-db-common --target wasm32-unknown-unknown -- -D warnings
```

#### Other Checks
```bash
# Check for unused dependencies
cargo shear

# Generate RPC documentation
make gen-rpc-doc

# Verify data migration schemas
make check-migrate

# Update migration schemas
make update-migrate-check
```

## Code Style Guidelines

### Module & Import Organization
1. **Import ordering** (top to bottom):
   - Standard library (`use std::...`)
   - External crates (alphabetical)
   - Internal crate modules (`use crate::...`)
   - Local modules (`use super::...`)
2. Group imports by category with blank lines between groups
3. Use explicit imports, avoid glob imports except for preludes

### Naming Conventions
- **Files**: Snake_case (e.g., `channel_actor.rs`, `payment_handler.rs`)
- **Types**: PascalCase (e.g., `ChannelActor`, `PaymentRequest`)
- **Functions/Variables**: Snake_case (e.g., `process_message`, `peer_id`)
- **Constants**: SCREAMING_SNAKE_CASE (e.g., `MAX_TLC_VALUE`, `DEFAULT_TIMEOUT`)
- **Modules**: Snake_case (e.g., `fiber::channel`, `ckb::actor`)

### Type Annotations
- Use explicit types for public APIs and struct fields
- Type inference is acceptable for local variables when clear
- Always annotate function return types
- Use `#[serde_as]` and `serde_with` for complex serialization (e.g., `U128Hex`, `U64Hex`)

### Error Handling
- Use `thiserror::Error` for custom error types (see `src/errors.rs`)
- Prefer `Result<T, Error>` over `Result<T, SomeError>`
- Use `?` operator for error propagation
- Provide descriptive error messages with context
- Don't panic in production code; use `expect()` only when truly unreachable

**Example**:
```rust
#[derive(Error, Debug)]
pub enum Error {
    #[error("Peer not found error: {0:?}")]
    PeerNotFound(PeerId),
    #[error("Channel not found error: {0:?}")]
    ChannelNotFound(Hash256),
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
}

pub type Result<T> = std::result::Result<T, Error>;
```

### Async & Actor Patterns
- This codebase uses the **Ractor** actor framework extensively
- Use `async_trait::async_trait` for async traits
- For WASM compatibility, use `#[cfg_attr(target_arch="wasm32", async_trait::async_trait(?Send))]`
- Actor messages use enums (e.g., `ChannelActorMessage`, `NetworkActorMessage`)
- Use `ractor::call` for RPC-style actor communication

**Example**:
```rust
#[cfg_attr(target_arch="wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Actor for MyActor {
    type Msg = MyActorMessage;
    type State = MyState;
    type Arguments = MyArgs;
    // Implementation...
}
```

### Logging & Tracing
- Use `tracing` crate for structured logging
- Levels: `trace`, `debug`, `info`, `warn`, `error`
- Include context in log messages (peer_id, channel_id, etc.)
- Log at appropriate levels:
  - `error!`: Critical failures requiring attention
  - `warn!`: Unexpected but recoverable situations
  - `info!`: Important state changes
  - `debug!`: Detailed operational info
  - `trace!`: Very verbose, protocol-level details

### Documentation
- Public APIs MUST have doc comments (`///`)
- RPC methods require detailed documentation (checked by `make gen-rpc-doc`)
- Use `/// # Example` sections for complex APIs
- Document invariants and assumptions
- Explain "why" in comments, not just "what"

### Testing Conventions
- Unit tests in same file: `#[cfg(test)] mod tests { ... }`
- Integration tests in `tests/` directory
- Use descriptive test names: `test_channel_open_accept_flow`
- Mock actors and external dependencies
- Test both success and error paths

### Platform-Specific Code
- Use `#[cfg(not(target_arch = "wasm32"))]` for native-only code
- Use `#[cfg(target_arch = "wasm32")]` for WASM-only code
- Keep platform-specific code minimal and isolated
- Test both native and WASM builds in CI

### Serialization
- Use `serde` with `#[derive(Serialize, Deserialize)]`
- Use `serde_with` for custom serialization (hex, base64, etc.)
- Use `molecule` for CKB-specific binary formats
- Maintain backward compatibility for persisted data

### Security & Best Practices
- No unsafe code without thorough review
- Validate all external inputs (RPC, peer messages)
- Use constant-time operations for cryptographic comparisons
- Clear sensitive data (private keys) when done
- Follow principle of least privilege

## Allowed Clippy Lints

The following clippy lints are explicitly allowed (see `Cargo.toml`):
- `expect-fun-call`: Acceptable to use `expect()` with computed messages
- `fallible-impl-from`: Allow `From` impls that can panic
- `large-enum-variant`: Acceptable for actor message enums
- `mutable-key-type`: Needed for certain data structures
- `needless-return`: Explicit returns are sometimes clearer
- `upper-case-acronyms`: Allow acronyms like `TLC`, `RPC`, `UDT`

## Project Structure

```
fiber/
├── crates/
│   ├── fiber-lib/         # Core library (fnn crate)
│   │   └── src/
│   │       ├── fiber/     # Protocol implementation
│   │       ├── ckb/       # CKB blockchain integration
│   │       ├── rpc/       # JSON-RPC API
│   │       ├── store/     # Data persistence
│   │       └── watchtower/ # Watchtower service
│   ├── fiber-bin/         # Binary executable
│   ├── fiber-wasm/        # WebAssembly bindings
│   └── fiber-wasm-db-*/   # WASM database workers
├── tests/                 # Integration tests
├── migrate/               # Database migration tool
└── Makefile              # Convenience targets
```

## Common Pitfalls

1. **Don't forget to run tests with nextest**, not standard `cargo test`
2. **Always run `make clippy` before committing** - CI enforces `-D warnings`
3. **Check WASM compatibility** for code in `fiber-lib` - it must compile for both native and WASM
4. **Update migration schemas** if you change data structures (`make check-migrate`)
5. **Regenerate RPC docs** if you modify RPC methods (`make gen-rpc-doc`)
6. **Use `cargo fmt` before committing** - formatting is strictly enforced

## Continuous Integration

CI runs the following checks (all must pass):
- `make check` (multiple build configurations)
- `make check-migrate` (migration schema validation)
- `make check-dirty-rpc-doc` (RPC documentation up-to-date)
- `cargo nextest run --no-fail-fast` (all tests)
- `cargo fmt --all -- --check` (formatting)
- `make clippy` (linting with warnings as errors)
- `typos` (spell checking)
- `cargo shear` (unused dependencies)

## Additional Resources

- RPC API documentation: `crates/fiber-lib/src/rpc/README.md` (auto-generated)
- Protocol specifications: `docs/specs/`
- Development notes: `docs/notes/`
