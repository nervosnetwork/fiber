# fiber-lsp-sdk

Portable Rust SDK for hosted Fiber clients and signer-owned channel keys. It
has no dependency on the Fiber node runtime, actor transport, or a concrete
database, and compiles for both native targets and `wasm32-unknown-unknown`.

```rust
use fiber_lsp_sdk::{MemoryStore, RootKey, RootSigner, SignerError};

# async fn example() -> Result<(), SignerError> {
let store = MemoryStore::default();

// New signer: persist the returned backup outside the ordinary signer store.
let created = RootSigner::create_random(store.clone()).await?;
let root_key_backup = created.root_key_backup.expose_secret();
let channel = created.root_signer.create_channel().await?;
let channel_key_id = channel.channel_key_id();

// Existing signer: restore the store, then supply the same root key.
let snapshot = store.snapshot().expect("snapshot memory store");
let restored_store = MemoryStore::from_snapshot(&snapshot).expect("restore memory store");
let root_signer = RootSigner::open(RootKey::import(root_key_backup)?, restored_store).await?;
let _identity = root_signer.identity_public_key();
let _channel_signer = root_signer.open_channel(channel_key_id).await?;
# Ok(())
# }
```

Applications provide durable storage by implementing `SignerStore`. The SDK
owns record encoding and only asks the backend to read, replace, delete, or
atomically compare-and-swap opaque byte values. An IndexedDB implementation can
therefore live in a browser integration crate without adding browser
dependencies here.

Call `HostedSession::open_tenant_channel` with trusted network configuration,
a `TenantOpeningRpc` transport, an independent `FundingVerifier`, and durable
`OpeningPersistence`. The SDK saves intent before sending the RPC, verifies the
wallet transaction and opening context, and persists the baseline and binding
before returning. Retry the same request after interruption. There is no separate
approval or bind RPC. The transport calls the standard
`open_channel_with_external_funding` method with the tenant token; tenant
responses include the required `opening_context`, which the SDK validates.

For external channel signing this response is a frozen proposal, not permission
to sign or broadcast wallet inputs. Bind it first, drive the initial channel
signing requests, and require an independently verified local recovery record
with a complete commitment signature before releasing the wallet signature.
The test agent's `/open-channel` endpoint enforces this ordering.

A session can bind multiple channels. Allocate a fresh pending channel signer
with `allocate_pending_channel` for each additional opening and supply that
signer's public material. Retry an interrupted opening with its original intent;
changing amount or asset while reusing the same signer is rejected before RPC.

For registration, call `HostedSession::begin_registration` with the trusted LSP
node ID, a fresh random 32-byte request ID and the current Unix time in
milliseconds. Send the proof to the hosting provider. The provider holds the
operator credential and calls Fiber's existing `lsp_register_tenant` RPC, then
returns the result to the application. Store that result with
`finish_registration` and persist `state()` using the existing storage adapter.
A consumed request cannot be retried; generate a fresh proof after a lost response.

Tenant tokens expire after the node's configured `lsp.token_ttl_secs` (default
2592000 seconds, or 30 days). Registration returns `expires_at_ms` as hexadecimal
Unix milliseconds; the SDK saves this beside the token in the session state.

The application checks `session.is_token_near_expiry(now_ms, advance_ms)` with
its chosen advance window, for example 24 hours for a 30-day token. Expired,
missing or incomplete credentials also return true. The method only reads the
saved metadata and performs no I/O.

After obtaining a replacement from the hosting provider, call
`session.replace_token(token, expires_at_ms)`. This updates only the token and
expiry, retaining channel bindings, pending opening state and signer keys.
Empty tokens or zero expiry are rejected without changing the current state.
The application owns persistence, renewal requests and scheduling; the SDK does
not request replacement credentials or start background tasks. Persist the
updated `state()` using the existing storage adapter and update transport
credentials. Reconnect credential-bound WebSockets when convenient. Older tokens
remain valid until their own expiry; expiry does not terminate an established
subscription.

RPC clients can subscribe to `channel_signing` and `watchtower_signing` over
WebSocket, or query `get_channel_signing_status` and
`get_watchtower_signing_status` through `fiber_lsp_sdk::json` (`json` feature,
on by default). Production clients then drive [`HostedSession`]: feed each
query or subscription result in, inspect [`ProcessOutcome`], and POST the returned submit
params. The session uses injected transport for opening and has no HTTP dependency. Auto-approving subscription loops stay in
`tests/fiber-lsp-sdk-agent` and must not be copied into production.

The wallet's RPC transport uses the tenant Biscuit returned by
`HostedSession::finish_registration` with the standard Fiber data-plane
methods. It calls `new_invoice`; the Node adds Public T's trampoline hint and
registers the hosted invoice using the LSP service's buffer policy. It calls the
standard `send_payment` method for outbound payments.

