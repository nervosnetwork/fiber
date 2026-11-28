# Fiber Deterministic Simulation Testing

`fiber-dst` runs real Fiber channel and payment actors over Turmoil TCP and
virtual time. Each node has its own runtime and SQLite directory. Nodes share
mock CKB state, and each node's access to the chain can fail independently.

## Run and replay

`make dst` (also `make simulate`) builds once and runs `random --chaos`
with a fresh seed each time. It saves a log per run under `target/dst/`
and prints a replay command for each failing seed before continuing.
Press Ctrl-C to stop. Set `DST_RUNS` for a finite sweep, which exits nonzero
if any seed fails. Use `DST_PAYMENTS` and `DST_PHASES` to change the workload,
and `CARGO_TARGET_DIR` to relocate the build and logs.

```sh
make dst
make simulate DST_RUNS=10 DST_PAYMENTS=32 DST_PHASES=5
```

```sh
cargo run --locked --manifest-path tests/fiber-dst/Cargo.toml -- random
cargo run --locked --manifest-path tests/fiber-dst/Cargo.toml -- random 42 --payments 128 --phases 8
cargo run --locked --manifest-path tests/fiber-dst/Cargo.toml -- random 6 --payments 32 --phases 5 --chaos
cargo nextest run --locked --manifest-path tests/fiber-dst/Cargo.toml
```

Each `random` invocation runs one simulation. Omit the seed to generate a new
one. The command prints the seed and workload arguments to stderr before
starting. Replay with the same build, seed, and arguments. Fixed scenarios
run through nextest, alongside cross-process replay checks for pinned random
seeds. CI runs the DST suite and the SQLite fault-isolation tests.

## Architecture

```text
random CLI / fixed test
          |
          v
Driver (scenario.rs)       topology, workload, fault policy, assertions
          |
          v
Harness (harness.rs)       step the world, read persisted state, collect hits
          |
          v
Turmoil world              one process, 5 ms virtual ticks
  +-- Fiber A host  <----- simulated TCP -----> Fiber B host
  |   Tokio + Ractor + SQLite                  Tokio + Ractor + SQLite
  |         |                                         |
  |         +------- per-caller CKB adapters ---------+
  |                           |
  |                       shared mock CKB
  |
  +-- optional external Watchtower hosts
      own runtime and SQLite, JSON-RPC over simulated TCP to Fiber,
      independent access to the same mock CKB

  +-- optional external `fiber-lsp-sdk` wallet hosts (`SDK-0`..`SDK-3`)
      persisted `RootSigner`/`HostedSession` on disk, Biscuit-authenticated
      JSON-RPC over simulated TCP to a hosted `LspService` node,
      injectable RPC duplicate / dropped-response / delay / crash / partition faults
```

The driver sends commands through the normal actor mailboxes. On each step it
advances the selected fault schedule, then asks Turmoil to process network
traffic and tick each running host's single-threaded Tokio runtime. Tokio
polls the node's Ractor actors and background tasks. The harness then collects
fault receipts and delivers signed gossip updates in a stable batch. A host
crash cancels that host's tasks. Restart opens the same SQLite directory, and
the shared chain survives.

The peer adapter carries framed Fiber messages over Turmoil TCP. Signed
gossip passes through the harness relay, outside the peer-link fault windows.
SQLite uses real databases with node-local VFS fault injection. The shared
in-memory chain verifies spending transactions against CKB scripts. The
settlement model supplies synthetic block time and confirmations along with
input maturity and live-cell queries. See [scenario.rs](src/scenario.rs),
[harness.rs](src/harness.rs), [host.rs](src/host.rs), and [tower.rs](src/tower.rs).

## Fixed scenarios and checks

Random exploration and the fixed scenarios share the harness and fault
adapters. Random exploration and the four-node swarm also share the
payment-wave driver and settlement auditor.

