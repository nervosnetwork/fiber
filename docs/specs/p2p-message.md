# Fiber Network P2P Message Protocol

Fiber uses [Molecule] messages to establish CKB payment channels, collaborate on
funding transactions, exchange payments and close channels. This release supports
**Channel V2 only**. The authoritative field definitions and wire IDs are in
[`fiber.mol`](../../crates/fiber-types/src/schema/fiber.mol).

## Compatibility and initialization

Peers exchange `Init { features, chain_hash }` before processing channel messages.
The chain genesis hash must match. Nodes advertise `CHANNEL_V2` as required
(feature bit 6); a peer must advertise support through bit 6 or its optional pair,
bit 7. A peer without V2 support is disconnected during Init, including when an
existing channel would otherwise be reestablished. There is no V1 fallback.

The commitment-lock feature byte is `1`. Opening messages must explicitly carry
`channel_features = 1`; zero and unknown feature combinations are rejected.
V2 uses 58-byte commitment-lock arguments and full 32-byte TLC payment hashes.
The configured commitment-lock contract must support this layout before rollout.

V1 wire IDs `1, 2, 5, 9, 10, 14, 15, 16, 17` are permanently reserved and rejected,
not reassigned to other messages. Remaining messages retain their existing IDs:

| ID | Message |
| --- | --- |
| 0 | Init |
| 3 | TxSignatures |
| 4 | TxUpdate |
| 6 | TxAbort |
| 7 | TxInitRBF |
| 8 | TxAckRBF |
| 11 | UpdateTlcInfo |
| 12 | AddTlc |
| 13 | RemoveTlc |
| 18 | AnnouncementSignatures |
| 19 | OpenChannelV2 |
| 20 | AcceptChannelV2 |
| 21 | CommitmentSignedV2 |
| 22 | RevokeAndAckV2 |
| 23 | ChannelReadyV2 |
| 24 | ReestablishChannelV2 |
| 25 | ShutdownV2 |
| 26 | ClosingSignedV2 |
| 27 | TxCompleteV2 |
| 28 | JitChannelRegister |

Malformed messages, including removed wire IDs, are rejected before dispatch.
The onion/trampoline packet version is independent of the channel protocol.

`JitChannelRegister` is a session-scoped message, not a channel operation. A
receiver sends it after `Init` to a peer that advertised the optional
`JIT_CHANNEL` feature (bit 9). `funding_udt_type_script` is empty for CKB;
`max_amount` is the largest inbound payment the receiver wants on a just-in-time
channel. The receiver sends `open_fee` as 0. The liquidity node ignores the
message unless JIT service is enabled. When it accepts, it sends
`JitChannelRegister` back with the same `max_amount` and `open_fee` set to its
configured fee. Silence is a rejection. If the node can fund a channel and the
fee was not configured, it rejects the registration. Explicit 0 charges nothing.
The payer pays this fee only to the invoice's `liquidity_provider`, and only when
`max_fee_amount` or `max_fee_rate` is set. A payer that sets neither does not pay a
non-zero open fee. The liquidity node requires the fee only when it has to open a
channel. A channel that can already forward the payment is paid a normal routing fee.
A higher payer fee cap does not increase the open fee. The receiver quotes the
acknowledged fee only when it is within `jit_max_open_fee`; unset accepts only 0.
The receiver auto-accepts one private two-way CKB open from that peer at a time,
only after that acknowledgement, and only when `jit_max_amount` is set and the
funder's inbound amount (the funding amount above reserved cell capacity) is within
it. Public, UDT, one-way, and externally funded opens are aborted. The liquidity
node also refuses a payment above `max_amount` or a multi-part payment. A receiver
disconnect before the funding signatures are sent abandons the open and fails the payment.
The liquidity node opens at most `jit_max_inflight_opens` channels at once (default 8)
and fails a payment that arrives while that many opens are still unconfirmed. A later
payment to the same receiver waits until the open sized for an earlier payment has
finished. It does not share that earlier funding. Once the open is ready, that balance
stays reserved for its payment until the payment finishes, and the next payment opens
its own channel.

### Timeouts and recovery

An open starts only when the remaining TLC expiry covers funding setup (3
minutes), four confirmations at an assumed 30 seconds each, two minutes after
the channel is ready, and the payment's final TLC expiry delta. Otherwise the
payment fails and no channel is opened. That budget is checked once, before the
open.

Before our funding signature is sent, the funding timeout (default 24 hours
from channel creation) aborts a stalled open. A receiver disconnect, an upstream
TLC removal, or a failed open does the same: the channel is abandoned, the
payment fails, and the in-flight slot is freed.

After our funding signature is sent, including while the channel waits to
become ready, the open stays. A broadcast funding transaction is traced until
four confirmations, a failed transaction, or a spent funding input. An
unbroadcast transaction has no JIT timer. A receiver disconnect leaves that open
and its payment in place and fails only payments queued behind it.

Removing the upstream TLC after the open can no longer be abandoned fails the
payment immediately. The channel stays tracked, occupies one
`jit_max_inflight_opens` slot, and blocks the next open to that receiver until
the channel is ready or the funding actor stops. When the channel becomes ready
after that TLC is gone, the reservation is dropped. A later payment uses the
ready channel when it can forward, and opens another channel when it cannot.

Restart restores an in-flight open, its queue, and its reservation while the
upstream TLC is still held. When that TLC is already gone, JIT tracking drops
the open. The funding channel can still confirm, and a new payment to that
receiver is refused while the channel is still opening.

A watchtower settles TLCs on-chain, including the upstream TLC on the channel
into the liquidity node. The funding transaction stays with the tracer.

## Channel establishment

The establishment flow is adapted from [BOLT 02]:

