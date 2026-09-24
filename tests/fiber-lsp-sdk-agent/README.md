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

Watchtower signing requires `poll_watchtower_verified(channel_id, authorization,
chain)`. The caller supplies locally approved `OnchainSpendAuthorization` and an
independent live `ChainVerifier`; preparation and confirmation both check the
chain. The plain polling loop rejects a pending watchtower signature without this
context. The command-line agent does not yet wire a production chain adapter or
payment-approval control endpoint; existing Bruno drivers must supply those
integrations before using it for a payment/watchtower scenario. Do not restore
unchecked signing to make a driver proceed.

The Rust fixtures construct valid commitment locks, participant nonces and peer
partial signatures. The watchtower restart test first signs a complete local
commitment, reopens the file-backed SDK store, and spends that commitment with
verified input/witness/output data. It also checks that missing evidence and a
spent input produce no submission and leave the persisted snapshot unchanged.
