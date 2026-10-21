# RPC subscriptions

Fiber exposes CKB-style `subscribe(topic, options)` and `unsubscribe(id)` over
WebSocket on the existing RPC listener. Notifications use method `subscribe` and
carry `params.subscription` plus a typed `params.result`. HTTP remains available
for ordinary RPCs and rejects subscriptions. The implementation uses jsonrpsee;
there is no separate SSE endpoint or signer session takeover protocol.

## Topics and authorization

| Topic | Read policy | State scope |
| --- | --- | --- |
| `channel_signing` | `get_channel_signing_status` | Host or authenticated tenant store |
| `watchtower_signing` | `get_watchtower_signing_status` | Authorized watchtower `node(...)` |

Each topic is available only when its corresponding RPC module is enabled;
watchtower also requires the build feature. A topic reuses the complete existing
read policy, including the watchtower policy's alternative capabilities.
The current implementation provides these two signing topics. Future channel,
payment, and invoice topics must define their own query projection, authorization,
state notifications, and recovery semantics.

Native clients authenticate the WebSocket upgrade with the same
`Authorization: Bearer <Biscuit>` header used by ordinary RPC requests. Browser
clients need an adapter capable of supplying this header; putting tokens into URL
parameters is not supported. A connection keeps one credential; reconnect to use
a refreshed credential.

Tenant and node identity come from verified token authority facts. Attenuation
blocks cannot select a different identity. Tenant subscriptions resolve cold stores
without starting or pinning tenant runtimes. A watchtower tenant token must also
match the registered tenant's watchtower identity. A non-tenant token follows
ordinary host query semantics and does not gain access to all tenant stores.
Without RPC authentication, local subscriptions use the host scope.

Authorization is checked at subscription creation and before enqueueing each
notification, including retries after backpressure. Idle subscriptions recheck
at most every ten seconds. Expired or otherwise invalid credentials terminate
the subscription. Data already sent to the connection cannot be recalled.

## Wire format

```json
{"jsonrpc":"2.0","id":1,"method":"subscribe","params":["channel_signing",{"include_snapshot":true}]}
```

```json
{"jsonrpc":"2.0","id":1,"result":"0xabc"}
```

The result is an opaque subscription ID; its encoding is controlled by jsonrpsee.
Updates use the existing query result types:

```json
{"jsonrpc":"2.0","method":"subscribe","params":{"subscription":"0xabc","result":{"type":"channel_signing","data":{"channel_id":"0x...","status":{"type":"NoSignatureRequired"}}}}}
```

`watchtower_signing` uses the same envelope with type `watchtower_signing` and
`GetWatchtowerSigningStatusResult` as data. The initial snapshot ends with:

```json
{"jsonrpc":"2.0","method":"subscribe","params":{"subscription":"0xabc","result":{"type":"snapshot_complete"}}}
```

A channel removed from the subscribed scope produces
`{"type":"removed","data":{"channel_id":"0x..."}}`.

```json
{"jsonrpc":"2.0","id":2,"method":"unsubscribe","params":["0xabc"]}
```

Cancellation returns a boolean. IDs belong to their creating connection; another
connection cannot cancel them. Cancellation remains available after token expiry.
Multiple subscribers for the same tenant are independent.

## Filters and snapshots

`options` is optional. `channel_ids` restricts the subscription to at most 256
channels. Omitting it selects all authorized channels; an empty list selects none.
Unknown options, including `tenant_id` and `node_id`, are rejected. A channel
outside the authorized scope and a missing channel both produce `channel not found`.

`include_snapshot` defaults to false. With true, the server sends currently
pending signing requests followed by `snapshot_complete`. It then sends changes
to signing status, including transitions to no pending request. Unchanged states
are deduplicated. Intermediate transitions may be coalesced: this is a current-state
subscription, not an event journal.

```text
persist channel/watchtower state
             |
             v
notify the affected namespace/node
             |
             v
subscription reloads authorized state
             |
             v
query projection -> bounded connection queue -> WebSocket
```

The listener is registered before reading the initial state. Changes during the
snapshot remain pending and cause another state read afterwards. This avoids a
snapshot/listen gap but does not create a globally atomic snapshot across resources.

Subscriptions end on disconnect, unsubscribe, authorization failure, or shutdown.
Reconnect and subscribe with `include_snapshot: true` to recover pending work.
Deduplicate signing requests by request ID and submit signatures using the
existing RPCs, whose authorization and verification remain authoritative. There
is no historical cursor, durable event log, or exactly-once guarantee.

The signing agent combines the two topic snapshots into one local
`snapshot_complete`, retries failures with jittered backoff, and reloads pending
work when processing fails. It stops after credentials are rejected. The SDK's
persisted verification, nonce handling, and signing policy remain in force.

## Resource bounds

Each listener allows 16,384 active general subscriptions, with at most 32
subscriptions per WebSocket connection. jsonrpsee bounds the connection output
queue to 64 messages. Each subscription has its own task and a coalesced wakeup;
a queue that stays full for five seconds terminates that subscription so it can
resynchronize. Idle namespace registrations are removed after their last reader
exits. WebSocket ping/pong detects inactive peers.

The CCH-only `subscribe_store_changes` interface remains separate and retains its
`internal("store_changes")` permission. Raw store changes, including preimages,
are not exposed as general subscription payloads. Tenant namespaced stores still
do not inherit the host/CCH watcher.