| Fixed test scenario | What it checks |
| --- | --- |
| `crash-before-ack` | The receiver crashes after committing and before sending its ACK. The hot cache agrees with SQLite, the immutable record is unchanged, and reopening SQLite recovers the exact committed records. The payment settles once after recovery and a new payment succeeds. |
| `faults` | A CKB outage, slow funding query, and a request timeout, SQLite I/O errors and disk full, refused TCP dial and read/write resets, a full outbound queue, a slow actor mailbox, and a single TLC frame delayed by seven virtual seconds. Each fault must hit and the channel must recover. |
| Two-node faults | Fixed seed 1729 selects a mix of direct payments, node crashes, partitions, and CKB, SQLite, TCP, outbound queue, and actor mailbox faults. The checker tracks balance bounds before settlement, mirrored channel state, and cleanup. |
| Diamond payments | Seed 42 exercises concurrent bidirectional multihop and MPP on a four-node diamond, with direct payments mixed in. Seeded crashes, partitions and network actor delays overlap the workload. Once the faults end, the auditor checks every invoice and its per-hop TLCs before issuing recovery probes. |
| `multihop` | A relay crashes after persisting downstream fulfillment and before relaying it upstream. The checker verifies the amount received, both channel deltas, and the relay fee. |
| `mpp` | A diamond topology forces a signed invoice onto two paths. One relay crashes while a shard is in flight. The invoice and both unique shards must settle, with exact channel balances and no TLCs left. |
| Watchtower settlement | Force-closes a channel with a committed hold-invoice TLC. A Watchtower mailbox delay, indexer outage, actor restart, and settlement submission timeout precede on-chain fulfillment. The test checks the settlement witness, invoice and payment states, and both wallet payouts. The external fixture also partitions the RPC link and completes settlement while the recipient is down. |
| Superseded watchtower round | Delays a bidirectional `CommitmentSignedV2` update and reconnects with a second round variant while dropping `RevokeAndAckV2`, forcing the Watchtower to retain `superseded_remote_settlements` and settle a force-closed superseded commitment in both built-in and external modes. |
| `lsp-offline-delivery` | Registers an external `fiber-lsp-sdk` wallet over Biscuit-authenticated JSON-RPC (with duplicate `lsp_register_tenant` replay rejected), crashes the wallet and evicts the cold tenant so a public trampoline payment buffers as `Deferred`, crashes and restarts the LSP, verifies replay rejection persists across restart, recovers and settles with duplicate `settle_invoice`, and then holds the wallet offline past `buffer_deadline` to verify clean expiry. |
| `lsp-signer-faults` | Crashes the external `fiber-lsp-sdk` wallet mid-`AwaitingSignature`, restarts from persisted `sdk_store.bin` and `session_state.json`, drops the RPC response after commit on `submit_channel_signature`, injects duplicate and conflicting signature replays, then force-closes the private channel while the wallet is offline so the returning wallet registers the preimage via `create_preimage` and completes self-funded `ExternalFundingRequired` Watchtower settlement with duplicate `submit_watchtower_signature`. |
| Funding signature replay | Drops the final funding signatures and restarts the first signer. The peer must replay the same signatures before Ready, and a payment must settle on the original funding outpoint. |
| Funding abort recovery | Drops a funding frame, verifies that both endpoints abort and clear funding state, and retries the open. |
| Channel ID rewrite | Injects an SQLite sync error in the rewrite and, in a separate run, crashes after commit before notifying the network actor. SQLite reopens with exactly one complete channel ID and matching indexes. The original attempt either becomes usable or aborts cleanly before a new channel settles a payment. |
| Interrupted funding cleanup | Fails a SQLite write while removing an aborted channel with three online siblings. A cold read checks rollback. After restart the target's records and indexes are gone, and a payment settles on each sibling's outpoint. |
| Parallel funding | Six channels fund concurrently on one peer link. A second fixture drops one channel's `TxUpdate` before opening its siblings, observes the two abort receipts across reconnection, and probes every surviving channel by outpoint. |

The SQLite adapter injects real SQLite result codes at VFS write and sync
callbacks. A one-shot error can target a node's next VFS operation or restrict
delivery to a write batch containing matching put and delete key prefixes.
When both prefixes are supplied, both operations must appear in the same batch.
The driver counts a hit when SQLite receives the error. Continuous payment
chaos can sample a targeted write fault during an early direct payment and
records whether the batch was hit. The channel checker reads both durable records
in one snapshot and treats a missing half as an invariant violation.

The queue budget models resource pressure at the simulation send
boundary. Chain outages hold requests until access is restored. Route setup
relays signed channel advertisements between the simulated nodes' graphs.

`fnn::simulation::slow_mailbox(&actor.get_cell(), duration)` delays each
ordinary message before its handler runs, using the target actor's virtual
clock. It targets a live instance of the simulated network, channel, payment,
mock CKB, transaction-tracing, or Watchtower actors. Use `GetChannelActor` to
select a specific channel on a specific node. Ractor's unbounded mailbox continues
accepting messages, so a burst can accumulate behind the delayed handler.
Dropping the returned guard clears the delay for subsequent messages. A
restarted actor needs a new rule. The directed scenario queues four requests
on one channel actor, checks that the local network actor and the peer channel
both respond before its first reply, and settles a payment under the slowdown.

The swarm runs a healthy baseline, a mixed workload with separately scheduled
faults, and a healthy recovery probe. The auditor starts from invoice intent
and initial channel balances. It observes each endpoint's TLC fulfillment
after the SQLite commit, checks balances during the run, and joins the two
endpoints' receipts by channel, offering node, and TLC ID. After recovery it
follows each successful attempt from the payer through relay links to the
recipient and checks shard amounts, relay fees, invoice status, commitments,
and released liquidity. An ordinary workload payment may fail under faults.
The auditor requires a consistent outcome, including agreement between the
payer and invoice, and the recovery probes must succeed. In-flight work must
drain within the virtual time budget. Applied failed TLC records can remain
until the next commitment.

## Random exploration

