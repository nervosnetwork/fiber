# Fiber Liquidity Cross-Node Negotiation And E2E Design

## Summary

This phase closes the production negotiation gap between separate liquidity client
and provider nodes, then adds two complementary test layers:

- in-process integration tests using real Fiber networking, real liquidity actors,
  RocksDB stores, and the CKB testtool-backed mock chain;
- Bruno tests using separate FNN processes, HTTP JSON-RPC, and a real CKB dev chain.

Tests must use public RPC boundaries. They must not copy provider quote records
directly into a client store.

## Goals

- Replace the fixed provider test key with the running node's Fiber public key.
- Define a complete, serializable quote envelope suitable for transfer between
  independent nodes.
- Add an idempotent client-side quote import RPC with strict validation.
- Make provider acceptance idempotent without mutating quoted terms.
- Persist signed liquidity transactions so every non-terminal state can recover
  after restart.
- Expose liquidity chain transaction records through public RPC.
- Add liquidity-lock to the in-process mock-chain and CKB dev-chain fixtures.
- Cover CKB and UDT Loop Out and Loop In happy paths, validation failures,
  refunds, idempotence, and recovery.
- Make failures diagnosable with bounded polling and relevant state/transaction
  output.

## Non-Goals

- Do not add provider discovery, quote gossip, reputation, or quote comparison.
- Do not sign quote envelopes in this phase. The directly configured provider
  HTTP endpoint is the trust boundary; signatures remain M9 scope.
- Do not add automated liquidity planning or execution.
- Do not replace existing focused unit tests with E2E tests.
- Do not make every edge case a Bruno test when deterministic in-process coverage
  provides the same assurance.

## 1. Cross-Node Quote Negotiation

### 1.1 Provider Identity

`LiquidityActorArguments` receives the local Fiber `Pubkey`, and
`LiquidityActorState` retains it. Production RPC startup derives the value from
`FiberConfig::public_key()` and passes it when spawning the actor.

Provider quote handlers use this public key. The existing deterministic provider
key is removed from production paths and retained only in test fixtures that
construct actors directly.

### 1.2 Quote Envelope

Add `LiquidityQuoteEnvelope` to `fiber-json-types`. It is a complete transport
representation of `LoopOutQuoteTerms` and includes:

- `quote_id`, `swap_kind`, and `provider_pubkey`;
- full asset information, including the exact UDT type script when applicable;
- amount, provider fee, routing fee limit, on-chain fee estimate, and capacity;
- payment hash, expiry, payout deadline, and refund lock time, all using the
  same units and values as the persisted quote terms;
- claimant and refund scripts encoded as Molecule script bytes in `0x` hex;
- the client invoice for Loop In.

Provider-facing quote RPCs return the envelope. Existing concise response fields
may be retained only if they remain a projection of the same persisted terms; the
envelope is the canonical value transferred to another node.

For Loop Out, `provider_quote_loop_out` receives the final claimant and refund
scripts and persists them before returning the envelope. The envelope is therefore
immutable and complete before client import. `provider_accept_loop_out` accepts
only `quote_id`; it never adds or changes quote terms.

`expires_at` and `payout_deadline` are Unix timestamps in milliseconds.
`refund_after_lock_time` is the exact encoded CKB `since` value placed on the
refund input: an absolute `SinceType::Timestamp` whose payload is Unix seconds.
Quote creation rounds the target Unix milliseconds up to seconds before encoding.
Tests decode the metric and absolute flag, then advance chain median time rather
than treating it as a wall-clock delay.

### 1.3 Import RPC

Add `import_liquidity_quote` with:

- `quote: LiquidityQuoteEnvelope`;
- `max_provider_fee`;
- `max_routing_fee`.

The client validates before persistence:

1. expiry is in the future;
2. direction is supported;
3. provider and routing fees fit the supplied caps;
4. asset kind and UDT type-script presence are consistent;
5. amount, fee, and gross calculations do not overflow;
6. claimant/refund scripts decode as valid Molecule scripts;
7. Loop In invoice payment hash, amount, and UDT type match the envelope;
8. quote ID is not already bound to different terms.

Import is idempotent. An identical existing envelope succeeds without mutation;
the same quote ID with different terms fails and never overwrites persisted data.
Loop Out envelopes require final claimant/refund scripts and no client invoice.
Loop In envelopes require the client invoice and preserve the internal payout
deadline value so envelope-to-terms conversion is lossless.

### 1.4 Manual Cross-Node Flows

Loop Out:

```text
client -> provider: provider_quote_loop_out(final claimant/refund scripts)
provider -> client: quote envelope
client -> client: import_liquidity_quote
client -> provider: provider_accept_loop_out(quote ID)
client -> client: loop_out
```

