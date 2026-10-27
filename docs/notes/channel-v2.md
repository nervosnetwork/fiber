# Channel V2 capability and durable close

The node capability `CHANNEL_V2` uses the existing required/optional feature pair
at bits 6/7. It combines independent commitment-owner nonce sessions with full
32-byte on-chain payment hashes. The commitment contract feature byte remains
`1`; released legacy channels with byte `0` continue to use V1 messages and their
original contract layout. `ChannelFeatures::V2` has value `1` and selects both
nonce sessions and the full payment hash layout; `LEGACY` is `0`.
`has_full_payment_hash()` delegates to `is_v2()`, without a separate feature bit.

Deployment prerequisite: a node must only advertise `CHANNEL_V2` once its
configured commitment-lock contract accepts the 58-byte V2 arguments (the legacy
57-byte args plus the trailing feature byte) and the full 32-byte on-chain
payment hash. The code hash of the deployed contract is not versioned in this
repository, so enabling the capability on a network requires confirming the
resolved mainnet/testnet commitment-lock binary from the deployment records
before rollout.

Cooperative close uses fresh `NoncePurposeV2::Closing` nonces, advertised in
`ShutdownV2`. The durable closing session retains both shutdown advertisements,
the original transaction, the exact signing context and both partial responses.
Pending TLCs must finish acknowledgement and balance application before the
closing transaction is fixed. Reconnection replays these records, without
reactivating commitment or revocation nonces. Conflicting advertisements and
signatures are rejected. Force close supersedes cooperative publication and uses
the latest actual usable commitment transaction.

The closing session also retains the peer-wait start time. Missing
remote ShutdownV2 and missing ClosingSignedV2 after local signing are outstanding
peer obligations with a bounded watchdog, including after reconciliation finishes.
Duplicate messages and reconnects do not extend that deadline. Local TLC draining
and on-chain confirmation after both signatures are distinct from peer waits.

Outgoing CS intents retain their ACK wait start too. A genuine outstanding
RAA remains a peer obligation during shutdown draining even after both shutdown
advertisements arrive. The watchdog uses the earliest outstanding CS-ACK/close
deadline. Restoring an actor from the database resets outstanding peer-wait start
times to the current time and persists them, granting a full response window so
local downtime does not cause an immediate force close. Actor startup rearms the
watchdog before any reconnect or handshake, and periodic maintenance also checks
it while the peer stays offline. Duplicate traffic, live-actor reconnects, and
reconciliation do not replace these durable timestamps. Quarantined V2 Stale,
local-only signing/draining, and both-signature on-chain waits do not authorize a
peer-timeout publication.

## Persistence boundaries

Nonce advertisements, signed responses, usable transactions and signing-acceptance
notifications are published only after their session/replay state is stored.
Staging, context binding and signing can run together in memory: when nothing is
published between them, one save of the accepted state is sufficient. Previously
stored intermediate sessions remain readable and recoverable. Cached closing
signatures and already-ready closing transactions can be replayed without another
early save. Ready replay skips its early save only when a previous reconnect
message has already committed the nonce; a handshake that rotates it still saves.

For offered TLC removals, the first save records the TLC and bound execution in
the durable effect outbox. That intent authorizes delivery even if source balance
application has not yet been saved; receipt/replay recovery handles this partial
state. The normal actor message-tail save records the applied balance and flags,
then publishes the ready outbox prefix once, without a second per-TLC application
save or a replay after every TLC. Maintenance and reestablishment also request
publication at that boundary. A later execution-lookup failure leaves earlier
successes ready for publication and the unbound TLC unapplied for retry.

The runtime prefix ends at the last successful application, excluding a later
intent whose application fails. Before requesting force close for such a failure,
the actor uses the same commit/publish boundary to release earlier successes, so
close supersession cannot overtake them. Startup replays the durable outbox even
though runtime publication markers are not restored. Startup and offline-owner
updates keep explicit saves because they do not pass through the message tail.
CS/ACK signing and RPC ownership-handoff boundaries retain their existing saves.
Forward-owner recovery saves only when it changes channel state. Outbox receipts
use the normal tail commit unless actual preimage deletion needs an earlier save.

## Backup restoration and ordinary crash recovery

