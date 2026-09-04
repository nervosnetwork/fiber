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

After `open_channel_with_external_funding` returns a frozen unsigned funding
transaction, call `ChannelSigner::bind_from_approved_funding` with that
transaction, the cells the wallet agreed to spend, and the shutdown script from
the open request. Later `ChannelSigner::prepare` checks every signing request
against that approved funding identity. The node does not supply a bindable
channel identity.

RPC clients should convert `get_channel_signing_status` and
`get_watchtower_signing_status` through `fiber_lsp_sdk::json` (`json` feature,
on by default). Production clients then drive [`HostedSession`]: feed each
RPC result in, inspect [`ProcessOutcome`], and POST the returned submit
params. The session performs no HTTP. Auto-approving poll loops stay in
`tests/fiber-lsp-sdk-agent` and must not be copied into production.

The wallet's RPC transport uses the tenant Biscuit returned by
`HostedSession::finish_registration` with the standard Fiber data-plane
methods. It calls `new_invoice` with an optional `lsp_buffer_duration_ms` and
reads `accepted_lsp_buffer_duration_ms` from `InvoiceResult`; the Node adds
Public T's trampoline hint and registers the hosted invoice. It calls the
standard `send_payment` method for outbound payments.

After a successful `new_invoice`, record `InvoiceResult.invoice.data.payment_hash`
with `HostedSession::registry_mut().record_issued_invoice`. After starting an
outbound payment, record `GetPaymentCommandResult.payment_hash` with
`record_outbound_payment`. `SigningPolicy::Auto` uses this client-owned registry
to reject commitment snapshots containing unknown inbound or outbound TLCs.
The application must persist this wallet payment registry alongside its
`HostedSessionState`; the signer store intentionally contains signing-safety
material rather than wallet invoice and payment history.

[`HostedSession::new`] defaults to [`SigningPolicy::Auto`]. Call
[`SigningPolicy::decide`] yourself only if you are not using `HostedSession`.
`Always` exists only under `test-apis`. Production clients use `Auto`
(inbound invoices this client issued, the snapshot hashes into the
commitment lock args, and local balance does not fall) or `Manual`. Auto
will not trust a node-supplied `local_amount` unless that snapshot is
committed by the unsigned transaction. Settlement, TLC, cooperative close,
and announcement requests always require confirmation under `Auto`.

Signing is deliberately split into review and approval. The node supplies typed
plaintext, never a caller-computed digest. `ChannelSigner::prepare` computes the
Fiber digest and returns a `SigningReview` plus the exact typed content. Calling
`ChannelSigner::sign(prepared)` represents user or policy approval of those
exact bytes.

The ordinary signer store contains the root public key and per-channel
allocation entropy. It also stores only signing-safety context: observed local
and remote commitment numbers, published nonce slots, and hashes of signed
content/messages. It never stores the root secret, funding key, TLC key,
commitment seed, MuSig2 base nonce, balances, or TLC state. Tenant routing is
deliberately outside the SDK. Reopening a signer requires both the same root key
and its serialized store.
