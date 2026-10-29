# Design: reconciling V2 commitment content after a reconnect

Status: proposal, not implemented. Related: #1561, #1677, and the cursor
reconciliation section of `channel-v2.md`.

## Problem

After a reconnect, `reestablish_v2` replays the persisted outstanding
`CommitmentSignedV2`. The peer verifies it by rebuilding the commitment
transaction from its own `tlc_state`, and the two sides can disagree even when
they hold exactly the same TLCs. The replay then fails with
`failed to verify signature: signature is invalid`, and both channels stay
`reestablishing` in `Syncing` until `REESTABLISH_TIMEOUT` (5 minutes).

Measured on the crossed-round reproduction
(`test_v2_crossed_rounds_do_not_storm_commitments`): 1 of 6 runs converged
before any fix, 4 of 6 after abandoning a CS whose own rebuild already differs
(`c8b7bba0`). The rest are this class.

## Minimal experiment

In a failing run both peers hold the same two TLCs with identical creation
numbers, but one rung apart in their status ladder:

```
A: local=2 remote=3  offered(0)=LocalAnnounced  received(0)=AnnounceWaitPrevAck
B: local=3 remote=2  offered(0)=Committed       received(0)=AnnounceWaitAck

A set for B's commitment #2 (by status) = ["Offered(0)"]
B set for B's commitment #2 (by status) = ["Received(0)", "Offered(0)"]
A created_at = Offered(0)@local=2@remote=2, Received(0)@local=2@remote=2
B created_at = Offered(0)@local=2@remote=2, Received(0)@local=2@remote=2
```

So the *inputs* both peers keep are identical, while the *selector* disagrees.
The selector is `ChannelActorState::build_settlement_data`, which decides
membership from the mutable status:

| TLC status | in our own commitment (`for_remote = false`) | in the peer's commitment (`for_remote = true`) |
| --- | --- | --- |
| Offered `LocalAnnounced` | no | yes |
| Offered `Committed` / `RemoveWait*` | yes | yes |
| Received `RemoteAnnounced` / `AnnounceWaitPrevAck` | yes | no |
| Received `AnnounceWaitAck` / `Committed` | yes | yes |

A signed B's commitment while B's TLC was `AnnounceWaitPrevAck` on A's side
(excluded), but B rebuilds it from `Committed` (included). Same TLCs, different
transaction.

## Why a "replay complete" marker would not fix this

An extra `ReestablishReplayCompleteV2` (or a `replay_complete` field on
`ReestablishChannelV2`) only orders the replayed updates before the commitment
step. Here nothing is missing: both peers already applied every update, and
replaying them again does not change any status. The disagreement is produced by
the status ladder itself, so the same replay would still fail after the marker.
That proposal is withdrawn as the primary fix; it could only narrow a crash
window once the content is deterministic.

## Correct direction

Make the membership of a commitment a function of durable, symmetric inputs
instead of the mutable ladder. Both peers already persist what is needed:

- `TlcInfo.created_at` (the local and remote commitment numbers at creation),
  identical on both sides in the experiment above;
- `OutgoingCommitmentV2.settlement` / `IncomingCommitmentV2.settlement`, the
  exact TLC set that was signed in that round.

Concretely:

1. Define a canonical predicate `tlc_in_commitment(tlc, commitment_number)` from
   `created_at` plus the removal snapshot (for example: created at or before
   that number and not removed before it). Its offset must be pinned by tests,
   but the experiment shows both peers' inputs are symmetric, so the predicate
   evaluates identically on both sides.
2. Use that predicate as the selector in `build_settlement_data` for V2
   channels, and keep the status ladder for progression bookkeeping only. The
   ladder can then skew without changing what a commitment contains.
3. On reestablishment, verify a replayed CS against the canonical set (or, when
   available, against the persisted `settlement` snapshot of the accepted
   round), so a byte-identical replay verifies even if the ladder drifted.
4. Keep V1 behaviour unchanged: gate the selector on `channel_features.is_v2()`
   or prove equivalence for the legacy layout, because `build_settlement_data`
   is shared with V1 and the V1 commitment layout must not change.

