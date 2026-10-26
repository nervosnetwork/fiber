# Design: ordered V2 reestablishment replay (`ReestablishReplayCompleteV2`)

Status: proposal, not implemented. Related: #1561, #1677, and the cursor
reconciliation section of `channel-v2.md`.

## Problem

After a reconnect, `reestablish_v2` replays the persisted outstanding
`CommitmentSignedV2` as soon as the handshake is validated. The peer verifies it
by rebuilding the commitment transaction from its *current* `tlc_state`.

When TLC updates crossed during the disconnect, the two peers signed their
rounds from different `tlc_state` views, so the rebuilt transaction differs and
the replay fails with `failed to verify signature: signature is invalid`. Both
channels then stay `reestablishing` in `Syncing` until `REESTABLISH_TIMEOUT`
(5 minutes) and are force-closed.

Evidence from the crossed-round reproduction
(`test_v2_crossed_rounds_do_not_storm_commitments`): before any fix 1 of 6 runs
converged; abandoning a CS whose *own* rebuild already differs raised that to
4 of 6. The remaining 2 of 6 are this case.

Cursors cannot detect it. The signer rebuilds from its own (unchanged) state and
gets exactly the transaction it signed, so `outstanding_v2_cs_is_stale` is
false; only the peer's state differs. There is no field in
`ReestablishChannelV2` that describes the peer's TLC set, and adding one would
be a full state digest exchange.

## Proposal

Make reestablishment explicitly two-phase for V2 channels:

1. **Phase 1 (replay).** Exchange `ReestablishChannelV2`, validate cursors and
   nonces as today, replay owed `AddTlc`/`RemoveTlc` updates in their original
   order, and replay the owed ACK (`session.incoming.response`) if the peer
   missed it. **Do not sign or replay a `CommitmentSignedV2`.**
2. **Marker.** After Phase 1 finishes, send the new
   `ReestablishReplayCompleteV2 { channel_id }`.
3. **Phase 2 (commitment).** Run the commitment step only after the peer's
   marker has been received: replay the persisted CS, abandon + re-sign a stale
   one, or answer the peer's replay from the durable record.

Because messages on a connection are delivered in order, the marker is only
received after the peer has applied every replayed update from that side. Both
peers therefore enter Phase 2 with the same TLC set, which is exactly the
guarantee that is missing today.

```
A                                                    B
|-- ReestablishChannelV2 --------------------------->|
|<-------------------------------- ReestablishChannelV2|
|-- AddTlc/RemoveTlc replay (owed) ---------------->|
|<--------------- AddTlc/RemoveTlc replay (owed) ---|
|-- ReestablishReplayCompleteV2 ------------------->|
|<------------------ ReestablishReplayCompleteV2 ---|
|-- CommitmentSignedV2 (replay or fresh) ---------->|
|<----------------- CommitmentSignedV2 (replay/fresh)|
|-- RevokeAndAckV2 -------------------------------->|
```

## Wire format and compatibility

Append `ReestablishReplayCompleteV2 { channel_id: [u8; 32] }` to the V2 channel
message union in `fiber-types/src/schema/fiber.mol` the same way
`CommitmentSignedV2`/`RevokeAndAckV2` were appended, without touching legacy
discriminants. It is only meaningful for `channel_features.is_v2()`; on a legacy
channel it is rejected like any other V2-only message
(`channel.rs::handle_peer_message` version filter).