```text
Initiator                              Acceptor
    |-------- OpenChannelV2 ----------->|
    |<------- AcceptChannelV2 ---------|
    |<------ funding collaboration --->|
    |<------ CommitmentSignedV2 ------>|
    |<--------- TxSignatures --------->|
    |<-------- ChannelReadyV2 -------->|
```

`OpenChannelV2` offers the asset type, funding amount, reserved CKB, shutdown
script, fee rates, commitment delay, TLC limits and channel public keys.
`AcceptChannelV2` confirms the peer's parameters and contribution, which may be
zero. Both advertise an `initial_commitment_nonce` for their initial owner
session and explicitly confirm `channel_features = 1`.

The opening channel ID is temporary. The final channel ID is derived from the
sorted TLC basepoints of both parties. Public channels also exchange channel
announcement nonces and, once ready, `AnnouncementSignatures`.

Both parties must exchange and verify `CommitmentSignedV2` before exchanging
funding `TxSignatures`, so each holds a usable unilateral exit before funding can
be published. The lower funding contributor sends `TxSignatures` first; equal
contributions are ordered by funding public key. `ChannelReadyV2` advertises the
next commitment number and nonce. An identical Ready replay is idempotent; a
conflicting number or nonce is rejected.

External funding uses the same V2 protocol. The unsigned wallet transaction is
returned only after commitment exchange provides a durable exit. Submitted
funding witnesses and opening recovery records survive restart.

## Funding transaction collaboration

The initiator sends `TxUpdate { channel_id, tx }`. The peer responds with a new
`TxUpdate` or `TxCompleteV2`. Both parties must agree on the final transaction
before signing. `TxCompleteV2.initial_commitment_nonce` must match the advertised
bootstrap nonce; it cannot replace that session.

The full CKB transaction is exchanged, rather than individual input/output
messages. A node must reject removal or modification of its own inputs/outputs.
`TxAbort { channel_id, message }` can terminate a still-abortable funding flow;
it must not discard a potentially live, already-signed funding channel.

### Peer-added complexity limits

Verifying a peer's `tx` requires a chain lookup per peer-added input. Before any
chain lookup, reject transactions exceeding these limits relative to the local
previous funding transaction. When the local transaction is empty, the deltas are
the remote totals. Signal rejection with a `TxAbort` describing the budget.

| Limit | Maximum |
| --- | --- |
| Serialized transaction size (in block) | `133120` bytes (`1024 * (128 + 2)`, the P2P frame cap) |
| Peer-added inputs | `64` |
| Peer-added outputs | `3` (funding cell plus UDT and CKB change; CKB-only channels use two) |
| Peer-added cell deps | `8` |

`TxInitRBF` and `TxAckRBF` retain their wire definitions, but the current runtime
does not implement RBF collaboration.

## Payment operations and commitment rounds

`AddTlc` carries a channel-local TLC ID, amount, full payment hash, hash algorithm,
absolute expiry and onion packet. Each direction assigns TLC IDs starting at zero.
Only the recipient of an Add can remove it, using `RemoveTlc` with either a
preimage or an encrypted failure packet. `UpdateTlcInfo` updates forwarding policy.

V2 maintains independent commitment-owner signing sessions. `CommitmentSignedV2`
identifies the commitment number and carries its funding partial signature,
funding nonce and, outside bootstrap, revocation nonce. `RevokeAndAckV2` carries
the matching number, revocation partial signature/nonce, next commitment point
and next commitment nonce. It does not use the removed V1 nonce chain.

TLC updates and the outgoing commitment intent are persisted together before
publication. ACK processing applies settled effects durably and exactly once.
Nonce contexts are bound to their keys, aggregate nonce and transaction: a
consumed nonce must never sign a different context. See the
[V2 session and persistence notes](../notes/channel-v2.md) for signing and replay
invariants.

## Reestablishment

After reconnecting, both sides exchange:

```text
ReestablishChannelV2 {
    channel_id,
    next_commitment_number,
    next_ack_number,
    next_local_commitment_nonce,
}
```

`next_commitment_number` identifies the sender's own verification session
(`session.own.number`, the sender's remote commitment cursor).
`next_ack_number` is the sender's local commitment cursor. The receiver checks
these cursors against its durable incoming/outgoing history before accepting
replay or new updates. Outstanding updates are replayed in their original order;
retransmission must retain the original signing context or use a genuinely fresh
session, never reuse a consumed nonce for changed data.

Opening Ready, outstanding ACKs and cooperative close have their own retained
replay records. Duplicate messages do not create a new round or silently extend
peer-response deadlines. Recovery excludes offline time from its online waiting
budget and remains bounded by the existing watchdogs.

A backup-restored `Stale` channel is quarantined. Peer assertions cannot prove
freshness after rollback: they do not authorize new signatures, automatic
unquarantine or publication of a potentially revoked local commitment.

## Channel closing

A cooperative close exchanges `ShutdownV2`, drains pending TLCs, then exchanges
`ClosingSignedV2`. Shutdown advertises the close script, fee rate and an independent
closing nonce. The closing session retains the transaction, signing context and
both partial responses before publication; commitment/revocation nonces are not
reused for closing. Reconnect replays retained records rather than changing an
already-signed closing transaction.

Force close publishes the latest usable signed commitment and follows the V2
on-chain settlement layout. Quarantine and signing-context checks still apply.

## Upgrading

There is no old-channel conversion or old-database migration in this release.
Use the old binary to close all existing channels and complete settlement, stop
it, back up its data, then start the new binary with a new empty database. Do not
delete old data while it may still be needed to recover funds. See the
[upgrade instructions](../../README.md#upgrading-to-the-v2-only-release).

[BOLT 02]: https://github.com/lightning/bolts/blob/master/02-peer-protocol.md
[Molecule]: https://github.com/nervosnetwork/molecule
