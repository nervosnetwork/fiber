# Commitment-Lock Full Payment Hash Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix OC-TLC-001 by adding a feature-bit-flagged v1 settlement layout (full 32-byte payment hash on-chain) to the commitment-lock contract, keeping the legacy layout byte-for-byte compatible, and making the fiber node negotiate/per-parse both layouts.

**Architecture:** `args[57]` becomes a feature-flag bitmap (58-byte args). Bit0 = `ONCHAIN_FULL_PAYMENT_HASH`. The contract shares all logic between layouts; only the HTLC entry length (85→97) and the preimage comparison width (20→32) fork. The fiber node negotiates the feature at channel open via node-level feature bits (no new wire messages), persists a per-channel version, and serializes/parses witnesses per that version. Legacy channels stay legacy forever; a lib-level reconciliation fix removes the stranding amplifier for them.

**Tech Stack:** Rust 1.93, ckb-std / ckb-testtool (fiber-scripts), ractor actors (fiber-lib), cargo-nextest, nextest config in `.config/nextest.toml`.

**Spec:** `docs/superpowers/specs/2026-09-10-commitment-lock-full-payment-hash-design.md`

## Global Constraints

- Order: **all fiber-scripts tasks (1–3) complete and green before any fiber node task (4+).**
- fiber `ChannelActorData` is persisted data: any new field MUST use `#[serde(default)]`; run `make check-migrate` after changing persisted structs and `make update-migrate-check` if it complains.
- Repo gate commands (all repo work): `cargo fmt --all` then `make clippy` then `cargo nextest run --no-fail-fast`; also `typos` and `cargo shear` before final commit. No clippy warnings (`-D warnings`).
- fiber-scripts commands run from `/home/quake/workspace/fiber-scripts`; fiber commands from `/home/quake/workspace/fiber`.
- fiber-scripts test build contract first: `make build` (full) or `make build CONTRACT=commitment-lock` (single); then `make test` (which is `cargo test`). Contract tests need real contract binaries in `build/release/`.
- Legacy path in the contract must remain **byte-for-byte equivalent**: 57-byte args path must pass the pre-existing integration tests unchanged.
- WALM: never let `args.len() == 58` fall through to `Ok`-legacy semantics; dispatch is a whitelist: 57 → legacy, 58 + `args[57] == 0x01` → v1, else `Error::ArgsLenError`.
- Do NOT add comments unless a code block already has them (match surrounding style); AGENTS.md governs naming (`SCREAMING_SNAKE_CASE` consts, snake_case fns, thiserror errors).
- Never commit secrets; commit messages imperative, lowercase prefix (`feat:`, `fix:`, `test:`, `chore:`).

---

### Task 1: Contract — version dispatch + shared HTLC layout parameterization

**Files:**
- Modify: `/home/quake/workspace/fiber-scripts/contracts/commitment-lock/src/main.rs` (constants at ~L82-85; `Htlc` impl at ~L97-135; `auth()` entry checks at ~L170-175; settlement loop at ~L233-301)
- Test: `/home/quake/workspace/fiber-scripts/tests/src/tests.rs` (add tests; existing 4 tests at ~L116 `test_funding_lock`, ~L203, ~L438, ~L1066 flows)

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `struct HtlcLayout { htlc_script_len: usize, payment_hash_len: usize }`, `fn resolve_htlc_layout(args: &[u8]) -> Result<HtlcLayout, Error>`, `fn preimage_matches(hash_type: PaymentHashType, committed_hash: &[u8], preimage: &[u8]) -> bool`. Later tasks and existing tests rely on: legacy args (57 bytes) behaving identically to today; 58-byte args with `args[57] == 0x01` accepted; anything else rejected with `Error::ArgsLenError`.

- [ ] **Step 1: Build the current contracts and run the current integration tests to establish the baseline**

```bash
cd /home/quake/workspace/fiber-scripts
make build CONTRACT=commitment-lock MODE=release
make test CARGO_ARGS="-- --nocapture"
```

Expected: build succeeds; all existing tests in `tests/src/tests.rs` pass. Record results before touching anything.

- [ ] **Step 2: Write the failing tests**

Add to `/home/quake/workspace/fiber-scripts/tests/src/tests.rs` two tests based on the existing settlement-with-preimage test (the test fn around line 438 that deploys `commitment-lock` and unlocks an offered/received HTLC with a preimage). Copy that test's full body — do not import helpers beyond what the file already defines (`generate_multisig_keys`, `multisig`, `EMPTY_WITNESS_ARGS`, the `Loader`).

The two changes relative to the copied v0 test:

```rust
// v1: args gain one byte (57 -> 58), bit0 set:
let mut v1_flag_args = base_lock_args.clone(); // 57 bytes, built exactly like the v0 test
v1_flag_args.push(0x01); // [57] = feature bitmap, bit0 = ONCHAIN_FULL_PAYMENT_HASH

// v1: each pending HTLC entry gains the extra 12 bytes of payment hash:
fn build_htlc_entry_v1(
    htlc_type: u8,           // same bit layout as v0: bit0 offered/received, bit1 hash algo
    payment_amount: u128,
    payment_hash: Hash256,   // FULL 32-byte payment hash now committed on-chain
    remote_key_hash: [u8; 20],
    local_key_hash: [u8; 20],
    expiry_since: u64,
) -> Vec<u8> {
    let mut vec = Vec::new();
    vec.push(htlc_type);
    vec.extend_from_slice(&payment_amount.to_le_bytes());
    vec.extend_from_slice(payment_hash.as_ref()); // 32 bytes, no truncation
    vec.extend_from_slice(&remote_key_hash);
    vec.extend_from_slice(&local_key_hash);
    vec.extend_from_slice(&expiry_since.to_le_bytes());
    vec // must be 97 bytes
}
```

