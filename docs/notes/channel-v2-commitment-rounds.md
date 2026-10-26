# Design: spec-aligned V2 recovery (two-part content + regenerate the signature)

Status: implemented in the working tree, tests partly updated. Supersedes
`channel-v2-commitment-rounds.md` (per-TLC numbers, rejected) and the snapshot
proposal.

## What the Lightning MuSig2 spec does

`bolt-simple-taproot.md` (the MuSig2 channel spec, merged in `lightning/bolts`)
handles reconnection like this:

- `channel_reestablish` gains a `next_local_nonces` TLV: fresh MuSig2 nonces, one per active
  commitment, sent on every re-establishment.
- Retransmission is **not** a signature replay: "It decides to retransmit the exact `update_`
  messages ... THEN it must regenerate the partial signature using the newly received
  `next_local_nonces`".
- Nonces must be persisted if not counter/shachain derived; each commitment state uses a fresh
  session, so a nonce is never reused for a different message.
- Cooperative close has its own nonce purpose (`closer_nonce` / `next_closee_nonce`), which is
  what our `NoncePurposeV2::Closing` already does.

The reason this is sufficient for them is that commitment content is a deterministic function of
the update stream and the counters, which both peers derive identically.

## Changes adopted here

1. **Content rule (V2 only)**: `TlcState::commitment_signed_tlcs_v2` implements the "two-part"
   rule - a commitment contains the updates its owner received, plus the updates its owner sent
   that the counterparty acknowledged. Fiber's `*PrevAck` rungs (which defer an update by a round
   when a CS was already in flight) are treated like their `*WaitAck` counterparts, because the
   deferral is a local-ordering artifact and lets the two peers disagree.
   `ChannelActorState::get_active_tlcs` selects this rule when `channel_features.is_v2()`; V1 keeps
   the historical ladder so released channels do not change content.
2. **Recovery never replays a signature**: `reestablish_v2` abandons an outstanding CS whose ACK
   never arrived and re-signs after recovery, with a fresh nonce from `allocate_nonce` (fresh
   random seed per session). The owed updates are still replayed in order. This removes
   `outstanding_v2_cs_is_stale` and the "replay the exact persisted CS" path.
3. Unchanged: `ReestablishChannelV2.next_local_commitment_nonce` already plays the role of
   `next_local_nonces`; no new message or field is needed.

## Consequences

- Content changes for V2 channels (the `*PrevAck` deferral is gone), which is acceptable because
  V2 is unpublished; V1 content is untouched.
- Recovery performs one extra signature (the spec does the same); no nonce reuse because every
  session allocates a fresh seed and `bind_context` refuses to rebind a nonce.
- The four V2 recovery tests that assert byte-identical replay of the persisted CS now encode the
  replaced contract and must be updated:
  - `test_v2_recovery_crash_with_unsent_cs` asserts `replayed.request == original.request`; the new
    contract is "same commitment number, fresh nonce, different request".
  - `test_v2_recovery_dual_owed_preserves_both_original_orders` and
    `test_v2_recovery_resumes_staged_incoming_and_ack` wait for the replayed CS message count.
  - `test_v2_recovery_handshake_precedes_replay_and_watchdog_bounds_wait` asserts
    `state.reestablishing` after the handshake, which no longer holds once the stale CS is
    abandoned and recovery completes.

## Cleanup enabled by this route

| Item | Status |
| --- | --- |
| `outstanding_v2_cs_is_stale()` | removed |
| "replay exact CS" branch in `reestablish_v2` | removed |
| `resign_stale_cs` conditional | became the unconditional recovery path |
| `receive_commitment_v2` duplicate-request special case | keep as idempotency for retransmits |
| `pending_incoming` re-processing | keep (crash safety) |
| `waiting_ack`, `*PrevAck` statuses, `OutgoingCommitmentV2.updates`, fold gate | keep: still drive the normal signing path; `*PrevAck` no longer influences membership, so they can be simplified later |

## Validation still to do

1. Update the four tests above to the new contract.
2. Crossed-round reproducer: expect deterministic convergence instead of 4/6.
3. Re-run `v2` + `settle` + `onchain` suites and the storm regression.