Loop In:

```text
client creates invoice
client -> provider: quote_loop_in
provider -> client: quote envelope
client -> client: import_liquidity_quote
client -> client: loop_in
client -> provider: provider_accept_loop_in(lock tx hash, output index)
```

These calls may initially be orchestrated by the test/client operator. Internal
remote HTTP fetching remains out of scope.

Provider acceptance is idempotent. Repeating an acceptance for a quote that has
already created its provider swap returns the existing swap and performs no new
payment, transaction build, broadcast, reservation, or store mutation.

### 1.5 Live Cell Data Validation

The current `CkbChainActor::GetLiveCell` response only preserves `CellOutput`,
which cannot validate a UDT amount because the amount is encoded in cell data.
Replace the response with a focused domain value containing both:

```rust
pub struct LiveCell {
    pub output: packed::CellOutput,
    pub data: packed::Bytes,
}
```

`validate_observed_loop_in_lock` validates:

- CKB capacity satisfies the quote capacity requirement and the lock amount;
- UDT type script exactly matches the quote asset;
- UDT data is exactly 16 bytes and decodes as little-endian `u128`;
- decoded UDT amount equals `amount + provider_fee`;
- missing, extra, or malformed UDT data fails before provider payment.

### 1.6 Provider Mode Management RPC

Add `set_liquidity_provider_mode({ enabled: bool })`. It delegates through the
liquidity actor to `LiquidityStore::set_provider_mode` and returns the updated
`LiquidityProviderStatus`.

Setting the current value again succeeds without additional side effects.
Disabling provider mode gates new provider quote and accept requests but does not
stop recovery for already-persisted non-terminal swaps.

### 1.7 Durable Transactions And Public Observability

Every locally built payout and Loop In lock transaction is stored as signed
serialized bytes before its first broadcast, under a dedicated store key
(`LiquidityChainTxSignedTx`) so the persisted `LiquidityChainTxRecord` bincode
shape is unchanged and no data migration is required. Recovery decodes the bytes,
verifies that the transaction hash equals the persisted record, and only then
rebroadcasts instead of failing closed. Missing, malformed, or hash-mismatched
bytes fail closed with a descriptive reason. Claim and refund transactions are
deterministic secp256k1 rebuilds already hash-verified against their record, so
they are not persisted as signed bytes.

Add `list_liquidity_chain_transactions({ swap_id })`. It returns all persisted
transactions for that swap with:

- transaction role;
- transaction hash;
- created outpoint, when present;
- lifecycle status;
- failure reason, when present;
- creation and update timestamps.

Results use a stable role order. Unknown swaps return an empty list. This RPC is
the public source for E2E ordering, confirmation, rejection, refund, restart, and
diagnostic assertions; tests do not read liquidity stores directly.

Chain transaction roles are reported with a distinct semantic label in the query
response: `payout`, `loop_in_lock`, `claim`, and `refund`. The stored internal role
enum remains `{Payout, Claim, Refund}`; a Loop In lock is labeled `loop_in_lock`
by the actor from the owning swap's direction, avoiding a stored-enum change and a
data migration.

## 2. In-Process Integration Test Layer

### 2.1 Infrastructure

Add the liquidity-lock binary to `tests/deploy/contracts/` and deploy it from
`MockContext::new()` as `Contract::LiquidityLock`, including its cell dep.

Extend the test-node fixture, or add a focused `LiquidityNetworkNode` wrapper,
that composes:

- `NetworkNode` and real TCP Fiber networking;
- an established payment channel;
- real `LiquidityActor` and `NetworkLoopOutPaymentAdapter`;
- RocksDB `Store`;
- real `CkbLiquidityChainWatcher` connected to `MockChainActor`;
- public RPC module calls matching the production HTTP boundary.

Helpers may create invoices, call liquidity RPCs, submit/confirm chain
transactions, restart liquidity actors, and poll swap states. Helpers must not
reach into a peer's store to transfer quotes.

The fixture owns explicit handles for the RPC server, liquidity actor, chain
actor, network actor, and background tasks. Restart helpers await old actor/task
termination before reopening the same store, so only one runtime can mutate a
node's liquidity state.

The controllable mock chain separates transaction submission from resolution:
submission records `Pending`; `commit` verifies the transaction, atomically
consumes inputs and creates outputs, then notifies tracers; `reject` creates no
outputs, consumes no inputs, records the reason, and notifies tracers. Pending and
rejected outputs are never returned as live cells.

### 2.2 Test Matrix

Loop Out:

- CKB and UDT happy paths reach `Success` on both nodes.
- No Fiber payment before payout confirmation.
- Payment failure reaches provider refund recovery.
- Wrong preimage is rejected by the contract.
- Success requires claim confirmation.
- Duplicate accept and duplicate confirmation are idempotent.
- Rejected payout transaction records its reason and recovery rebroadcasts the
  same hash from persisted signed bytes without rebuilding it.
