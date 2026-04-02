# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [v0.8.0] - 2026-03-28

### Breaking Changes

#### RPC: PeerId replaced with Pubkey

All RPC methods and types that previously used `peer_id` (base58-encoded libp2p PeerId) now use `pubkey` (hex-encoded compressed secp256k1 public key). ([#1154])

- `open_channel`: parameter `peer_id` renamed to `pubkey`, type changed from PeerId to Pubkey
- `list_channels`: parameter `peer_id` renamed to `pubkey`
- `disconnect_peer`: parameter `peer_id` renamed to `pubkey`
- `connect_peer`: parameter `address` changed from required `MultiAddr` to optional `String`; new optional parameter `pubkey` added (at least one of `address` or `pubkey` must be provided)
- `node_info` response: field `node_id` renamed to `pubkey`; `addresses` type changed from `Vec<MultiAddr>` to `Vec<String>`
- `Channel` type: field `peer_id` renamed to `pubkey`
- `PeerInfo` type: field `peer_id` removed; `address` type changed from `MultiAddr` to `String`
- `NodeInfo` (graph) type: field `node_id` renamed to `pubkey`; `addresses` type changed to `Vec<String>`

#### RPC: JSON type serialization overhaul

A new `fiber-json-types` crate was introduced for RPC-facing types, changing several serialization formats. ([#1169])

- `Attribute` (invoice) enum variants changed from PascalCase to snake_case (e.g., `FinalHtlcTimeout` → `final_htlc_timeout`)
- `Attribute::UdtScript` inner type changed from structured `CkbScript` to hex string
- `Attribute::PayeePublicKey` inner type changed from `PublicKey` to `Pubkey`
- `HashAlgorithm` enum variants changed from PascalCase to snake_case (e.g., `CkbHash` → `ckb_hash`)
- `CkbInvoice.signature` type changed from structured `InvoiceSignature` to hex string; `InvoiceSignature` type removed
- `CchInvoice::Fiber` and `CchInvoice::Lightning` now carry encoded invoice strings instead of structured objects
- `ChannelState` flags serialization changed to SCREAMING_SNAKE_CASE with pipe separator (e.g., `"OUR_INIT_SENT | THEIR_INIT_SENT"`)

#### RPC: CCH order status renames

- `CchOrderStatus::Succeeded` renamed to `Success` ([#1211])
- `CchOrderStatus::OutgoingSucceeded` renamed to `OutgoingSuccess` ([#1211])

#### RPC: Error behavior changes

Several RPC methods now wait for operations to complete and return proper errors instead of silently returning success. ([#1202])

- `connect_peer`, `disconnect_peer`: now return errors on failure
- `commitment_signed`, `check_channel_shutdown` (dev RPCs): now return errors on failure

#### Store: Data format changes requiring migration

- Channel actor state (prefix 0) gains two new fields: `pending_replay_updates` and `last_was_revoke` ([#1111])
- Channel-by-pubkey index (prefix 64) re-keyed from PeerId to Pubkey ([#1154])
- Network actor state (prefix 16) restructured: `peer_pubkey_map` removed, `saved_peer_addresses` re-keyed from PeerId to Pubkey ([#1154])
- Channel open records (prefix 201) changed `peer_id` field to `pubkey` ([#1154])

Run `fnn-migrate` to apply all data migrations automatically. See the [migration guide](docs/notes/v0.8.0-migration-guide.md) for details.

### Features

- Support external wallet signing for channel funding, enabling hardware wallets, multisig setups, and browser-based signers via new `open_channel_with_external_funding` and `submit_signed_funding_tx` RPCs ([#1120])
- CCH can now run as a standalone process, connecting to a Fiber node over HTTP RPC and subscribing to store changes via WebSocket `subscribe_store_changes` endpoint ([#1165])
- Automatic funding retry with exponential backoff (up to 5 retries) for transient CKB RPC/network errors during channel funding ([#1213])
- New `fiber-cli` crate providing a command-line client for all Fiber RPC methods with colorized output and auto-generated subcommands ([#1144])
- Inbound and reconnect protections: per-peer budget for inbound connections, single-flight guard on pending channel opens, bounded deferred TLC replay, delayed channel-ready until reestablish completes ([#1200])
- New `list_payments` RPC for querying payment sessions ([#1152])
- Unified storage layer with `StorageBackend` trait abstracting over RocksDB, SQLite, and browser backends ([#1191])
- RPC documentation improvements for node identifiers and channel rebalancing ([#1150])

### Bug Fixes

- Persist `CommitDiff` to disk so commitment replay after restart is deterministic, fixing potential wrong-order or missing replay during reestablish ([#1111])
- Forward `LocalCommitmentSigned` event to watchtower so it has data needed to dispute stale remote-initiated closes ([#1151])
- CCH: dynamically compute remaining incoming time and cap outgoing route expiry to prevent HTLC timeout issues in multi-hop routes ([#1148])
- Fix gossip startup sync cursor pollution where locally-generated messages could incorrectly advance the cursor, skipping remote gossip updates ([#1227])
- Treat no-route errors as permanent in CCH outgoing payments ([#979])
- Handle overflow in BTC final TLC expiry delta calculation ([#977])
- Fix receive_btc invoice currency code from 'Fibt' to 'Fibd' ([#1146])
- Fix clearer error when key file has 0x prefix ([#1162])

### Refactoring

- Extract `fiber-types` crate for all serializable types ([#1152])
- Extract `fiber-store` crate with `StorageBackend` trait ([#1168])
- Add `fiber-json-types` crate for RPC-facing types ([#1169])
- Rename various types and CCH fields for consistency ([#1211])
- Use upstream ractor release ([#1214])
- Cleanup unused fields ([#1231])

[v0.8.0]: https://github.com/nervosnetwork/fiber/compare/v0.7.1...v0.8.0
[#979]: https://github.com/nervosnetwork/fiber/pull/979
[#977]: https://github.com/nervosnetwork/fiber/pull/977
[#1111]: https://github.com/nervosnetwork/fiber/pull/1111
[#1120]: https://github.com/nervosnetwork/fiber/pull/1120
[#1144]: https://github.com/nervosnetwork/fiber/pull/1144
[#1146]: https://github.com/nervosnetwork/fiber/pull/1146
[#1148]: https://github.com/nervosnetwork/fiber/pull/1148
[#1150]: https://github.com/nervosnetwork/fiber/pull/1150
[#1151]: https://github.com/nervosnetwork/fiber/pull/1151
[#1152]: https://github.com/nervosnetwork/fiber/pull/1152
[#1154]: https://github.com/nervosnetwork/fiber/pull/1154
[#1162]: https://github.com/nervosnetwork/fiber/pull/1162
[#1165]: https://github.com/nervosnetwork/fiber/pull/1165
[#1168]: https://github.com/nervosnetwork/fiber/pull/1168
[#1169]: https://github.com/nervosnetwork/fiber/pull/1169
[#1191]: https://github.com/nervosnetwork/fiber/pull/1191
[#1200]: https://github.com/nervosnetwork/fiber/pull/1200
[#1202]: https://github.com/nervosnetwork/fiber/pull/1202
[#1211]: https://github.com/nervosnetwork/fiber/pull/1211
[#1213]: https://github.com/nervosnetwork/fiber/pull/1213
[#1214]: https://github.com/nervosnetwork/fiber/pull/1214
[#1227]: https://github.com/nervosnetwork/fiber/pull/1227
[#1231]: https://github.com/nervosnetwork/fiber/pull/1231