V2 is not released yet (the feature byte `1` and its messages are unpublished,
and the migration merged in #1665 keeps the same epoch), so no legacy
negotiation is required. A node that speaks V2 without this message cannot
complete Phase 2 with a peer that waits for the marker; since the capability is
unreleased, treat that as a coordinated rollout requirement and record it in
`channel-v2.md`. Do not add a silent fallback initially: falling back would
reintroduce the crossed-view failure while hiding it.

## State machine

Runtime-only fields on `ChannelActorState` (no migration; the durable
`session.outgoing`/`session.incoming` records stay the source of truth):

- `recovery_replay_sent: bool` — Phase 1 finished for the current recovery.
- `recovery_peer_replay_complete: bool` — peer marker received.
- `recovery_commitment_pending: bool` — Phase 2 is owed but parked.

Changes in `channel_v2.rs::reestablish_v2`:

1. After cursor/nonce/history validation, replay updates and the owed ACK only;
   set `recovery_replay_sent = true`; send the marker.
2. Replace the current "replay/sign CS here" tail with
   `self.run_recovery_commitment_step(myself, state).await?`, which returns
   early (and sets `recovery_commitment_pending`) while
   `recovery_peer_replay_complete == false`.
3. `run_recovery_commitment_step` keeps today's decisions: replay a verifiable
   persisted CS, abandon and re-sign a stale one, answer a duplicate from
   `session.incoming`, then `finish_recovery_v2`.

New handler `receive_reestablish_replay_complete_v2`:

- reject unless `state.reestablishing && state.recovery_peer_v2.is_some()`
  (a marker on an idle channel is a protocol violation);
- idempotent on duplicates;
- set `recovery_peer_replay_complete = true`, clear
  `recovery_commitment_pending`, and run `run_recovery_commitment_step`.

`session.pending_incoming` / `session.pending_ack` processing currently happens
at the end of `reestablish_v2`; it must move into Phase 2 as well, because those
paths can sign or answer commitments.

## Crash and loss points

The marker is runtime-only and the CS session records are durable, so a crash
only restarts recovery:

| Crash point | After restart |
| --- | --- |
| after replay, before marker | new handshake; replay repeats (duplicate `AddTlc` with the same id is rejected as an error today and must be made idempotent, see below); marker re-sent |
| after marker sent, before Phase 2 | peer re-handshakes; Phase 1 repeats; Phase 2 runs |
| after Phase 2 CS, before ACK | existing durable CS replay/abandon rules apply |
| marker lost in flight | both sides park; bounded by `REESTABLISH_TIMEOUT`. Optional: re-send the marker from `MaintainChannelTlcs` while parked and `recovery_replay_sent` is true |
| both peers crash | symmetric; each restart replays and re-marks |

One prerequisite falls out of the table: a replayed `AddTlc` for a TLC the peer
already applied must be a no-op (identical content) rather than
`InvalidParameter("Received tlc id ... is not the expected next id ...")`. That
is safe and independent of the marker, and should land first.

## Tests

Acceptance is the existing crossed-round reproducer converging every run:

- `test_v2_crossed_rounds_do_not_storm_commitments`: extend to assert both
  payments settle, and run it repeatedly (it is the regression for this design).

New deterministic coverage:

1. Ordering: with the peer's marker withheld, no CS is signed or replayed;
   delivering the marker then performs exactly one Phase 2 step.
2. Idempotency: duplicate marker, plus a marker received before our own Phase 1
   finished, still produce one Phase 2 step.
3. Validation: marker on a non-reestablishing channel, on a legacy channel, and
   with a wrong channel id are rejected without state change.
4. Crash matrix: persist after Phase 1, drop the runtime flags, re-run
   reestablishment, assert convergence and no duplicate signing.
5. Timeout: marker never arrives → no signing, no panic, and the channel is
   closed by the existing `REESTABLISH_TIMEOUT` path.
6. Nonce audit: the feature-gated secnonce-reuse assertion
   (`Musig2SignContext::sign`) must stay clean across the crash schedules.

## Alternatives considered

- **Reuse `ChannelReadyV2` as the marker.** Rejected: it is bound to the opening
  phase (`AwaitingChannelReady`/`AwaitingTxSignatures`, number must be
  `INITIAL_COMMITMENT_NUMBER + 2`) and stored in `session.remote_ready`, so
  reusing it would collide with bootstrap validation.
- **Always abandon and re-sign, never replay.** Nonce-safe, but the fresh CS is
  still signed at handshake time, before the peer's replayed updates arrive, so
  the view divergence remains.
- **Let the receiver detect it.** Impossible without a state digest: the
  receiver cannot distinguish a stale signer from a malicious one, and the
  signer cannot detect it either (its own rebuild matches).
- **Exchange TLC-set digests in the handshake.** Would diagnose the divergence
  but still needs a reconciliation phase to fix it, so it is strictly more work
  than the marker.

## Open questions for review

1. Marker as a new message (proposed) versus an explicit third handshake round
   that reuses `ReestablishChannelV2` with a `complete` flag.
2. Retransmit the marker from maintenance while parked, or rely solely on
   `REESTABLISH_TIMEOUT`?
3. Rollout: is a coordinated V2 rollout acceptable, or does this need a
   capability bit (no room in the current byte, would need another feature
   pair)?