This removes the need for a new wire message. The remaining wire-level question
is whether a replayed CS must also carry the settlement snapshot to be verified
against; that is only needed if the canonical predicate cannot be derived
locally, which the experiment suggests it can.

## MuSig2 nonce constraints (why BOLT 2 is not directly transferable)

BOLT 2 recovers by retransmitting byte-identical `commitment_signed` and
`revoke_and_ack` messages, chosen by `next_commitment_number` /
`next_revocation_number`. That is safe for Lightning because each commitment is
a single-party ECDSA signature whose nonce is derived per message, so
retransmission never reuses a nonce across different messages.

Our commitment is a MuSig2 aggregation, so the constraint is stricter:

- A secret nonce MUST NOT sign two different messages (nonce reuse leaks the
  key). RFC 9611 also asks implementations to persist nonces before use and to
  treat a possibly-used nonce as burned.
- Therefore only two moves are legal after a reconnect: **replay the exact
  bytes** that were signed, or **burn that session and sign with a fresh
  nonce**. "Re-sign the same state until the peer accepts" is not a safe loop.
- V2 already provides this: `session_v2::allocate_nonce` uses a fresh random
  seed per session (not derived from the commitment number), the exact
  transaction, settlement and signing context are persisted in
  `session.outgoing` before the message leaves, and `bind_context` refuses to
  bind the same nonce to a different `(keys, aggregate nonce, message)`
  (`Conflicting V2 signing context for consumed nonce`). The abandoned-session
  path added in `c8b7bba0` relies on all three.
- This is also why the V1 re-sync proposal was rejected (#1590/#1628): V1 derives
  the secret nonce from the commitment number and purpose, so re-signing the
  same number in a different context can reuse it.

How other proposals avoid the problem:

- **Simple taproot channels**: MuSig2 is used only for the funding output, which
  is signed once (or during a splice), while commitment transactions stay
  single-party. BOLT 2 retransmission remains valid there.
- **eltoo / Decker-Russell-Osuntokun (LN symmetry)**: a single signature stays
  valid for any successor state (`SIGHASH_SINGLE|ANYONECANPAY` over the update
  transaction), so state changes never require re-signing the same data with the
  same nonce.
- **Common practice for MuSig2 state machines**: pre-commit the public nonce,
  persist the signing context before sending, and on restart replay the exact
  message or burn the session. Nobody retransmits a *different* signature for
  the same slot.

The design implication is that recovery must make the message reproducible
(deterministic content, as proposed above), not make the signer try again.

## Tests

Acceptance: the crossed-round reproducer settles every run, asserted in
`test_v2_crossed_rounds_do_not_storm_commitments` (currently it only asserts
bounded rounds and points).

New deterministic coverage:

1. `tlc_in_commitment` predicate: for every pair of statuses reachable after a
   reconnect, the set derived from `created_at` matches on both peers while the
   status-derived set may differ (encode the experiment above as a unit test on
   the two captured states).
2. Replay verification uses the canonical set: a CS signed before a status
   skew still verifies after the skew.
3. V1 unchanged: existing V1 commitment tests plus the V1 #1561 reproducer keep
   their current behaviour.
4. Recovery without a status skew stays byte-identical: no new `commitment_signed`
   is produced, the persisted request is replayed exactly.
5. Crash matrix: persist, drop runtime state, rerun reestablishment, converge.
6. Nonce audit: the feature-gated secnonce-reuse assertion stays clean.

## Open questions

1. Exact predicate and offset for `created_at` (is a TLC created at `(2,2)`
   included in commitment 2 or 3?), to be pinned by tests against the legacy
   derived sets so no channel changes contents at upgrade.
2. Do we still want the `ReestablishReplayCompleteV2` marker as a second step to
   bound the crash window between "replayed" and "signed"? It is not required
   for correctness once the content is deterministic.
3. Whether to also verify against the persisted `settlement` snapshot (stronger,
   but requires the snapshot in the replayed message or an agreed hash).
