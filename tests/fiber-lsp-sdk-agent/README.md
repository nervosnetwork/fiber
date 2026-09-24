# Checked SDK test agent

The agent exercises the production SDK validation path. It uses Manual policy,
then explicitly confirms a successfully prepared request. It never infers payment
approval or opening terms from an LSP signing response.

Before polling a channel, the local fixture driver must:

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
`GET /status` includes accepted watchtower request IDs per channel; the settlement
E2E asserts an actual SDK submission in addition to chain settlement.

Payment/invoice/preimage control endpoints remain unwired. Payment scenarios
must supply these authorizations before signing; never restore unchecked signing
for a driver. Nonce reuse protection also remains enabled in the normal build.

The Rust fixtures construct valid commitment locks, participant nonces and peer
partial signatures. The watchtower restart test first signs a complete local
commitment, reopens the file-backed SDK store, and spends that commitment with
verified input/witness/output data. It also checks that missing evidence and a
spent input produce no submission and leave the persisted snapshot unchanged.
