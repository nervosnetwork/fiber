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

// Existing signer: reopen against the same durable store and root key.
let root_signer = RootSigner::open(RootKey::import(root_key_backup)?, store).await?;
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

> **Warning (no state rollback)**: Never snapshot and roll back a signer store
> in production. Restoring stale signer state can reuse a V2 MuSig2 nonce and
> leak the channel funding private key. `MemoryStore::snapshot` and
> `MemoryStore::from_snapshot` are gated behind the `test-apis` feature for
> restart-recovery tests only.

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
If the LSP definitively rejects an opening and you need to retry with modified
parameters, call `abandon_pending_channel_opening` to retire the pending signer
and clear the rejected intent before allocating a fresh signer.

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

## Watchtower Quotas, Request Limits, and Recovery

The LSP watchtower enforces per-tenant and global resource limits to protect operator storage and RPC resources. Hitting a limit **never corrupts or deletes existing watched channels or signing state**, but failing to handle it can leave newly opened channels or invoices unprotected, or delay on-chain settlement.

`fiber-lsp-sdk` provides policy and callback hooks so applications can react safely without embedding transport or background loops in the SDK:

- [`WatchtowerLimitPolicy`]: defaults to `FailClosed` (recommended). `PruneAndRetry` allows pruning closed channels or settled/expired preimages and retrying once for **tenant-scoped** limits, while still failing closed on **global** limits.
- [`WatchtowerEventHandler`]: receives `on_quota_exceeded(&WatchtowerQuotaError, WatchtowerQuotaAction)` and `on_status_update(&WatchtowerHealthStatus)`.
- [`HostedSession::handle_watchtower_status_result`]: evaluates `GetWatchtowerSigningStatusResult` (`NoSignatureRequired`, `ExternalFundingRequired { request_id, content, expires_at_ms, required_expiry_since }`, or `Backoff { retry_after_ms, consecutive_expired_requests }`), notifies `WatchtowerEventHandler`, and when supplied with a wallet-funded transaction + `OnchainSpendAuthorization` validates `ExternalFundingRequired` against trusted `ChainVerifier` (or call [`HostedSession::handle_external_funding_watchtower_status`] directly).
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

### 3. External signing request caps, 30-minute TTL, and per-node backoff

- **Limits**:
  - Concurrent active external signing requests (`pending_requests + signed_signatures`): **4 per channel**, **16 per tenant (`node_id`)**, **64 globally**.
  - **Non-renewing 30-minute TTL**: each pending or signed-but-unconfirmed request expires after 30 minutes (`WATCHTOWER_HOLD_MS`) from creation.
  - **Per-node exponential backoff**: if a request expires unsigned or fails CKB broadcast (excluding `-1107` already-in-mempool), the watchtower evicts it and pauses new signing requests across **all channels of that `node_id`** starting at 5 minutes and doubling up to 60 minutes. A successful broadcast resets the channel's backoff state.
- **Status feedback & safe client handling**:
  1. **Handle every watchtower status variant**: inspect `WatchtowerHealthStatus::Healthy`, `PendingExternalFunding`, and `InBackoff`; for `WatchtowerSigningStatus::ExternalFundingRequired`, set the payout lock and fund/adjust the template transaction in the tenant wallet and call [`HostedSession::handle_external_funding_watchtower_status`] (or pass the funded transaction into [`HostedSession::handle_watchtower_status_result`]).
  2. **Sign promptly within TTL**: keep `watchtower_signing` subscriptions (or `get_watchtower_signing_status` polling) responsive and submit verified signatures well before `expires_at_ms`.
  3. **Verify before signing**: only submit signatures that pass `HostedSession` / `ChainVerifier` checks; repeatedly submitting invalid or unspendable transactions triggers per-node backoff across all of the tenant's channels.
  4. **Respect backoff**: when `WatchtowerHealthStatus::InBackoff` is reported, avoid tight-loop polling and wait until `retry_after_ms`.
  5. **Emergency self-broadcast**: check `health.needs_self_broadcast(dispute_deadline_ms, now_ms, safety_margin_ms)`. When `true`, construct and broadcast the settlement transaction directly from the locally persisted recovery record using the wallet's own fee inputs.

### 4. Input and witness bounds & externally funded settlements (`ExternalFundingRequired`)

