# Copilot Instructions for Fiber Network Node (FNN)

## Build, test, and lint commands

- Toolchain: Rust `1.93.0` (`rust-toolchain.toml`), default workspace members are `fnn` and `fiber-bin`.
- Build:
  - `cargo build --locked`
  - `cargo build --release --locked`
  - `make check` (CI-style check matrix: debug/release/no-default-features + migrate crate check)
- Tests (prefer `nextest`):
  - `cargo nextest run --no-fail-fast`
  - `RUST_TEST_THREADS=2 cargo nextest run --no-fail-fast` (matches CI and `.config/nextest.toml` expectations)
  - `cargo nextest run -p fnn -p fiber-bin`
  - Single test: `cargo nextest run test_name`
  - Single test pattern: `cargo nextest run 'tests::channel::test_name'`
  - Fallback when needed: `RUST_LOG=off cargo test -p fnn -p fiber-bin`
- Lint/format:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -p fnn -p fiber-bin -- -D warnings`
  - `make clippy` (also checks wasm crates)
  - `typos`

## High-level architecture

- Runtime bootstrap is in `crates/fiber-bin/src/main.rs`:
  1. Parse config and open `Store`.
  2. Start root actor/task tracking.
  3. Start CKB chain actor and initialize contracts/chain hash.
  4. Build `NetworkGraph` and start `NetworkActor` via `start_network`.
  5. Start built-in watchtower actor (or standalone watchtower RPC client), depending on config.
  6. Start JSON-RPC server (`rpc::server::start_rpc`) with enabled modules.
- Core responsibilities in `crates/fiber-lib`:
  - `fiber/`: payment-channel protocol actors and flows (`network`, `channel`, `payment`, `gossip`, `graph`).
  - `ckb/`: CKB integration and on-chain transaction handling.
  - `rpc/`: jsonrpsee RPC modules (`info`, `peer`, `channel`, `payment`, `invoice`, `graph`, etc.).
  - `store/`: persistence traits/impl/schema.
- Data compatibility flow:
  - Persistent key schema lives in `crates/fiber-lib/src/store/schema.rs`.
  - Storage migrations live in `migrate/src/migrations/` and are validated in CI.

## Key conventions specific to this repository

- Identity boundary: prefer `Pubkey` for business logic and RPC input/output; keep `PeerId` at p2p transport boundaries (multiaddr/session handling).
- Actor-first design: cross-component interactions should go through actor message enums (`NetworkActorCommand`, `NetworkActorMessage`, service events) rather than direct shared-state mutation.
- RPC documentation is generated and CI-checked: after RPC API/doc comment changes, run `make gen-rpc-doc` and ensure `make check-dirty-rpc-doc` passes.
- Persistence changes require migration discipline: when storage structures/keys change, update migration code and run `make check-migrate`.
- WASM is a first-class target for shared code paths; when touching cross-platform code in `fiber-lib`, keep `cfg` guards intact and run wasm-target clippy via `make clippy`.
- Test execution is tuned by `.config/nextest.toml` overrides; prefer `nextest` over plain `cargo test` for reliability and CI parity.