## Residual crossed-round failure: the signed message differs

With the retired-nonce history in place the reconnect reproducer settles 4/6 instead
of 2/6. Instrumenting both sides for the remaining cases gives a conclusive split:

```
DIAG sign   n=3 nonce=<X> msg=30a065d7...
DIAG verify n=3 own=<X>   msg=591d982c...   (signature invalid)
```

The aggregate nonce matches and the commitment number matches, yet the message the
peer signed is not the message we rebuild. So the residual is *not* about nonces or
about which TLCs a commitment contains: on the failing pairs both sides report the
same set (`["Received(0)", "Offered(0)"]`) and the same `created_at` values. The
divergence must therefore be a role-dependent field of
`build_commitment_tx_and_settlement_data(for_remote)` — the two sides call it with
opposite `for_remote` values for the same commitment, so any field that is computed
from the local view instead of the commitment owner's view splits them.

Next step for whoever picks this up: log the full `SettlementData` from both sides
for the same commitment number and diff it field by field (balances, fees, removed
TLC effects, revocation/delayed outputs). Do not guess the membership rule again —
the 15-fixture sweep in this note already ruled out per-status membership changes.

### Instrumented confirmation: the commitment *membership* differs by one TLC

Dumping `build_commitment_transaction_output` on both sides (for_remote, version,
settlement witness hash, TLC list) for a failing sample gives this table:

```
rm=true  v=3 wit=2415e67e tlcs=["Received(0)/2000/1793884789228", "Offered(0)/2000/1794172789228"]
rm=false v=3 wit=8a678f13 tlcs=["Offered(0)/2000/1794172789228"]
```

The lock-script aggregate key is symmetric (matching `rm=true` / `rm=false` pairs
agree), the version is the same, the TLC ids that *both* include are identical in
amount and expiry. The only difference is that one side omits the TLC it
**received** from the commitment it is about to sign, while the peer signing that
same commitment includes it.

In BOLT 2 terms, the owner's commitment contains the updates it received plus the
updates it sent that the peer acknowledged. The failing side is dropping a
received update, i.e. `commitment_signed_tlcs_v2` still gates the received half on
an acknowledgement rung (`AnnounceWaitAck` / `AnnounceWaitPrevAck`) in this state
instead of including everything the owner received for that commitment number.

So the remaining fix belongs in the *membership* rule after all, but not in the
form the earlier 15-fixture sweep rejected: the sweep varied whole status→membership
maps, whereas the defect here is narrower — a TLC drops out of one side's view of
the owner's commitment in the crossed/removal rungs.

Tried and rejected: switching `build_settlement_data` to
`TlcState::commitment_signed_tlcs_v2(for_remote)` when the channel is V2. That makes
the crossed reproducer strictly worse (0/6 settle, previously 4/6) because the
two-part rule re-includes updates that the durable commitment had already dropped,
so healthy rounds stop agreeing too. The settlement builder must key on the durable
commitment numbers stored on the TLC (`created_at`, and whatever removed-at marker
is added), not on the live status ladder; the 15-fixture sweep showed `created_at`
alone changes the content of 8/10 healthy pairs, which is why a *removed-at*
marker is required before this can be attempted again.

### Experiment: drop a whole commitment round instead of only the commitment

Hypothesis: the crossed reproducer is an artifact of dropping the commitment while
delivering the update that belongs to it. Measured by holding two
`CommitmentOrAck` messages instead of one `CommitmentSigned` and discarding both:

```
drop CS only        : 4/6 settle
drop CS + answer    : 0/6 settle
```

So the hypothesis is wrong, and the result is more useful than the original flaky
observation: dropping a whole round is **deterministic** (0/6), while dropping only
the commitment is flaky (4/6). A lost round — the plainest reconnect case, with no
crossed state and no in-flight updates — now reproduces every time, which is a much
better target than the probabilistic one.

Next step: debug the 0/6 variant first. It needs no crossing, so the failure should
reduce to "which of the two owed messages is replayed and in what order" rather than
to commitment content. Only if that variant turns out to agree on content should the
content question be revisited.