Actual database backup restoration marks penalty-risk channels `Stale`. A V2
`Stale` channel remains quarantined: peer counter/nonce assertions cannot establish
freshness relative to history lost by rollback. It cannot become Ready, generate
new commitment/revocation/closing signatures, or locally publish its potentially
revoked commitment (including force close or watchdog-driven close). Cached
protocol responses are not replayed from quarantine. The network publication
boundary also refuses local closing publication while the stored V2 channel is
Stale. There is no automatic unquarantine procedure or independent freshness proof
in this implementation. Replacing a nonce would not make the old commitment safe.

Ordinary restart from the same database image retains Ready/ShuttingDown state and
its exact durable signing intents, so the normal reconciliation/replay path still
works. Released V1 channels retain their historical stale peer audit behavior.

Database preflight decodes the complete current `ChannelActorData` for every
current-epoch channel, including closed channels, and validates its cryptographic
intents before any migration writes. An epoch alone cannot establish that a
full-hash record contains genuine V2 state. Unsupported unpublished full-hash V1
databases are refused without conversion; released zero-feature records retain
their legacy payload and an empty V2 session.

The latest migration epoch remains `20260925120000` from #1665. Its frozen
published target ends at the feature byte. The shared
`fiber_types::channel_v2_validation::decode_channel_actor_data` decoder reads
that exact legacy-zero shape by appending only a serialized `None` in memory,
then decoding the complete current schema and checking feature/session
correspondence. Store getters,
channel iteration, and raw `fiber_types::deserialize::<ChannelActorData>` calls
use this codec without revalidating cryptographic snapshots or counters on every
runtime read. `decode_and_validate_channel_actor_data` additionally invokes the
full intent validator for September preflight and database validation. Actor
restore invokes `validate_session_v2` before any writes or signing. Opening an
already-current published database does not rewrite records or its version.
V2 sessions, signed history, and trailing bytes must always decode exactly.

The public raw-KV helper `fiber_types::deserialize` now requires
`DeserializeOwned + 'static` rather than `Deserialize<'a>`. This source-level
API change enables safe actual-type dispatch for owned stored values. Callers
that need borrowed decoded values can use `bincode::deserialize` directly.

Current preflight uses the backend's existing paged prefix iterator (100 records
per page), so retained closed histories are not all materialized at once. Every
record is still validated before migration writes/version stamping. Usable V2
commitments require exactly one canonical 112-byte funding witness: the 16-byte
XUDT-compatible prefix, aggregate key, and aggregate signature. Production and
validation use the same encoder.

Deterministic schema/store samples have three variants: minimal V1, fully
populated V1, and genuine V2 bootstrap. Each variant has a distinct deterministic
channel ID so storing one cannot overwrite another.

## Queued TLC updates

V2 keeps one outgoing commitment outstanding per direction. Local TLC operations
received while waiting for its ACK stay in the existing retry queue; the ACK does
not confirm those queued updates. Once unblocked, the actor drains the queue in
order, checks each operation against the updated channel limits, and signs the
successful updates in one commitment. Successful add results are notified only
after that commitment intent is stored. Peer updates already awaiting a follow-up
commitment share this round. V1 retains its single-success retry behavior.

## Restart recovery for forwarded and settled TLCs

Two durable records outlive the in-memory task that was supposed to finish them,
and neither is replayed by reestablishment (which only resends offered/removed TLC
updates). Both are re-driven from the persisted state instead.

A forwarding task (`waiting_forward_tlc_tasks`) is reconciled on actor start. An
already-created downstream TLC carries its upstream link in
`TlcInfo::forwarding_tlc`; recovery re-links the incoming TLC to it. A downstream
`AddTlc` can instead still be in `retryable_tlc_operations`, with its peeled onion
and `previous_tlc` preserved but no outgoing TLC id assigned yet. Recovery matches
the payment hash and exact upstream channel/TLC identity in either representation,
and retains the waiting guard for a queued Add until its execution reports a
result. Only when neither durable representation exists may recovery fail the
incoming TLC back for retry. Before a queued Add is published, its upstream TLC
must still exist, match the payment hash and hash algorithm, and have neither a
selected/queued removal nor an existing downstream link. Trampoline forwards are different:
their durable owner is the payment session's `TrampolineContext.previous_tlcs`,
not a downstream channel's `forwarding_tlc`. Recovery matches the payment hash and
exact upstream channel/TLC identity, retains the pending-forward guard against
local auto-fulfillment, and resumes the payment owner via `RetrySendPayment`.
That path also replays the upstream result when the session is already terminal.
A closed channel is skipped, since its TLCs settle on-chain.