- **TLC cap**: at most **254** TLCs per settlement snapshot (`MAX_WITNESS_TLC_COUNT = 0xfe`).
- **Script / output data cap**: at most **1024 bytes** for `funding_udt_type_script` args and `revocation_data.output_data`.
- **Externally funded settlements (`WatchtowerSigningStatus::ExternalFundingRequired`)**: to protect LSP operators from fee-draining and UTXO-griefing attacks, an externally signed watchtower channel **never spends operator fees or subsidizes tenant cell capacity**. For **both CKB and UDT** settlements (balance unlocks and TLC unlocks), the watchtower emits an **unfunded template transaction** (`WatchtowerSigningStatus::ExternalFundingRequired { request_id, content, expires_at_ms, required_expiry_since }`, surfaced on `WatchtowerHealthStatus::PendingExternalFunding`) containing only `inputs[0]` (commitment cell), the settlement outputs (`[residual_commitment_output, payout_output]`, or `[payout_output]` when both parties are settled, with a placeholder payout lock that the client wallet replaces with its own `destination` lock), and `witness[0]` (commitment witness placeholder):
  - **When external wallet CKB is needed (IMPORTANT for wallet developers)**:
    - **CKB balance / CKB preimage TLC / high-excess UDT balance unlocks (`required_expiry_since == None` and payout CKB exceeds occupied capacity + fee)**: the wallet can set the payout lock and deduct the transaction fee directly from the payout output (`inputs.len() == 1`, zero external wallet CKB needed), or optionally attach a wallet funding input.
    - **Timeout TLCs (`required_expiry_since == Some(...)`), UDT TLC unlocks, and low-excess partial UDT balance unlocks**: the wallet must attach its own plain-CKB funding input (`inputs[1..]`) covering the CKB capacity shortfall (if any) plus the transaction fee (setting `since >= required_expiry_since` on a funding input for timeout TLCs). Whenever a wallet opens or holds hosted **UDT** channels (or offers outbound TLCs), **keep an unspent plain-CKB reserve in the user's wallet** — each concurrent UDT TLC settlement requires at least the new UDT output occupied capacity + change output occupied capacity + transaction fee (roughly **~204 CKB** for a standard xUDT/sUDT cell + secp256k1 lock; keep a comfortable buffer per expected concurrent UDT TLC). Because the wallet itself selects `inputs[1..]` and attaches `cell_deps` + `witness[1..]`, **any wallet lock script (secp256k1, JoyID, Omnilock, Multisig, etc.) and any wallet funding address** are supported.
  - **Wallet + SDK signing flow for `ExternalFundingRequired`**:
    1. Read the template transaction from `WatchtowerSigningStatus::ExternalFundingRequired { request_id, content, expires_at_ms, required_expiry_since }` (also surfaced on `WatchtowerHealthStatus::PendingExternalFunding`).
    2. Set the payout output's `lock` to the wallet's `destination` lock and fund fees (and any CKB capacity shortfall): either deduct `fee` directly from the payout output when `required_expiry_since.is_none()` and sufficient CKB is available, or append the wallet's `cell_deps`, funding inputs (`inputs[1..]`, with `since >= required_expiry_since` whenever `Some`), plain-CKB change output, and wallet-signed `witness[1..]`. Keep `inputs[0]`, `witness[0]`, and (when `outputs.len() >= 2`) residual commitment `outputs[0]` / `outputs_data[0]` unchanged.
    3. Pass the funded transaction along with trusted `ChainVerifier` + `OnchainSpendAuthorization` (set `fee_sponsor_lock: None` if funding from `destination` or deducting from payout, or `Some(wallet_funding_lock)` if funding from another wallet address) to `HostedSession::handle_external_funding_watchtower_status`.
    4. Call `HostedSession::confirm_external_funding_onchain(pending, &chain)` to sign `inputs[0]` and obtain `SubmitWatchtowerSignatureParams { channel_id, request_id, signature, signed_transaction: funded_tx }`.
    5. Submit the params to `submit_watchtower_signature`. The watchtower verifies the submitted transaction against the template and `signature`, injects `signature` into `witness[0]` while keeping any wallet `witness[1..]` intact, and broadcasts through its standard retry/pruning path.
- Clients should enforce these bounds locally before sending watchtower RPCs.

## Wallet Integration Security Checklist

1. **Opening order**: Never sign or broadcast `unsigned_funding_tx` immediately after `HostedSession::open_tenant_channel`. First drive the initial commitment exchange and verify `channel_signer.has_verified_opening_exit().await? == true` so the wallet holds a complete local exit transaction before locking funds in the 2-of-2 funding script.
2. **Hold invoices only**: When receiving payments via a hosted LSP, create **hold invoices** (send only `payment_hash` to the LSP; never pass `payment_preimage` upfront). Release the preimage only after `ChannelSigner::authorize_preimage_release` succeeds against an independent `ChainVerifier`.
3. **Backup & storage durability**: `RootKeyBackup` alone **cannot** recover channel funds if `SignerStore` (channel bindings, nonce state, recovery records) is lost. Never copy the test-only `FileSignerStore` into production, never roll back signer snapshots, and in multi-tab browser wallets serialize `SignerStore` + `OpeningPersistence` writes with Web Locks (`navigator.locks`).
4. **Independent chain verification & CKB reserve**: Treat LSP-supplied network parameters and watchtower health statuses as untrusted. Hardcode trusted `OpeningNetwork` contract hashes/deps in the wallet, monitor force-closes + `dispute_deadline_ms` via an independent CKB RPC / light client so `needs_self_broadcast` triggers even if a malicious LSP reports `Healthy`, keep a plain-CKB reserve in the user's wallet (~204+ CKB per concurrent UDT TLC; ~62+ CKB for timeout TLCs) for hosted settlements, and handle `WatchtowerSigningStatus::ExternalFundingRequired` via `handle_external_funding_watchtower_status` + `confirm_external_funding_onchain` (or self-broadcast as fallback).
5. **Token expiry & revocation**: In addition to polling `is_token_near_expiry`, handle RPC authentication errors (HTTP 401 on HTTP or WebSocket upgrade / revoked token) by running `begin_registration` + `replace_token` and reconnecting WebSockets.
