# Checked SDK test agent

The agent exercises the production SDK validation path. It uses Manual policy,
then explicitly confirms a successfully prepared request. It never infers payment
approval or opening terms from an LSP signing response.

The agent consumes `GET /lsp/events` with its existing tenant Bearer token.
Channel and watchtower requests pass through the same SDK verification and
confirmation path, then the existing JSON-RPC signature submission methods.
There is no periodic signing-status poll in the running agent. `--once` processes
one initial pending snapshot and exits.

The reader reconnects after EOF, transport errors, or failed event processing,
using exponential backoff (500 ms to 30 s) with jitter and `Retry-After` support.
Creating a reader takes over the tenant's signer stream. The server returns an
`X-Lsp-Session-Id`, which the reader reuses on all reconnects, including the
five-minute connection expiry. Another reader for the same tenant replaces that
session; the old reader stops on HTTP 409 rather than taking it back. Rejected
credentials (401/403) also stop retries. Sessions are local to the RPC listener
and are re-established after server restart; they do not revoke signature
submission permissions.
A connection must stay healthy for 30 s before backoff resets. Every reconnect
receives persisted pending requests again; the SDK retains nonce/signature safety
state. Unknown event types are ignored. A 60 s read timeout detects stalled
streams, and dropping the reader cancels both the connection and retries.

Before servicing a channel, the local fixture driver must:

1. Send the original `OpenChannelWithExternalFundingParams` to `POST /open-channel` as
   `{ "params": ... }`, setting `pubkey` to the LSP public node, `public: false`,
   and the client-owned `external_channel_signer`, plus explicit commitment delay
   and fee rate. The agent calls the SDK opening entrypoint, which saves intent, sends
   `open_tenant_channel`, checks live funding inputs through
   `--ckb-rpc`, and verifies the response using local dev-chain binaries from
   `--contracts-dir`. It returns the opening result only after SDK verification and persistence.
2. There is no `/bind` endpoint or driver-supplied commitment baseline.
3. Use `open_channel` to register the test's exact payment/invoice authorizations,
   fulfillment preimages when settling TLCs, and close or announcement approvals.
   These authorizations and the verified channel history persist in the SDK store.

The test-only `POST /authorize` control endpoint records driver intent:

- `Invoice`: `channel_id`, SDK `PaymentAuthorization` in `terms`, and the
  wallet-held `preimage`. Calls `authorize_invoice` before the LSP sees a payment.
- `ReleasePreimage`: `channel_id`, `payment_hash`, `preimage`, and `target`
  (`Invoice` or `Watchtower`). Calls SDK `authorize_preimage_release` with the
  independent chain source, then immediately invokes `settle_invoice` or
  `create_preimage`. SDK validation failure sends no preimage to the LSP.
- `Close`: `channel_id`, `local_script`, `remote_script`, `local_fee_rate`,
  `remote_fee_rate`. The fixture computes exact plain-CKB close fees from those
  scripts and rates and calls SDK `authorize_close`. An automatically accepting
  peer contributes zero fees; the initiator uses the explicitly selected rate.
- `Watchtower`: `channel_id`, `destination`, `max_fee` (integer shannons).
  This persisted fixture policy permits plain-CKB sponsor inputs locked to the
  approved destination and caps the total fee. It does not approve arbitrary
  destinations or copy a signing request into an authorization.

The command-line agent obtains live cells, canonical committed ancestors, input
maturity and median time from the independent `--ckb-rpc` endpoint. Dependencies
come from dev-chain genesis, with funding/commitment/auth binaries checked against
`--contracts-dir`. Commitment ancestry matches the signed template after checking
the broadcast-time funding dependencies, which are excluded from Fiber’s signing
message. The adapter supports this plain-CKB fixture; unsupported
relative timestamp inputs and histories over 128 ancestors fail closed.
It builds exact `OnchainSpendAuthorization` within the local policy, then uses
SDK preparation and confirmation, both of which recheck chain evidence.
`GET /status` includes accepted watchtower request IDs and key purposes per
channel, plus commitment references with complete SDK-verified revocation
signatures. Settlement scenarios assert SDK submissions; the preimage scenario
requires a TLC-key submission, and the revocation scenario requires complete
evidence for the old peer commitment before broadcasting it.

The inbound fixtures create hold invoices from wallet-owned terms; they never
give the LSP a preimage before SDK recovery checks pass. Outbound payment control
is not implemented. Nonce reuse is reported as a review warning and does not block signing policy.

The Rust fixtures construct valid commitment locks, participant nonces and peer
partial signatures. The watchtower restart test first signs a complete local
commitment, reopens the file-backed SDK store, and spends that commitment with
verified input/witness/output data. It also checks that missing evidence and a
spent input produce no submission and leave the persisted snapshot unchanged.