Test A `v1_settlement_with_preimage_unlock_succeeds`: full flow of the copied test but with the 58-byte args (`0x01` flag), `build_htlc_entry_v1` witness entries (97 bytes), full-hash `payment_hash = hash(preimage)` — assert `context.verify_script` succeeds.

Test B `v1_settlement_rejects_prefix_only_preimage` (the regression for the original attack): same as Test A but `payment_hash = hash(preimage)[0..20] || random_suffix(12)` — the on-chain committed 20-byte prefix STILL matches (so the legacy contract would accept), but the v1 contract must reject. Assert `verify_script` returns an error with code `Error::PreimageError as i8` (21).

Test C `v1_args_with_unknown_flag_bits_rejected`: same flow as Test A but `args[57] = 0x03` (bit1 unknown) — assert `Error::ArgsLenError as i8` (15 expected per enum ordering: IndexOutOfBound=1, ItemMissing=2, LengthNotEnough=3, Encoding=4, MultipleInputs=5, InvalidSince=6, InvalidUnlockType=7, InvalidWithPreimageFlag=8, InvalidSettlementCount=9, InvalidUnlockCount=10, InvalidExpiry=11, ArgsLenError=12, WitnessLenError=13, EmptyWitnessArgsError=14, WitnessHashError=15, ... → verify against the actual enum in main.rs and adjust the literal).

Test D `v1_zero_mask_rejected`: `args[57] = 0x00` → `Error::ArgsLenError`.

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cd /home/quake/workspace/fiber-scripts
make build CONTRACT=commitment-lock MODE=release
make test
```

Expected: Tests A–D FAIL. Tests A/B/D fail because the v0 contract rejects every 58-byte args with `ArgsLenError` while the tests expect those states; Test B fails because the tests expect the attack to fail but the code rejects for the wrong reason — acceptable at this stage; document observed results.

- [ ] **Step 4: Implement the version dispatch**

Replace the fixed constants and `Htlc` parsing in `contracts/commitment-lock/src/main.rs`:

```rust
// delete the two consts currently existing:
// const HTLC_SCRIPT_LEN: usize = 85;
// replace with a per-version layout
struct HtlcLayout {
    htlc_script_len: usize,
    payment_hash_len: usize,
}

const HTLC_LAYOUT_LEGACY: HtlcLayout = HtlcLayout {
    htlc_script_len: 85,
    payment_hash_len: 20,
};

const HTLC_LAYOUT_V1: HtlcLayout = HtlcLayout {
    htlc_script_len: 97, // 1 (htlc_type) + 16 (payment_amount) + 32 (payment_hash)
                         // + 20 (remote_htlc_pubkey_hash) + 20 (local_htlc_pubkey_hash) + 8 (htlc_expiry)
    payment_hash_len: 32,
};

// feature bitmap bit0: full 32-byte payment hash committed on-chain
const FEATURE_ONCHAIN_FULL_PAYMENT_HASH: u8 = 0b0000_0001;

fn resolve_htlc_layout(args: &[u8]) -> Result<HtlcLayout, Error> {
    match args.len() {
        57 => Ok(HTLC_LAYOUT_LEGACY),
        58 if args[57] == FEATURE_ONCHAIN_FULL_PAYMENT_HASH => Ok(HTLC_LAYOUT_V1),
        _ => Err(Error::ArgsLenError),
    }
}

fn preimage_matches(
    hash_type: PaymentHashType,
    committed_hash: &[u8], // len 20 -> legacy prefix; len 32 -> v1 full
    preimage: &[u8],
) -> bool {
    let digest = match hash_type {
        PaymentHashType::Blake2b => blake2b_256(preimage),
        PaymentHashType::Sha256 => Sha256::digest(preimage).into(),
    };
    committed_hash == &digest[..committed_hash.len()]
}
```

Change `Htlc<'a>` accessors to carry `payment_hash_len`:

```rust
struct Htlc<'a> {
    data: &'a [u8],
    payment_hash_len: usize,
}

impl<'a> Htlc<'a> {
    fn new(data: &'a [u8], layout: HtlcLayout) -> Self {
        Self { data, payment_hash_len: layout.payment_hash_len }
    }
    fn htlc_type(&self) -> HtlcType {
        if self.data[0] & 0b00000001 == 0 { HtlcType::Offered } else { HtlcType::Received }
    }
    fn payment_hash_type(&self) -> PaymentHashType {
        if (self.data[0] >> 1) & 0b0000001 == 0 { PaymentHashType::Blake2b } else { PaymentHashType::Sha256 }
    }
    fn payment_amount(&self) -> u128 {
        u128::from_le_bytes(self.data[1..17].try_into().unwrap())
    }
    fn payment_hash(&self) -> &'a [u8] {
        &self.data[17..17 + self.payment_hash_len]
    }
    fn remote_htlc_pubkey_hash(&self) -> [u8; 20] {
        let off = 17 + self.payment_hash_len;
        self.data[off..off + 20].try_into().unwrap()
    }
    fn local_htlc_pubkey_hash(&self) -> [u8; 20] {
        let off = 17 + self.payment_hash_len + 20;
        self.data[off..off + 20].try_into().unwrap()
    }
    fn htlc_expiry(&self) -> u64 {
        let off = 17 + self.payment_hash_len + 40;
        u64::from_le_bytes(self.data[off..off + 8].try_into().unwrap())
    }
}
```

In `auth()`:

```rust
// after the existing `if args.len() != 57 { return Err(Error::ArgsLenError); }`
// REMOVE that check entirely and replace with:
let layout = resolve_htlc_layout(args.as_ref())?;
```

