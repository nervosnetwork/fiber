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
