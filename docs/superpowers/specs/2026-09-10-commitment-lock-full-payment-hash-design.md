# Design: Commitment-Lock Full Payment Hash (OC-TLC-001)

Fixes: [nervosnetwork/security-reviews#134](https://github.com/nervosnetwork/security-reviews/blob/b2b60167bae1fd3e04d016c37ade6f4a75ce8d0b/repos/nervosnetwork/fiber/scans/incremental/rounds/2026-08-18-189b249a-r1/report.md) — finding F-0061 / OC-TLC-001
Status of source analysis: confirmed against `fiber-scripts/contracts/commitment-lock/src/main.rs` and `fiber` develop `189b249a`.

## Problem statement

The commitment-lock contract commits only the first 20 bytes of the TLC payment hash
on-chain (`HTLC_SCRIPT_LEN = 85`, payment hash at witness bytes `[17..37]`; preimage check
`hash(preimage)[0..20] == committed 20 bytes` in `main.rs:314-323` and `355-365`).
Any party controlling the downstream payment hash (invoice issuer, or a downstream hop
supplying the hash for a forwarded TLC) can pick a preimage `P` first and advertise
`H = hash(P)[0..20] || arbitrary_12_byte_suffix`, so an on-chain preimage unlock is
accepted even though `hash(P) != H`.

The fiber-side reconciliation then fails closed in the wrong way:

- The watchtower persists the `OnChainTlcSettlement` (exact `(channel_id, tlc_id)` key) even
  when the revealed preimage fails the full-hash check (`watchtower/actor.rs:1113-1124`).
- `resolve_onchain_tlc` classifies that exact settlement as `Unknown`
  (`fiber/onchain_tlc_reconcile.rs:140-148`).
- Fulfillment collectors require `Fulfilled`; fail-forward/timeout collectors require
  `SettledWithoutPreimage`. Neither accepts `Unknown`.
- The ordinary received-TLC expiry path excludes forwarded TLCs
  (`fiber/channel.rs:2986-2990`).

Net effect: a forwarding node whose outgoing on-chain TLC was consumed with a
prefix-only preimage can neither fulfill nor fail its incoming TLC. The previous hop
later timeout-claims the incoming TLC while the forwarding node has already lost the
outgoing value on-chain — the forwarding node is stranded.

## Scope decisions

1. Contract is upgraded in place (type-script keyed lookup: `ckb/contracts.rs` resolves
   the live cell for the fiber-script type id at every tx build, so old channel cells
   automatically execute updated contract code — no per-channel code-hash pinning).
2. **No in-place channel format migration.** A legacy channel stays legacy for its whole
   settlement chain: the contract inherits `args[0..36]` into every subsequent commitment
   cell (`main.rs:466-471`), so no mid-chain args transition is possible or implemented.
3. Feature-bit negotiation happens **only at channel open**; legacy nodes keep opening
   legacy channels, which is compatible by design.
4. The lib-level reconciliation fix is retained as the only protection available for
   legacy channels and boundary paths.

## Contract design (fiber-scripts)

### args layout

```
legacy (57 bytes):
  [0..20]  pubkey hash (blake160 of aggregate x-only key)
  [20..28] delay epoch (EpochNumberWithFraction full value, LE u64)
  [28..36] commitment number (BE u64)
  [36..56] blake160(settlement snapshot bytes)
  [56]     is_first_settlement flag (0x00 / 0x01)

v1 (58 bytes):
  [0..57]  identical to legacy
  [57]     feature flag bitmap
             bit0 = ONCHAIN_FULL_PAYMENT_HASH
             remaining bits reserved; unknown bits must be 0
```

### Dispatch rules

- `args.len() == 57` → legacy layout + legacy verification semantics (must remain
  byte-for-byte equivalent to the current contract).
- `args.len() == 58 && args[57] == 0x01` → v1 layout.
- Any other 58-byte form (`args[57] == 0x00` or unknown bits `& !0x01 != 0`) →
  `Error::ArgsLenError` (no fall-through to legacy ever; unknown future flag bits and
  the zero-mask value fail loudly).

The only version fork is:

- HTLC entry length: 85 (legacy) vs 97 (v1) — 97 = 1 (type) + 16 (amount) + 32 (payment
  hash) + 20 + 20 (pubkey hashes) + 8 (expiry).
- `payment_hash` field length: 20 (legacy) vs 32 (v1).
- Preimage check: `hash(preimage)[0..20] == staged 20 bytes` (legacy) vs
  `hash(preimage) == staged 32 bytes` (v1), for both blake2b and sha256 hash algorithms.

Everything else (revocation path, since checks incl. `is_first_settlement` gating,
unlock-type dispatch, party settlement, auth calls, output-args derivation) is shared,
single-copy logic. `HTLC_SCRIPT_LEN`/`PAYMENT_HASH_LEN` become runtime consts selected by
the args feature bit; `Htlc` accessors are parameterized by a layout descriptor so the
two versions never duplicate logic blocks.

### What v1 does NOT change

- Witness unlock entries (`2 + 65 [+32]`), party-settlement section (72 bytes),
  `EMPTY_WITNESS_ARGS` prefix, since semantics, auth scheme.
- No witness-level version field: the witness layout is derived from the args version;
  the `args[36..56]` snapshot-hash check fails on any layout mismatch, so a second
  in-witness version source would be redundant surface, not defense.

## fiber-lib design (feature-bit compatibility only)

### Negotiation and persistence

- New FeatureBit in `fiber-types/protocol.rs` (channel features), name:
  `ONCHAIN_FULL_PAYMENT_HASH`.
- At channel open both sides negotiate the bit; unsupported peers → channel opens
  legacy (existing behavior, no special-casing beyond "bit absent").
- Persist the negotiated result as a per-channel `commitment_contract_version`
  (enum: `Legacy` / `V1`) in channel state; watchtower tracking snapshots carry the same
  value. Restart-derived behavior must always come from this stored value, never from
  "what the current binary/contract supports".

### Serialization points (dual format)

- `settlement_tlc_to_witness` (`fiber/channel.rs:4711-4728`): v1 writes the full 32-byte
  payment hash; legacy keeps `[0..20]`. Writes are driven by the stored channel version.
- Settlement snapshot builders feeding args construction
  (`settlement_data_to_witness`, commitment args construction at `channel.rs:9760-9777`):
  push the `[57]` feature byte exactly when v1 is negotiated; legacy args stay 57 bytes.
- Watchtower witness parsing (`watchtower/actor.rs:2221-2253`, `Htlc` at `2089-2166`):
  parse HTLC entries with the stored per-channel layout; `payment_hash` becomes
  full-length for v1; `find_matched_private_key` matches by full equality for v1 and by
  20-byte prefix for legacy.

### Reconciliation fix (strikes the stranding bug for legacy/boundary paths)

- Add `OnChainTlcResolution::SettledWithInvalidPreimage` for the case "exact settlement
  record exists, preimage matches the committed prefix but not the full hash".
- `collect_onchain_fulfilled_tlcs` / `onchain_fulfilled_preimage`: unchanged (still only
  `Fulfilled`).
- Timeout/fail-forward collectors (`collect_onchain_timeout_settled_tlcs`,
  `collect_onchain_received_timeout_settled_tlcs`): accept `SettledWithoutPreimage |
  SettledWithInvalidPreimage`, reusing the existing Forwarded / OriginPayer handling.
  The offered TLC is provably consumed on-chain and the incoming TLC provably cannot be
  fulfilled with this preimage, so failing forward is the only consistent disposition.
- The watchtower keeps persisting the exact settlement record with the mismatching
  preimage (forensics), but logs `error!` (active-theft signal), and must NOT
  `insert_watch_preimage` for a full-hash mismatch (current behavior already correct).
- Note: `collect_onchain_timeout_settled_tlcs` currently additionally filters
  `tlc.expiry < expect_expiry`; the invalid-preimage disposition must be collected
  regardless of expiry (the output is already consumed when the settlement is observed).

### Fee/size estimation

- v1 settlement witnesses grow 12 bytes per pending HTLC (97 vs 85). Verify
  `estimate_tx_size` / `checked_calculate_commitment_tx_fee` include witness bytes; fix
  if not.

## Testing / regression checklist

fiber (fnn):

1. `resolve_onchain_tlc` for an exact settlement whose preimage passes the committed
   20-byte prefix but fails the full 32-byte hash → resolves
   `SettledWithInvalidPreimage` (replacement for the current
   `resolve_returns_unknown_when_preimage_mismatches` semantics).
2. Collectors route `SettledWithInvalidPreimage` into the existing
   Forwarded (fail upstream) / OriginPayer (final payment failure) paths, independent of
   TLC expiry.
3. Watchtower stores the settlement record and emits the `error!` diagnostic on a
   full-hash mismatch; never inserts the preimage into the watch-preimage store.
4. Witness round-trip tests for both layouts (85/97 byte HTLC, `find_matched_private_key`
   matching rules prefix vs full).
5. Feature negotiation: open channel with bit both sides → persisted `V1`; open against
   legacy peer → channel opens legacy; restart preserves stored version.

fiber-scripts contract tests:

6. Legacy args (57) must pass the exact current test suite unchanged (byte-for-byte
   equivalence).
7. v1 args: preimage matching full hash succeeds; preimage matching only the 20-byte
   prefix fails with `PreimageError` (the original attack must fail on-chain).
8. Unknown flag bits (`args[57]` with any of bits 1..7 set) → `ArgsLenError`.

Adversarial guards:

- `args.len() == 58` with version 0 (or unknown values) must never be treated as legacy.
- The commitment args builder must never emit a 58-byte args snapshot hash for a
  legacy-format pending-HTLC list (fail fast in fiber off-chain code as well).

## Residual risk (explicitly accepted)

- Every pre-upgrade channel keeps the 20-byte prefix commitment on-chain forever; the
  on-chain attack surface remains for those channels. The lib-level fail-forward fix
  removes only the stranding amplification, not the on-chain loss.
- Ending usage requires either all legacy channels to settle + coop close (workflows
  can encourage this over time) or a future contract feature to support in-place args
  transitions (out of scope by decision).