## V2-only nonce ownership

Only the V2 channel protocol is supported. Unreleased hosted-LSP signer data
from older prototypes is not compatible: use a new signer store and channel;
never restore a V1 wire path or derive a nonce from a commitment number alone.

Opening material includes `v2_nonces`, a pool of fresh SDK-generated public
nonces. Follow-up material replenishes that pool and supplies indexed
`commitment_points`. Persist the secret seeds in the SDK store before exposing
public material. The node stores public allocations only; external channels
never fall back to node-held funding/TLC keys or secret nonces.

Every signing request carries `content.v2`, binding the channel ID, commitment
owner and exact allocated public nonce, in addition to the slot's number and
purpose. The signer verifies the binding, key order, aggregate nonce and peer
partial signature against the persisted request before signing. Commitment,
revocation, announcement and cooperative close (`Closing`) use separate nonce
domains. An exact retry may return its durable receipt; a changed signing
context for an allocated nonce must fail closed, including after restart.
Pool exhaustion also fails closed: do not recycle a nonce or synthesize local
secret material to continue. Keep follow-up material available while driving
signing requests; availability while a signer is offline is not guaranteed.

## Client-side verification flow

```text
Locally saved opening intent + pending signer (no channel ID yet)
                              |
              open_channel_with_external_funding
                              |
             Frozen funding transaction + opening_context
                              |
          Verify local intent, keys, channel ID, asset, reserves,
          fee, trusted contracts and wallet-approved funding inputs
                              |
          Persist verified commitment baseline + funding binding
                              |
                 Register payment / invoice authorizations
                              |
Untrusted LSP request --------> Decode + bind to local channel
                              |
                  Select mandatory prepare_* verifier
                              |
       +----------------------+-----------------------+
       |                      |                       |
  Commitment              Revocation            Close / Announcement
       |                      |                       |
  Funding input,         Known old state +       Locally approved terms
  contract, asset,       exact successor         + exact output amounts,
  fees, balances         in correct lane         fees / announcement
       |                      |                       |
  TLC authorization,     Penalty output,         No live TLCs and equal
  ids, derived keys,     version ordering,      lane balances for close
  expiry, resolution     recovery window              |
       |                      |                       |
  Both commitment        Complete successor           |
  lanes and exact        proof before revoking        |
  balance transition     our previous state           |
       |                      |                       |
       +----------------------+-----------------------+
                              |
             Verify V2 channel + owner + fresh public nonce,
             purpose, number, participants, aggregate nonce
             and required peer partial
                              |
                     Validated PreparedSigning
                              |
                 Auto policy / explicit confirmation
                 (Auto: verified commitment updates
                  without outbound changes, after opening)
                              |
                 Recheck revision and applicable time limits
                              |
                 Sign + verify complete peer proof, if present
                              |
                 CAS: persist validated state, recovery proof
                 and signing history before returning signature
                              |
                        Submit to LSP

Any validation failure ------> Reject; no returned signature or store mutation
```

```text
On-chain settlement / TLC spend
  LSP transaction + local spending approval + stored commitment
                              |
  Independent ChainVerifier: live inputs, lineage, maturity, time
                              |
  Verify witness, signing key, preimage / timeout, residual state,
  destination, asset conservation and exact fee
                              |
  Explicit confirmation -> recheck chain + revision -> persist + return signature

Preimage release
  Authorized invoice + incoming TLC in complete local recovery record
                              |
  Verify live funding, unrevoked state, preimage and remaining recovery window
                              |
  Persist release / fulfillment intent -> application may release preimage
                              |
  Later commitment: verify TLC removal and exact balance increase
```

## Watchtower Quotas, Fee-Hold Limits, and Recovery

The LSP watchtower enforces per-tenant and global resource limits to protect operator storage and fee UTXOs. Hitting a limit **never corrupts or deletes existing watched channels or signing state**, but failing to handle it can leave newly opened channels or invoices unprotected, or delay on-chain settlement.

`fiber-lsp-sdk` provides policy and callback hooks so applications can react safely without embedding transport or background loops in the SDK:

- [`WatchtowerLimitPolicy`]: defaults to `FailClosed` (recommended). `PruneAndRetry` allows pruning closed channels or settled/expired preimages and retrying once for **tenant-scoped** limits, while still failing closed on **global** limits.
- [`WatchtowerEventHandler`]: receives `on_quota_exceeded(&WatchtowerQuotaError, WatchtowerQuotaAction)` and `on_status_update(&WatchtowerHealthStatus)`.
- [`HostedSession::handle_watchtower_status_result`]: evaluates `GetWatchtowerSigningStatusResult` (`NoSignatureRequired`, `SignatureRequired { request_id, content, expires_at_ms }`, or `Backoff { retry_after_ms, consecutive_expired_requests }`), notifies `WatchtowerEventHandler`, and returns `(WatchtowerHealthStatus, ProcessOutcome)`.
- [`HostedSession::handle_watchtower_rpc_error`]: classifies watchtower RPC quota errors and invokes `on_quota_exceeded` with the recommended [`WatchtowerQuotaAction`].