Then thread `layout` where needed:

1. `settlement_script_len = pending_htlcs_len + 72` uses `layout.htlc_script_len` instead of the const:

```rust
let pending_htlcs_len = 1 + (pending_htlc_count as usize) * layout.htlc_script_len;
let settlement_script_len = pending_htlcs_len + 72;
if witness.len() < settlement_script_len {
    return Err(Error::WitnessLenError);
}
```

2. The loop `while witness.len() > i { ... }` slice bounds become `i + layout.htlc_script_len` for the HTLC header, and the settle offset offsets `pending_htlcs_len + 20` etc. stay unchanged (party settlement section is layout-independent); the loop over `witness[1..pending_htlcs_len].chunks(layout.htlc_script_len)` replaces the old const chunk.

3. Each `Settlement` htlc-parse site builds `let htlc = Htlc::new(htlc_script, layout);` and replaces the two duplicated preimage checks with `preimage_matches(htlc.payment_hash_type(), htlc.payment_hash(), preimage)`:

```rust
let preimage = settlements[0].preimage();
if !preimage_matches(htlc.payment_hash_type(), htlc.payment_hash(), preimage) {
    return Err(Error::PreimageError);
}
```

Pre-image unwraps at both offered (`main.rs:313`) and received (`main.rs:355`) go through the same helper; ALSO keep `is_first_settlement` gating lines as-is (`args[56]`) — no change there.

- [ ] **Step 5: Verify tests pass, then commit**

```bash
cd /home/quake/workspace/fiber-scripts
make build CONTRACT=commitment-lock MODE=release
make test
```

Expected: baseline v0 tests still pass (unlock semantics unchanged for 57-byte args); Tests A, B, C, D from Step 2 PASS.

```bash
cd /home/quake/workspace/fiber-scripts
git add contracts/commitment-lock/src/main.rs tests/src/tests.rs
git commit -m "feat(commitment-lock): add v1 settlement layout with full 32-byte payment hash"
```

---

### Task 2: Contract — adversarial guards and legacy regression sweep

**Files:**
- Test: `/home/quake/workspace/fiber-scripts/tests/src/tests.rs`
- Modify: `/home/quake/workspace/fiber-scripts/contracts/commitment-lock/src/main.rs` (only if the sweep finds a gap)

**Interfaces:**
- Consumes: `resolve_htlc_layout` (Task 1).
- Produces: hard guarantees that a legacy-spending attacker cannot smuggle a v1 witness entry past the legacy path and vice versa: 77-byte (legacy 20-byte hash) entries inside a 97-byte worker are impossible from the outside because dispatch is purely by `args.len()`.

- [ ] **Step 1: Cross-format mismatch tests**

Add tests where the args and the witness format disagree:

Test E `v1_settlement_rejects_legacy_htlc_entries`: args with `[57] = 0x01` (v1) but witness pending HTLC entries are 85 bytes (legacy layout) — the witness snapshot hash committed in `args[36..56]` IS the hash of these legacy bytes (i.e., the commitment side also used legacy). Build this by taking the baseline v0 test and only changing `[57]` byte on the args (which the fingerprint hash `args[36..56]` does NOT cover). Expected FAIL constraint: the contract must reject the mismatch. The rejection mechanism is the `witness[0..pending_htlcs_len]` total length check: the witness for `pending_htlc_count = N` must be exactly `1 + N * layout.htlc_script_len + 72 + unlocks` — when the args claim 97-byte entries but witness entries are 85 bytes, the parser gets misaligned shapes. Sharpen this: the cleanest misalignment is caught by the lock script's `blake2b_256(witness[0..settlement_script_len])[0..20] != args[36..56]` — computing `pending_htlcs_len` with 97 bytes and committed hash over 85-byte entries yields a visible hash mismatch → `Error::WitnessHashError`. Assert that error code precisely.

Test F `legacy_settlement_rejects_v1_htlc_entries`: args (57-byte legacy) with a witness whose entries are 97 bytes each — the snapshot hash committed in `args[36..56]` covers the legacy-summed (85) version — assert `Error::WitnessHashError`.

- [ ] **Step 2: Run them, verify both fail/are rejected for identifiable errors, fix main.rs only if a witness-hashing/parse ambiguity leaks through**

```bash
cd /home/quake/workspace/fiber-scripts && make build CONTRACT=commitment-lock && make test
```

Expected: both tests PASS (i.e., the contract rejects each mismatch in the specific way asserted). If either leaks through differently, the fix belongs in `resolve_htlc_layout` usage points and must remain in the whitelist semantics; adjust the test to match the correct rejection path, never loosen it.

- [ ] **Step 3: Full legacy-regression sweep + quality gates + commit**

