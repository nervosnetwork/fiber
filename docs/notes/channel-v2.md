# Channel V2 capability and durable close

The node capability `CHANNEL_V2` uses the existing required/optional feature pair
at bits 6/7. It combines independent commitment-owner nonce sessions with full
32-byte on-chain payment hashes. The commitment contract feature byte remains
`1`; released legacy channels with byte `0` continue to use V1 messages and their
original contract layout. `CommitmentContractFeatures` describes the on-chain
layout; its `is_v2()` accessor selects the node protocol.

Cooperative close uses fresh `NoncePurposeV2::Closing` nonces, advertised in
`ShutdownV2`. The durable closing session retains both shutdown advertisements,
the original transaction, the exact signing context and both partial responses.
Pending TLCs must finish acknowledgement and balance application before the
closing transaction is fixed. Reconnection replays these records, without
reactivating commitment or revocation nonces. Conflicting advertisements and
signatures are rejected. Force close supersedes cooperative publication and uses
the latest actual usable commitment transaction.

The closing session also retains the original peer-wait start time. Missing
remote ShutdownV2 and missing ClosingSignedV2 after local signing are outstanding
peer obligations with a bounded watchdog, including after reconciliation finishes.
Duplicate messages and reconnects do not extend that deadline. Local TLC draining
and on-chain confirmation after both signatures are distinct from peer waits.

Outgoing CS intents retain their original ACK wait start too. A genuine outstanding
RAA remains a peer obligation during shutdown draining even after both shutdown
advertisements arrive. The watchdog uses the earliest outstanding CS-ACK/close
deadline. Actor startup rearms it before any reconnect or handshake, and periodic
maintenance also checks it while the peer stays offline. Restart, duplicate traffic,
and reconciliation do not replace these durable timestamps. Quarantined V2 Stale,
local-only signing/draining, and both-signature on-chain waits do not authorize a
peer-timeout publication.

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

Current preflight uses the backend's existing paged prefix iterator (100 records
per page), so retained closed histories are not all materialized at once. Every
record is still validated before migration writes/version stamping. Usable V2
commitments require exactly one canonical 112-byte funding witness: the 16-byte
XUDT-compatible prefix, aggregate key, and aggregate signature. Production and
validation use the same encoder.

Deterministic schema/store samples have three variants: minimal V1, fully
populated V1, and genuine V2 bootstrap. Each variant has a distinct deterministic
channel ID so storing one cannot overwrite another.
