# V2 commitment rounds after a reconnect

This note records what the crossed-round investigation actually found, so the
conclusions are not re-derived incorrectly later.

## Root cause: the owed updates were never replayed

`reestablish_v2` regenerated the outstanding commitment but its "replay the owed
updates" branch was dead code (`if false`). After a reconnect the sender therefore
signed a commitment containing an update the peer had never received, so the peer
rebuilt a different transaction for the same commitment number and rejected the
signature:

```
DIAG sign   n=3 nonce=<X> msg=30a065d7...
DIAG verify n=3 own=<X>   msg=591d982c...   -> signature is invalid
```

The aggregate nonce and the commitment number matched; only the message differed,
because the peer's membership was missing one TLC. Fix: replay the owed updates in
their original relative order before regenerating the signature, and carry the
original update list into the regenerated round so a second crash can replay it
too.

Symptom-level notes from before the root cause was known, kept because they are
still true and they explain the secondary fix:

- the two sides can disagree about membership when one side folded an update while
  it still owed an ACK (`AnnounceWaitPrevAck`) and the other did not. V1 serialises
  the two commitment directions so that rung cannot be present at signing time; V2
  keeps them independent, so the `for_remote` view must include it;
- `RemoteAnnounced` deliberately stays excluded for `for_remote`: the peer sees the
  same update as its own `LocalAnnounced`, which *is* included for `for_remote`, so
  the two views agree.

## What was tried and rejected

The following were measured against the reproducer and are wrong; do not repeat
them.

| Attempt | Result |
|---|---|
| Two-part rule (`commitment_signed_tlcs_v2`) in `build_settlement_data` | 0/6 settle; re-includes updates the durable commitment already dropped, so healthy rounds stop agreeing |
| Include received-but-unacked TLCs in the peer commitment | breaks `test_v2_paired_concurrent_updates` |
| Drop unverifiable commitments during recovery | 1/6; both sides drop and stop |
| Drop + restart the handshake, bounded | 2/6; adds a state field without converging |
| Add a durable `removed_at` marker to `TlcInfo` | never needed: the failure was the missing replay, not the membership rule |
| Fold statuses while processing an ACK | starts a new round on every ACK (`#1561` storm); only updates folded by a real peer commitment may trigger a round |

## The deadlock was bounded anyway

Before the replay fix, a stuck state (`waiting_ack`, `ChannelReady`, an
unverifiable commitment) was already covered by the peer-response watchdog:
`set_waiting_ack(true)` arms `CheckActiveChannel` at `PEER_CHANNEL_RESPONSE_TIMEOUT`
(30 s in production, 10 s under `cfg(test)`), `ack_peer_wait_v2()` exposes the
durable deadline, `MaintainChannelTlcs` re-checks it, and on expiry the actor calls
`notify_network_actor_shutdown_me` (force close). Measured with a widened window:

```
A final: state=Closed(CloseFlags(UNCOOPERATIVE_LOCAL | WAITING_ONCHAIN_SETTLEMENT))
B final: state=Closed(CloseFlags(UNCOOPERATIVE_LOCAL | WAITING_ONCHAIN_SETTLEMENT))
```

So the worst case was a bounded uncooperative close, not a wedged channel. No
attempt counter is needed: it would call the same `force: true` shutdown.

## Regression coverage

- `test_v2_crossed_lost_commitments_settle_after_reconnect` — deterministic crossed
  recovery (`lose_raa` both ways), asserts both payments settle and balances move by
  exactly the fee.
- `test_v2_crossed_rounds_do_not_storm_commitments` — requires both payments to
  settle *and* bounds the rounds and retained remote commitment points.
- `test_v2_recovery_replays_updates_before_regenerated_cs` — replay order.
- `test_v2_settlement_accounts_for_prev_ack_updates` — the `AnnounceWaitPrevAck` /
  `RemoveWaitPrevAck` alignment.
- `test_v2_recovery_dual_owed_preserves_both_original_orders` — cached ACK is
  replayed byte for byte while the commitment itself is regenerated.

`test_v2_reestablish_lost_raa_replays_exact_response_and_gates_ready` stays ignored:
it drives recovery through an ACK captured before the reconnect, which the rotation
contract makes unverifiable. Rewrite it around the regenerated commitment (deliver
what the peer sends after the handshake) before re-enabling.
