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
the open request. The intent-specific `prepare_*` methods validate signing requests
against that approved funding identity and locally authorized state. The node does not supply a bindable
channel identity.

RPC clients should convert `get_channel_signing_status` and
`get_watchtower_signing_status` through `fiber_lsp_sdk::json` (`json` feature,
on by default). Production clients then drive [`HostedSession`]: feed each
RPC result in, inspect [`ProcessOutcome`], and POST the returned submit
params. The session performs no HTTP. Auto-approving poll loops stay in
`tests/fiber-lsp-sdk-agent` and must not be copied into production.

The wallet's RPC transport uses the tenant Biscuit returned by
`HostedSession::finish_registration` with the standard Fiber data-plane
methods. It calls `new_invoice`; the Node adds Public T's trampoline hint and
registers the hosted invoice using the LSP service's buffer policy. It calls the
standard `send_payment` method for outbound payments.

## Client-side verification flow

```text
Wallet-approved funding, opening balances, keys, scripts and fees
                              |
                 Bind funding + approve opening terms
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
             Verify nonce purpose, counter, participants,
             aggregate nonce and required peer partial
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