- Restart recovery covers every reachable Loop Out non-terminal state, including
  `Created`, `Quoted`, and safe
  rebroadcast from persisted signed transaction bytes.

Loop In:

- CKB and UDT happy paths reach `Success` on both nodes.
- Observed-output validation succeeds for matching cells.
- Payment hash, claimant lock, refund lock, amount, refund time, and asset type
  mismatches each fail before provider payment.
- No provider payment before lock confirmation.
- Payment failure reaches client refund recovery.
- Success requires provider claim confirmation.
- Duplicate quote import and accept are idempotent.
- Restart recovery covers every reachable Loop In non-terminal state, including
  `Created`, `Quoted`, and safe
  rebroadcast from persisted signed transaction bytes.

Provider and quote administration:

- envelope carries the actual provider node public key;
- import accepts identical terms and rejects conflicting terms;
- expired quotes and fee-cap violations fail;
- disabled provider mode gates every provider quote/accept path;
- add, update, disable, list, and status asset RPCs reflect durable state;
- disabled assets cannot be quoted.

## 3. Bruno Real-Process E2E Layer

### 3.1 Dev-Chain Fixture

Add the liquidity-lock binary to the deployment assets and make it resolvable by
`ContractsContext`. Update dev-chain/node configuration so both FNN processes can
load its script and cell deps. Enable the liquidity RPC module for the participating
nodes and configure independent data directories, keys, ports, and asset registries.

The deployment process must be reproducible from repository scripts; tests must
not depend on manually deployed cells.

Restart tests use a dedicated process supervisor that owns individual CKB and FNN
PIDs. It stops and restarts only the selected node with the same data directory,
uses traps to clean up owned processes, and does not rely on broad `pkill` patterns
or the generic all-node monitor.

### 3.2 Bruno Suites

Bruno drives only public HTTP JSON-RPC methods across separate processes. Suites
cover:

1. CKB Loop Out happy path;
2. CKB Loop In happy path, including real `GetLiveCell` validation;
3. UDT Loop Out happy path;
4. UDT Loop In happy path;
5. observed-output mismatch rejection before payment;
6. refund after the real lock time;
7. provider-disabled RPC rejection;
8. process restart with non-terminal swap recovery;
9. quote envelope transfer and import across nodes;
10. final `get_swap`/`list_swaps` state consistent with chain transaction state.

Malformed observed-output cases use a checked-in transaction-builder helper to
create and sign exact lock/type/data mutations. Bruno submits the resulting
transaction through public CKB JSON-RPC and passes its mined outpoint to the
provider RPC.

## 4. Isolation, Diagnostics, And Reliability

- Each suite uses unique invoices, quote IDs, and transactions.
- Each Bruno suite explicitly sets provider mode and its asset registry instead
  of depending on prior suites.
- State waiting uses bounded polling with descriptive timeout failures; fixed long
  sleeps are prohibited.
- Test cleanup stops child processes and preserves logs on failure.
- Failure output includes client/provider swap records, liquidity chain tx
  records, relevant transaction hashes, and both node logs.
- Tests that mutate shared chain time or restart processes run serially through
  nextest test groups or the Bruno suite runner.

## 5. Completion Criteria

The E2E phase is complete only when:

- all in-process liquidity integration tests pass repeatedly;
- Bruno CKB and UDT happy paths pass in both directions;
- at least one real-process refund case and one restart-recovery case pass;
- cross-node tests use only public RPC and never copy stores;
- provider identity in every envelope matches the serving node;
- every locally built payout/Loop In lock can be recovered from persisted signed
  bytes, and claim/refund recover deterministically with hash verification;
- public chain-transaction records agree with CKB transaction/live-cell state;
- generated RPC documentation and migration checks are current;
- CI commands, prerequisites, timeouts, and failure artifacts are documented.

## 6. Implementation Order

1. Finalize Loop Out terms at quote time and add real provider identity plus quote
   envelope/import RPC.
2. Make provider acceptance idempotent.
3. Extend `GetLiveCell` with cell data and complete CKB/UDT validation.
4. Add the provider-mode management RPC.
5. Persist signed transaction bytes and add the chain-transaction query RPC.
6. Add liquidity-lock to test deployment fixtures.
7. Build the in-process liquidity network fixture and happy paths.
8. Add in-process validation, failure, refund, idempotence, and all-state recovery.
9. Configure real dev-chain liquidity nodes and add Bruno happy paths.
10. Add Bruno mismatch, refund, and restart cases.
11. Run full verification, review flaky-test behavior, and document execution.