### 1. Watchtower channel quota (`create_watch_channel`)

- **Defaults**: `64` watched channels per tenant, `4096` globally (configurable by the LSP operator).
- **Behavior when exceeded**: `create_watch_channel` returns an `InvalidParams` JSON-RPC error containing `quota exceeded`. Updates to already-watched channels (`update_revocation`, `update_local_settlement`, `update_pending_remote_settlement`) and removals (`remove_watch_channel`) are not blocked.
- **Safe client handling**:
  1. **Fail closed by default (`WatchtowerLimitPolicy::FailClosed`)**: never ignore the error or fund/use a channel while assuming watchtower protection is active.
  2. **Tenant cap (`WatchtowerQuotaScope::Tenant`)**: if using `WatchtowerLimitPolicy::PruneAndRetry`, call `remove_watch_channel` only for channels that are already closed and finalized on-chain, then retry `create_watch_channel` once; if it still fails, fail closed.
  3. **Global cap (`WatchtowerQuotaScope::Global`)**: always fail closed and surface an operator-capacity error to the user.

### 2. Watchtower preimage quota (`create_preimage` & hosted invoices)

- **Defaults**: `256` watched preimages per tenant, `16384` globally.
- **Behavior when exceeded**:
  - Direct `create_preimage` RPC calls fail with `InvalidParams` (`quota exceeded`).
  - For hosted tenants creating invoices via `new_invoice`, the asynchronous `PreimageCreated` forwarding skips watchtower registration with a server warning rather than failing `new_invoice`.
- **Safe client handling**:
  1. **Prune proactively**: call `remove_preimage` (and clean up expired or already-settled invoices) as part of normal wallet housekeeping so the active preimage count stays well below the per-tenant cap.
  2. **Retry before sharing an invoice**: if `create_preimage` fails with `WatchtowerQuotaScope::Tenant`, prune stale preimages and retry before handing the invoice to a payer; on `WatchtowerQuotaScope::Global` or repeated failure, fail closed rather than risking an unclaimed inbound TLC during an offline force-close.

### 3. External signing fee-hold caps, 30-minute TTL, and per-node backoff

- **Limits**:
  - Concurrent active fee-holding requests (`pending_requests + signed_signatures`): **4 per channel**, **16 per tenant (`node_id`)**, **64 globally**.
  - **Non-renewing 30-minute TTL**: each pending or signed-but-unconfirmed request holds operator fee cells for at most 30 minutes (`WATCHTOWER_HOLD_MS`) from creation.
  - **Per-node exponential backoff**: if a request expires unsigned or fails CKB broadcast (excluding `-1107` already-in-mempool), the watchtower evicts it, releases the fee cells, and pauses new fee-holding requests across **all channels of that `node_id`** starting at 5 minutes and doubling up to 60 minutes. A successful broadcast resets the channel's backoff state.
- **Status feedback & safe client handling**:
  1. **Feed full status results into `HostedSession::handle_watchtower_status_result`**: inspect the reported [`WatchtowerHealthStatus`] (`Healthy`, `PendingSignature { expires_at_ms, remaining_ms, .. }`, or `InBackoff { retry_after_ms, remaining_backoff_ms, consecutive_expired_requests }`).
  2. **Sign promptly within TTL**: keep `watchtower_signing` subscriptions (or `get_watchtower_signing_status` polling) responsive and submit verified signatures well before `expires_at_ms`.
  3. **Verify before signing**: only submit signatures that pass `HostedSession` / `ChainVerifier` checks; repeatedly submitting invalid or unspendable transactions triggers per-node backoff across all of the tenant's channels.
  4. **Respect backoff**: when `WatchtowerHealthStatus::InBackoff` is reported, avoid tight-loop polling and wait until `retry_after_ms`.
  5. **Emergency self-broadcast**: check `health.needs_self_broadcast(dispute_deadline_ms, now_ms, safety_margin_ms)`. When `true`, construct and broadcast the settlement transaction directly from the locally persisted recovery record using the wallet's own fee inputs.

### 4. Input and witness bounds

- **TLC cap**: at most **254** TLCs per settlement snapshot (`MAX_WITNESS_TLC_COUNT = 0xfe`).
- **Script / output data cap**: at most **1024 bytes** for `funding_udt_type_script` args, `payout_lock_script` args, and `revocation_data.output_data`.
- Clients should enforce these bounds locally before sending watchtower RPCs.