```bash
cd /home/quake/workspace/fiber-scripts
make build
make test
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

Expected: everything green.

```bash
cd /home/quake/workspace/fiber-scripts
git add -A
git commit -m "test(commitment-lock): cross-format mismatch guards and legacy sweep"
```

**STOP — Part 1 (fiber-scripts) must be fully green and committed before starting Task 3.**

---

### Task 3: fiber-types — feature bit + per-channel contract version

**Files:**
- Modify: `/home/quake/workspace/fiber/crates/fiber-types/src/protocol.rs:242-247` (feature block)
- Modify: `/home/quake/workspace/fiber/crates/fiber-types/src/channel.rs:1480` (`ChannelActorData` — add version field)
- Test: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/features.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `ONCHAIN_FULL_PAYMENT_HASH_OPTIONAL: u16 = 7` / `ONCHAIN_FULL_PAYMENT_HASH_REQUIRED: u16 = 6`, `FeatureVector::supports_onchain_full_payment_hash()`, `ChannelActorData.commitment_contract_version: CommitmentContractVersion` (enum `Legacy`/`V1`, `Default` = `Legacy`), and `CommitmentContractVersion::for_negotiated(ours: bool, peer: Option<bool>) -> CommitmentContractVersion` used by Task 4.

- [ ] **Step 1: Write the failing test**

In `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/features.rs` follow the style of the existing tests (e.g. `features.rs:16-20`):

```rust
#[test]
fn test_onchain_full_payment_hash_feature_bit() {
    let mut vector = FeatureVector::default();
    assert!(!vector.supports_onchain_full_payment_hash());
    vector.set_onchain_full_payment_hash_optional();
    assert!(vector.supports_onchain_full_payment_hash());
    assert!(!vector.requires_onchain_full_payment_hash());
    vector.set_onchain_full_payment_hash_required();
    assert!(vector.requires_onchain_full_payment_hash());
    // backward compat: the pre-existing features still work
    assert!(vector.supports_feature(GOSSIP_QUERIES_OPTIONAL) == false);
    vector.set_gossip_queries_optional();
    assert!(vector.supports_feature(GOSSIP_QUERIES_OPTIONAL));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo nextest run -p fnn test_onchain_full_payment_hash_feature_bit
```

Expected: compile FAIL — no `ONCHAIN_FULL_PAYMENT_HASH_*` consts.

- [ ] **Step 3: Register the feature and the version type**

In `/home/quake/workspace/fiber/crates/fiber-types/src/protocol.rs:242-247`:

```rust
    declare_feature_bits_and_methods! {
        GOSSIP_QUERIES, 1;
        BASIC_MPP, 3;
        TRAMPOLINE_ROUTING, 5;
        // more features, please note that base bit must be defined as increasing odd numbers
        ONCHAIN_FULL_PAYMENT_HASH, 7;
    }
```

In `/home/quake/workspace/fiber/crates/fiber-types/src/channel.rs`, add the enum near the other `ChannelActorData` field types (file top-level):

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Copy)]
#[serde(rename = "snake_case")]
pub enum CommitmentContractVersion {
    /// 20-byte prefix commitment (57-byte args, pre-upgrade channels).
    Legacy,
    /// 32-byte payment hash committed on-chain, 97-byte HTLC witness, 58-byte args.
    V1,
}

impl Default for CommitmentContractVersion {
    fn default() -> Self {
        Self::Legacy
    }
}

impl CommitmentContractVersion {
    /// V1 only when BOTH peers advertise support of the feature bit.
    pub fn for_negotiated(ours_supports: bool, peer_supports: Option<bool>) -> Self {
        match ours_supports && peer_supports == Some(true) {
            true => Self::V1,
            false => Self::Legacy,
        }
    }
}
```

Into `ChannelActorData` (channel.rs), add persist-safe field:

```rust
    /// Which commitment-lock settlement witness layout this channel uses,
    /// decided once at channel-open and never changed.
    #[serde(default)]
    pub commitment_contract_version: CommitmentContractVersion,
```

- [ ] **Step 4: Run tests to green, then run migration-schema check and commit**

```bash
cargo nextest run -p fnn test_onchain_full_payment_hash_feature_bit
make check-migrate
```

Expected: test PASSES. `check-migrate` — if it reports the new field needs a migration schema entry, run `make update-migrate-check` and review the generated diff (`ChannelActorData` gains a default).

```bash
cargo fmt --all
git add crates/fiber-types
git commit -m "feat(types): add ONCHAIN_FULL_PAYMENT_HASH feature bit and per-channel commitment contract version"
```

---

### Task 4: fiber-lib — decide + persist the channel version; dual-format witness builder

**Files:**
- Modify: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/channel.rs` — `settlement_tlc_to_witness` at 4711-4728 and `settlement_data_to_witness` at 4680-4700, args builder in `fn build_commitment_transaction_output` (`channel.rs:9752-9777`), plus wherever `ChannelActorData` is constructed from `OpenChannel`/`AcceptChannel` negotiate (search `ChannelActorData::new(`)
- Test: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/channel.rs`

**Interfaces:**
- Consumes: Task 3's `CommitmentContractVersion`, `FeatureVector::supports_onchain_full_payment_hash()`.
- Produces: `settlement_tlc_to_witness(tlc, for_remote, version)` writes `payment_hash` `0..20` for Legacy and full 32 bytes for `V1`; new helper `fn contract_feature_flags(version: CommitmentContractVersion) -> u8` (bit0 set iff `V1`) used in the args builder at 9760-9777. Full function signatures (visible to Task 5).

- [ ] **Step 1: Version decision at channel establishment**

Find the constructor site: `grep -rn "ChannelActorData::new\|ChannelActorData {" crates/fiber-lib/src/fiber/channel.rs crates/fiber-lib/src/fiber/network.rs`. At that point both parties' negotiated feature vectors are in scope — the initiator's own features from `self.features`/config and the peer's from `peer_session_map[pubkey].features` (network.rs:5844 already extracts it in `check_feature_compatibility`). At the `OpenChannel` init path in network.rs (`network.rs:5471-5554`) and the mirrored accept path, the node sets:

```rust
// peer_features is the same Option<&FeatureVector> extraction that
// check_feature_compatibility (network.rs:5844) uses; `None` -> Legacy.
let commitment_contract_version = CommitmentContractVersion::for_negotiated(
    self.features.supports_onchain_full_payment_hash(),
    peer_features.map(|f| f.supports_onchain_full_payment_hash()),
);
```

(Wire both the OpenChannel and the AcceptChannel constructor sites with
`CommitmentContractVersion::for_negotiated(ours, peer_support)`; do not hardcode
`Legacy`. Also set the bit optional in the node's default announcement feature vector:
`config.rs:610-614 gen_node_features` gains `let mut fv = FeatureVector::default();
fv.set_onchain_full_payment_hash_optional(); fv`.)

- [ ] **Step 2: Write failing tests for dual-format witness bytes**

In `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/channel.rs` add (importing from `fiber::channel::settlement_tlc_to_witness` as available at `channel.rs:4711`):

```rust
#[test]
fn settlement_tlc_to_witness_matches_commitment_contract_layout() {
    let tlc = gen_utils::generate_test_settlement_tlc(); // reuse the existing SettlementTlc factory used by other witness tests (search `settlement_tlc_to_witness` in fiber-tests)
    let legacy = super::settlement_tlc_to_witness(&tlc, false, CommitmentContractVersion::Legacy);
    assert_eq!(legacy.len(), 85); // htlc_type(1) + amount(16) + hash(20) + keys(40) + expiry(8)

    let v1 = super::settlement_tlc_to_witness(&tlc, false, CommitmentContractVersion::V1);
    assert_eq!(v1.len(), 97);
    // the first 17 bytes are identical; the hash field is the difference
    assert_eq!(&legacy[0..17], &v1[0..17]);
    assert_eq!(&v1[17..49], tlc.payment_hash.as_ref());
    assert_eq!(&legacy[17..37], &tlc.payment_hash.as_ref()[0..20]);
    // keys and expiry match after the different-length hash field (49 vs 37 offset)
    assert_eq!(&legacy[37..], &v1[49..]);
    // byte 0 (htlc_type) equal, both for for_remote = true and false
    assert_eq!(
        super::settlement_tlc_to_witness(&tlc, true, CommitmentContractVersion::V1)[1..17],
        tlc.payment_amount.to_le_bytes()
    );
}
```

(If `generate_test_settlement_tlc` doesn't exist as written, reuse whatever the current tests use to construct a `SettlementTlc` — search `settlement_tlc_to_witness` call sites in `fiber-lib/src/fiber/tests/`.)

- [ ] **Step 3: Threading the version through the builder**

Update the builder signatures (free function at `channel.rs:4711` and its only callers — the loop in `settlement_data_to_witness` at `channel.rs:4685`):

```rust
pub fn settlement_tlc_to_witness(
    tlc: &SettlementTlc,
    for_remote: bool,
    commitment_contract_version: CommitmentContractVersion,
) -> Vec<u8> {
    let payment_hash_len = match commitment_contract_version {
        CommitmentContractVersion::Legacy => 20,
        CommitmentContractVersion::V1 => 32,
    };
    let mut vec = Vec::new();
    let offered_flag = if tlc.tlc_id.is_offered() { 0u8 } else { 1u8 };
    vec.push(((tlc.hash_algorithm as u8) << 1) + offered_flag);
    vec.extend_from_slice(&tlc.payment_amount.to_le_bytes());
    vec.extend_from_slice(&tlc.payment_hash.as_ref()[..payment_hash_len]);
    if for_remote {
        vec.extend_from_slice(blake160(&tlc.remote_key.serialize()).as_ref());
        vec.extend_from_slice(blake160(&tlc.local_key.pubkey().serialize()).as_ref());
    } else {
        vec.extend_from_slice(blake160(&tlc.local_key.pubkey().serialize()).as_ref());
        vec.extend_from_slice(blake160(&tlc.remote_key.serialize()).as_ref());
    }
    let since = Since::new(SinceType::Timestamp, tlc.expiry / 1000, false);
    vec.extend_from_slice(&since.value().to_le_bytes());
    vec
}
```

`settlement_data_to_witness` gains the same parameter and threads it into every `settlement_tlc_to_witness` / `ToWitness()` element. Callers updated accordingly (channel.rs and watchtower test helpers all must pass a version; for now pass `CommitmentContractVersion::Legacy` everywhere except the channel-open decision site).

The builder for the contract args (channel.rs:9760-9777) appends the feature byte only for V1:

```rust
        let mut commitment_lock_script_args = [
            &blake2b_256(x_only_aggregated_pubkey)[0..20],
            self.get_delay_epoch_as_lock_args_bytes().as_slice(),
            version.to_be_bytes().as_slice(),
        ]
        .concat();

        let (local_settlement_key, remote_settlement_key) = self.get_settlement_keys();
        commitment_lock_script_args.extend_from_slice(
            blake160(&settlement_data_to_witness(
                &settlement_data,
                for_remote,
                self.commitment_contract_version, // NEW: must reach the HTLC builder
                local_settlement_key,
                remote_settlement_key,
            ))
            .as_ref(),
        );
        commitment_lock_script_args.push(0x00);
        if self.commitment_contract_version == CommitmentContractVersion::V1 {
            commitment_lock_script_args.push(0x01); // args[57] feature bitmap, bit0
        }
```

Every other `commitment_lock_script_args` push sites (`channel.rs:6659`, `channel.rs:8652`) get the same conditional push; update the mirror acceptance path in network.rs if it builds args too (search `is_first_settlement` / `push(0x00)` for all sites).

- [ ] **Step 4: Run nextest, commit**

```bash
cargo nextest run -p fnn -p fiber-bin --no-fail-fast
cargo fmt --all && make clippy
```

Expected: green.

```bash
git add crates/fiber-lib crates/fiber-types
git commit -m "feat(channel): negotiate and persist per-channel commitment contract version; dual-format witness builder"
```

---

### Task 5: fiber-lib — watchtower dual-format parsing

**Files:**
- Modify: `/home/quake/workspace/fiber/crates/fiber-lib/src/watchtower/actor.rs` — `struct Htlc` (2089-2166), `SettlementWitness::build_from_witness` (2221-2253), all `load_binary`-companion build sites for `witness_tlc.to_witness()` comparisons and the harness that stores tracked witness bytes around 1030-1042 and 1216-1250 (tracked witness bytes must be built with the SAME version as the observed tx)
- Test: `/home/quake/workspace/fiber/crates/fiber-lib/src/watchtower/tests.rs` (or wherever `SettlementWitness::build_from_witness` tests live — search `build_from_witness` usages: watchtower/actor.rs:2671, 1556)

**Interfaces:**
- Consumes: Task 4's `ChannelActorData::commitment_contract_version` for tracked channel snapshot.
- Produces: `struct Htlc` gaining `payment_hash: Vec<u8>` (length 20 or 32 depending on layout) instead of `[u8; 20]`; `SettlementWitness::build_from_witness(witness_bytes, layout: HtlcLayout)`; `HtlcLayout` reexported or duplicated param-side as `fn watchtower_layout(version: CommitmentContractVersion) -> (usize, usize, usize, usize)` tuple `(htlc_script_len, payment_hash_len, unlock_no_preimage_len, unlock_with_preimage_len) = (85, 20, 67, 99)` / `(97, 32, 67, 99)`.

- [ ] **Step 1: Write failing test**

Create in the watchtower test module (mirror the pattern at `watchtower/actor.rs:2655-2690` where a settlement witness is built and parsed):

```rust
#[test]
fn settlement_witness_parses_both_layouts() {
    // legacy 85-byte HTLC
    let mut legacy = Vec::new();
    legacy.push(0u8); // pending count placeholder before outer calls; keep exact builder ordering as tests use them now
    // ... use the existing test helper `settlement_witness_builder` (used at actor.rs:2536/2565) but add the
    // CommitmentContractVersion argument; for now verify sizes:
    // witness_len for no-preimage unlock == 67, with-preimage == 99 (unchanged by version)
    // full payment-hash parsing: for V1 the parsed htlc.payment_hash has len 32 == full Hash256
}
```

Concretely write a test that drives `SettlementWitness::build_from_witness` with a hand-serialized witness using the 97-byte layout and asserts `pending_htlcs[0].payment_hash == payment_hash_full`, and its `witness_len()` == 99 for `with_preimage` — asserting the lengths that the V2 test's assumptions must hold.

- [ ] **Step 2: Parsing driven by stored channel version**

`SettlementWitness::build_from_witness` currently hardcodes `reader.take(85)` at actor.rs:2229 and `pending_htlcs.push(Htlc::build_from_witness(reader.take(85)?))`. Thread the version from the tracked channel state:

```rust
/// Returns the HTLC script length for the commitment=settlement layout of the channel.
/// The tracked channel snapshot fix by task 4 is read from the ActorState via the
/// store at the point the watchtower is registering the commitment; the flagged
/// version decides the on-wire HTLC length.
pub fn build_from_witness(
    witness: &[u8],
    commitment_contract_version: CommitmentContractVersion,
) -> Option<Self> {
    let (htlc_script_len, payment_hash_len): (usize, usize) = match commitment_contract_version {
        CommitmentContractVersion::Legacy => (85, 20),
        CommitmentContractVersion::V1 => (97, 32),
    };
    let mut reader = WitnessReader::new(witness);
    let _unlock_count = reader.take_u8()?;
    let pending_htlc_count = reader.take_u8()? as usize;

    let mut pending_htlcs = Vec::with_capacity(pending_htlc_count);
    for _ in 0..pending_htlc_count {
        pending_htlcs.push(Htlc::build_from_witness(
            reader.take(htlc_script_len)?,
            payment_hash_len,
        ));
    }
    // ... rest unchanged
}

struct Htlc {
    htlc_type: u8,
    payment_amount: u128,
    payment_hash: Vec<u8>,
    remote_htlc_pubkey_hash: [u8; 20],
    local_htlc_pubkey_hash: [u8; 20],
    htlc_expiry: u64,
}

impl Htlc {
    pub fn build_from_witness(witness: &[u8], payment_hash_len: usize) -> Self {
        let htlc_type = witness[0];
        let payment_amount = u128::from_le_bytes(witness[1..17].try_into().unwrap());
        let payment_hash = witness[17..17 + payment_hash_len].to_vec();
        let remote_htlc_pubkey_hash = witness[17 + payment_hash_len..37 + payment_hash_len]
            .try_into()
            .unwrap();
        let local_htlc_pubkey_hash = witness[37 + payment_hash_len..57 + payment_hash_len]
            .try_into()
            .unwrap();
        let htlc_expiry = u64::from_le_bytes(witness[57 + payment_hash_len..65 + payment_hash_len]
            .try_into()
            .unwrap());
        Self { htlc_type, payment_amount, payment_hash, remote_htlc_pubkey_hash, local_htlc_pubkey_hash, htlc_expiry }
    }
    // to_witness() gains no flag — writes self.payment_hash (now len 20 or 32 faithfully)
    pub fn to_witness(&self) -> Vec<u8> {
        let mut vec = Vec::new();
        vec.push(self.htlc_type);
        vec.extend_from_slice(&self.payment_amount.to_le_bytes());
        vec.extend_from_slice(&self.payment_hash);
        vec.extend_from_slice(&self.remote_htlc_pubkey_hash);
        vec.extend_from_slice(&self.local_htlc_pubkey_hash);
        vec.extend_from_slice(&self.htlc_expiry.to_le_bytes());
        vec
    }

    pub fn find_matched_private_key<'a>(
        &self,
        settlement_data: &'a SettlementData,
        with_preimage: bool,
    ) -> Option<&'a Privkey> {
        settlement_data.tlcs.iter().find_map(|settlement_tlc| {
            // full-hash equality for V1 (len 32); prefix equality for Legacy (len 20)
            let payment_hash_matches = settlement_tlc
                .payment_hash
                .as_ref()
                .starts_with(&self.payment_hash);
            // ... rest unchanged
        })
    }
}
```

Call sites where the version is unknown at compile time must read it from the tracked channel snapshot (`watchtower/actor.rs: actors.rs TrackedSettlementTlc` structures — extend `TrackedSettlementTlc` with `commitment_contract_version: CommitmentContractVersion` so `watchtower::actors.rs:1032` `witness_tlc.to_witness() != tracked_tlc.witness` comparisons remain byte-exact). Search all `SettlementWitness::build_from_witness` call sites (actor.rs:671, 1023, 1556, 2671) and thread the version.

- [ ] **Step 3: Run tests; commit**

```bash
cargo nextest run -p fnn watchtower --no-fail-fast
cargo fmt --all && make clippy
```

Expected: green.

```bash
git add crates/fiber-lib
git commit -m "feat(watchtower): parse settlement witness with per-channel contract version"
```

---

### Task 6: fiber-lib — reconciliation resolution for prefix-only preimages

**Files:**
- Modify: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/onchain_tlc_reconcile.rs` (lines 53-58 enum; 109-165 `resolve_onchain_tlc`; 251-299 `collect_onchain_timeout_settled_tlcs`; 302-332 received timeout collector)
- Test: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/onchain_tlc_reconcile.rs` (replace `resolve_returns_unknown_when_preimage_mismatches` at line 85; add collector test for the invalid-preimage case `<expect_expiry` handling)

**Interfaces:**
- Consumes: existing `OnChainTlcResolution::{Unknown, Fulfilled, SettledWithoutPreimage}`, `OnChainTimeoutSettledTlc` (role `Forwarded`/`OriginPayer`).
- Produces: `OnChainTlcResolution::SettledWithInvalidPreimage` — the exact-case fix. Callers that previously got `Unknown` for a full-hash-mismatch (with settlement record present) now get `SettledWithInvalidPreimage` and the timeout/fail-forward collectors accept it identically to `SettledWithoutPreimage`.

- [ ] **Step 1: Replace the failing semantics test**

In `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/onchain_tlc_reconcile.rs:85`, the current test `resolve_returns_unknown_when_preimage_mismatches` documents the vulnerable behavior. Replace it entirely:

```rust
#[test]
fn resolve_prefix_valid_full_hash_invalid_preimage_is_failed_settlement() {
    // exact settlement record persisted with a preimage whose hash prefix
    // (20 bytes) matches the committed hash but whose full hash differs
    let preimage = Hash256::from_uint(0xdeadbeefu64);
    let prefix_commit = Hash256::truncate_prefix(hash_algorithm.hash(preimage), 20);
    let payment_hash = // prefix_valid + attacker-chosen suffix,
        concat(prefix, random_12_bytes);
    let settlement = OnChainTlcSettlement { payment_hash, hash_algorithm, preimage: Some(preimage), tx_hash, tlc_index: 0 };
    store.insert_onchain_tlc_settlement(...);
    let channel_id = ...;
    let resolution = resolve_onchain_tlc(&channel_id, &store, tlc_id::Offered(0), payment_hash, hash_algorithm);
    // NEW: must be detected as a failed settlement rather than Unknown
    assert_eq!(resolution, OnChainTlcResolution::SettledWithInvalidPreimage);
}
```

(Adjust `Hash256::from_uint` / helper names to whatever exists in that file's test module — the helpers at `onchain_tlc_reconcile.rs:19-57` (`payment_hash_for`, `empty_channel_state`, `tlc_info`) already implement the base harness; extend with a `preimage hydro p` variant that truncates the prefix rather than reusing `payment_hash_for`.)

- [ ] **Step 2: Implement the resolution**

In `resolve_onchain_tlc` (lines 140-148), replace the fallthrough:

```rust
    let Some(preimage) = settlement.preimage else {
        return OnChainTlcResolution::SettledWithoutPreimage;
    };
    let discovered_payment_hash: Hash256 = hash_algorithm.hash(preimage).into();
    if discovered_payment_hash == payment_hash {
        return OnChainTlcResolution::Fulfilled(preimage);
    }
    // The exact on-chain output was consumed with a script-valid (20-byte prefix)
    // preimage that does not hash to the full committed payment hash: the
    // settlement provably took place and the upstream TLC provably cannot be
    // fulfilled with this preimage. Fail forward.
    error!(
        "On-chain preimage for channel {:?} tlc {:?} tx {:?} hashes to {:?}, expected full hash {:?}: treat as failed settlement",
        channel_id, tlc_id, settlement.tx_hash, discovered_payment_hash, payment_hash
    );
    OnChainTlcResolution::SettledWithInvalidPreimage
```

Swap `warn!` to `error!` (actively indicates a theft pattern; the store already retains the record for forensics), extend the enum at 53-58:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnChainTlcResolution {
    Unknown,
    Fulfilled(Hash256),
    SettledWithoutPreimage,
    /// The output was consumed with a preimage matching the committed 20-byte prefix
    /// but not the full 32-byte hash. Downstream provably took the money; the incoming
    /// TLC provably cannot be fulfilled. Must be treated like `SettledWithoutPreimage`
    /// by the fail-forward/timeout collectors.
    SettledWithInvalidPreimage,
}
```

Update the two collectors (`collect_onchain_timeout_settled_tlcs` 251-299 and `collect_onchain_received_timeout_settled_tlcs` 302-332) — the `matches!` patterns:

```rust
            if !matches!(
                resolve_onchain_tlc(...),
                OnChainTlcResolution::SettledWithoutPreimage
                    | OnChainTlcResolution::SettledWithInvalidPreimage
            ) {
                return None;
            }
```

ALSO in `collect_onchain_timeout_settled_tlcs`: drop the `tlc.expiry < expect_expiry` gate for the invalid-preimage hits specifically — the invalid-preimage TLC is already consumed on-chain immediately, so it must NOT wait for expiry. Simplest correct implementation: first resolve, then filter by expiry only when resolution == `SettledWithoutPreimage`:

```rust
            match resolve_onchain_tlc(&channel_id, store, tlc.tlc_id, tlc.payment_hash, tlc.hash_algorithm) {
                OnChainTlcResolution::SettledWithoutPreimage => {
                    if tlc.expiry >= expect_expiry { return None; } // wait for real expiry
                }
                OnChainTlcResolution::SettledWithInvalidPreimage => {} // immediate
                _ => return None,
            };
```

- [ ] **Step 3: Integration check for the full stranding path (regression test runner overview)**

Run the reconciliation suite (as noted in the review report):

```bash
cargo test -p fnn --features sqlite fiber::tests::onchain_tlc_reconcile:: -- --nocapture
```

Expected: all tests pass, including the new `resolve_prefix_valid_full_hash_invalid_preimage_is_failed_settlement` and the collector test asserting the forwarded TLC is queued for `RemoveTlc`/fail-forward despite `expect_expiry` in the future.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all && make clippy
git add crates/fiber-lib/src/fiber/onchain_tlc_reconcile.rs crates/fiber-lib/src/fiber/tests/onchain_tlc_reconcile.rs
git commit -m "fix(reconcile): treat prefix-valid full-hash-invalid on-chain preimage as failed settlement (OC-TLC-001)"
```

---

### Task 7: fee/size estimation + full repo gate

**Files:**
- Modify: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/fee.rs` (only if estimation lacks witness bytes)
- Test: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/fee.rs`

**Interfaces:**
- Consumes: Task 4 dual-format builders.
- Produces: fee estimation that accounts for the 12 bytes/HTLC witness growth in v1 (only if the current `estimate_tx_size` path doesn't already count the witness).

- [ ] **Step 1: Verify the estimate includes witness bytes, then add the size-sensitivity test**

First inspect how fee is computed today: `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/fee.rs:27-72` runs `checked_calculate_commitment_tx_fee` over a settlement tx built from `get_cell_deps_count(...)` plus tx size (`estimate_tx_size` / internal tx size calc). Concretely check by reading the function chain from `fee.rs:26` (`checked_calculate_commitment_tx_fee`) down to whatever size estimator it invokes, and answer one question: **is the witness section length included in the size estimate?**

If it counts actual tx bytes (built via `TransactionBuilder` with the witness attached), no code change is needed. If anything hardcodes the legacy layout (85) or omits witnesses entirely, fix that call site to serialize the actual witness instead.

Then add the regression pin in `/home/quake/workspace/fiber/crates/fiber-lib/src/fiber/tests/fee.rs` (mirroring the style of `fee.rs:4 checked_commitment_tx_fee_allows_intermediate_u64_overflow_when_final_fee_fits`):

```rust
#[test]
fn witness_bytes_are_counted_for_both_commitment_contract_versions() {
    // Build the SAME one-tlc settlement twice, differing ONLY in
    // CommitmentContractVersion (Legacy -> 85-byte HTLC, V1 -> 97-byte HTLC),
    // both through the channel builders that feed into
    // checked_calculate_commitment_tx_fee (reuse the fixtures the existing
    // fee.rs:4 test builds for its settlement tx; parameterize the version via
    // the builder signature introduced in Task 4).
    let fee_legacy = ...;
    let fee_v1 = ...;
    // 12 extra bytes per HTLC entry must not be free; allow for rounding.
    assert!(fee_v1 >= fee_legacy);
    // exactness where the estimator is byte-exact:
    // assert_eq!(fee_v1 - fee_legacy, expected_fee_for_12_bytes(fee_rate));
}
```

Replace the `...` by calling the real fee function the same way the existing test in that file does, passing the alternative version — the two call sites must be identical apart from the version argument. If it turns out the estimate already handles both layouts by construction, keep the test but assert equality of the *witness bytes* delta feeding the fee calc (i.e., assert the pre-fee tx size difference is exactly 12).

- [ ] **Step 2: Run the full gate**

```bash
cd /home/quake/workspace/fiber
make check        # debug/release/no-default-features builds
make check-migrate
cargo nextest run --no-fail-fast
cargo fmt --all && make clippy
typos
cargo shear
```

Expected: all green. (No RPC changes: `make gen-rpc-doc` / `make check-dirty-rpc-doc` only if the local pipeline is dirty.)

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "test(fee): verify commitment tx fee accounting for v1 witness layout growth"
```

---

## Handoff Notes for Part 2 executor

- Everything in Part 2 touches persisted state EXCEPT the reconcile fix (Task 6). Only Task 3's `ChannelActorData` field + Task 5's `TrackedSettlementTlc` are persisted-shape changes; both must pass `make check-migrate`.
- All Part-2 tests must remain green under nextest's default thread requirements (`.config/nextest.toml`).
- If `SettlementTlc`'s round-trip tests (`channel.rs` witness tests) exist in `fiber-lib/src/fiber/tests/channel.rs` around lines running settlement flow tests, fold the dual-format expectations there — do not create a parallel test file.