A settled V2 removal needs the payment effect (`GetV2PaymentExecution`) before its
outbox entry becomes durable. The network actor can be busy signing funding
transactions for many seconds, so the lookup waits up to 30s and, if it still
fails, leaves the TLC settled but unapplied and
retries from the maintenance timer and after the next restart. The channel actor
never panics on this transient lookup, and no outbox entry is written before its
effect is bound.

## Peer-response deadline versus a partition

The durable `peer_wait_started_at` is persisted so that an unanswered obligation is not
forgotten by a restart. A restart resets it (local downtime must not consume the peer's
window), and the same reasoning applies to a live reconnect: the peer could not have
answered messages that were outstanding while it was unreachable.

* While the peer is away (`ChannelConnectivityState::Offline`) the deadline keeps
  counting, so a peer that never returns is still force-closed.
* While a reconnected peer's reestablishment handshake is in flight
  (`Syncing`) the deadline is suspended, and `refresh_peer_response_wait_v2_after_reestablish`
  restarts it when the handshake completes (both the `ChannelReady` and the
  `ShuttingDown` recovery paths).

A peer that keeps reconnecting before each window expires can therefore postpone the
force close, exactly as with the V1 revocation-nonce watchdog, which also restarts its
window on reconnect. A reconnect that never completes is still bounded by
`REESTABLISH_TIMEOUT`.

## Reestablishment cursor reconciliation

The handshake carries `next_commitment_number` = the sender's `session.own.number`
(= the sender's `remote`, which must equal our `local`), `next_ack_number` = the
sender's `local` (which must equal our `remote`), and the sender's current
`own` verification nonce. `validate_recovery_history_v2` keeps
`session.own.number == remote` and `session.remote_number == local`, so the four
recovery cases are:

| Observed | Meaning | Action |
| --- | --- | --- |
| `peer_own == local`, `peer_local == remote` | aligned | replay our outstanding CS, or answer the peer's replay |
| `peer_own == local`, `peer_local + 1 == remote` | the peer missed our ACK | replay the exact cached `incoming.response` |
| `peer_own + 1 == local` | the peer missed our newest round | the peer replays its CS; we answer from the durable `incoming` record |
| anything else | unreconcilable | reject the handshake |

Replaying a `CommitmentSignedV2` that equals `session.incoming.request` is
answered from the durable record instead of being re-verified
(`receive_commitment_v2` does this by commitment number). That is deliberate:
after a cursor advanced, rebuilding the commitment transaction from the current
state cannot reproduce the transaction the peer originally signed, so
re-verification would always fail.

Cursors alone cannot detect the remaining case. When updates cross, each peer
signs its round from a different `tlc_state`, so both persisted CSs are
individually consistent with their own signer and both fail verification on the
peer, which then stays in `reestablishing` until `REESTABLISH_TIMEOUT`. The
signer cannot see this because rebuilding from its own (unchanged) state still
matches what it signed. The current code abandons and re-signs only when its own
rebuild already differs (`outstanding_v2_cs_is_stale`), which covers part of the
case (measured: 4 of 6 runs converge, 1 of 6 before).

Two candidate fixes, both of which need a design decision because they change
what is replayed:

1. Never replay a persisted CS after a reconnect; always abandon the session and
   sign a fresh CS. Nonce-safe (`session_v2::allocate_nonce` uses a fresh random
   seed per session), but still racy: the fresh CS is also built at handshake
   time, before the peer's replayed `AddTlc` messages arrive.
2. Add an explicit "peer replay complete" marker to reestablishment and only
   sign or replay after it, so both peers apply each other's replayed updates
   first. This is the only variant that removes the view divergence, and it is a
   wire-protocol addition.

Until one of them lands, do not treat a V2 channel that completes
reestablishment as proof that the crossed-round state was reconciled.
A minimal experiment on the failing state shows that both peers hold identical
`TlcInfo.created_at` values while the status-derived commitment sets differ, so
the fix is to derive commitment membership from the durable counters instead of
the mutable status ladder. Neither the marker nor "always re-sign" addresses
that, and MuSig2 forbids re-signing a different message with the same nonce, so
recovery must make the message reproducible. Details, the experiment output, the
MuSig2 constraints and the comparison with BOLT 2 retransmission are in
[`channel-v2-reestablish-replay.md`](./channel-v2-reestablish-replay.md).
