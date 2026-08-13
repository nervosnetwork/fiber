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