`random` generates a six-to-eight-node topology with a diamond core, co-hosts
`LspService` on one of the core relay nodes alongside two external
`fiber-lsp-sdk` wallet hosts (`SDK-0`, `SDK-1`), funds both the public mesh and
the private hosted channels, and runs a mix of direct, multihop, MPP, and
trampoline/hosted-tenant payments. The default is 128 public orders over eight
waves plus interleaved LSP/SDK rounds, followed by four recovery probes and an
on-chain settlement probe. The default CLI policy also runs four baseline
payments on the core, covering bidirectional multihop and MPP before expansion.

| Policy | When and where it injects faults |
| --- | --- |
| `random` without a flag | Directed boot and funding faults, followed by per-wave node crashes, link partitions, and network mailbox delays. A seed picks a 25%, 50%, or 75% chance for each candidate window. The fixed diamond test keeps all three candidates. |
| `random --chaos` | The continuous scheduler starts at boot and continues through funding, overlapping payment waves, and the hosted LSP/SDK workload (long-offline buffer expiries, abrupt wallet crashes mid-signing, LSP crashes during `Deferred` delivery, SDK-LSP partitions, dropped requests, dropped responses after commit, delays, and duplicate RPCs). This is the policy used by `make dst`. |

The continuous scheduler samples node crashes, link partitions, TCP
disconnects, and whole peer-frame drops or delays. It can select an in-flight
relay and crash a node at a protocol checkpoint. During funding, the
open/accept exchange for a tracked channel has a protected window, and an
opening gets a quiet period if it has yet to converge. Broader fault adapter
coverage, including SQLite error codes, refused TCP dials, socket I/O errors,
and writer backpressure, is exercised by fixed scenarios.

One third of `--chaos` seeds select one expansion link for a batch of two to
six concurrent openings. The seed chooses direction, submission gaps, and
whether to drop one channel's `TxUpdate`, delay a node's CKB signing, or run
the continuous node and link fault scheduler during funding. The batch checks
each channel against its own funding lock and outpoint. After recovery it
sends an explicit-route payment on every surviving channel.

The continuous policies can submit the next payment wave while the previous
one is still in flight. A frame-loss window resets the link on recovery if a
frame was dropped.
Seeded checkpoints can crash a node after persisting `AwaitingChannelReady`
or a payment commitment, or after persisting a relay fulfillment.

One in eight seeds is eligible for a long peer-frame delay. The wave policy
selects a seeded wave and prefers the direction of a direct payment. The
continuous policy samples eligible link-fault slots until it arms one delay.
The selected frame waits 5–10 virtual seconds before the TCP write. Later
frames on the same stream wait behind it. Other streams can progress. The
trace records the selected direction and the frame when the rule hits.

## Watchtower and on-chain settlement

Even seeds run built-in Watchtower actors in the Fiber hosts with the node's
SQLite store. Odd seeds give each Fiber node an external Watchtower on a
separate Turmoil host with its own SQLite store. Random runs enable the
towers before opening channels so they can collect commitment snapshots.

When a funding output is spent during a payment wave, the driver triggers
regular Watchtower and channel shutdown checks. The auditor checks that both
endpoints agree on the close transaction, the commitment and its successor
cells are spent, and any TLC settlement receipts match those spends. The
wave finishes after payments and their attempts have drained.

If a wave reaches its drain budget after faults have healed, the driver can
advance the virtual Unix clock past the next pending on-chain TLC expiry
from a matching commitment snapshot. Fiber and simulated CKB share that
clock. Turmoil's elapsed time and fault schedule continue on the same ticks.

After the workload and recovery probes pass, every `random` policy commits a
held payment on a seeded online channel and force-closes it on the shared
mock CKB. Both tower modes exercise the real scanner and transaction builder,
with a slow mailbox, CKB indexer outage, actor restart, and settlement
submission timeout.

The external mode sends real Watchtower JSON-RPC methods over framed Turmoil
TCP. Once both towers persist the commitment snapshot, it drops the RPC link
and reveals the preimage. The `create_preimage` call must fail before the
driver repairs the link and resubmits `SettleInvoice`. The external store must
then contain the preimage. The recipient crashes before settlement and
remains down until the external tower records a verified TLC witness and all
commitment cells are spent. After restart, the simulation's read-only chain
observer verifies settlement witnesses in the Fiber store. Both modes check
payer and invoice status and the CKB payouts to both wallets.

## Reading results

The final `random reconciled` line reports audited orders, successful
orders, fault overlap, peak concurrency, and recovery probes. In the window
policy `fault_overlaps` counts waves. In the continuous policies it records
whether any fault overlapped an in-flight payment in the combined workload.

The trace includes the build revision, virtual timestamps, frame
fingerprints, fault receipts, and settled state. Failed runs include the
assertion and preceding events. The replay suite compares complete traces
from independent processes. Routing orders inbound channels by liquidity,
capacity, and update time, using the funding outpoint to break ties.
Set `RUST_LOG` to enable actor logs on stderr.
