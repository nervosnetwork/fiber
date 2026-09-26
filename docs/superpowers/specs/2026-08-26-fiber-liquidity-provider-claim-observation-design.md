# Fiber Liquidity Provider Claim Observation Design

## Status

Approved design for provider-side observation of a client Loop Out claim.

## Context

The in-process CKB Loop Out E2E now reaches these states:

- the provider payout is committed;
- the client Fiber payment settles;
- the client claim passes CKB VM verification and commits;
- the client reaches `Success`;
- the provider remains in `PaymentSettled`.

The provider cannot use the existing transaction tracer because it does not know
the independently constructed client claim transaction hash. It only knows the
payout outpoint being spent. `GetLiveCell(None)` is not sufficient because it
cannot distinguish a committed claim from a refund, reorg, missing parent, or
unknown spend.

`LiquidityActorMessage::ProviderClaimObserved` and the corresponding state
transition already exist, but no production component sends the message.
Provider restart recovery also incorrectly calls `watch_claim`, which requires a
provider-local claim transaction record that does not exist.

## Goals

- Detect a committed transaction that spends an exact watched outpoint.
- Confirm the spend is the liquidity-lock claim path, not refund or an unknown
  spend, before marking provider success.
- Discover claims committed before watcher registration or during provider
  downtime.
- Deduplicate registration and duplicate observations.
- Reuse the capability outside liquidity when another subsystem needs exact
  outpoint-spend observation.
- Keep the existing transaction-hash tracer unchanged.

## Non-Goals

- Do not infer success from live-cell disappearance alone.
- Do not require the client to use one canonical claim transaction hash.
- Do not add Fiber p2p messages or send the claim hash from client to provider.
- Do not redesign CKB finality or add terminal-state rollback after a later
  reorg. The configured confirmation depth remains the finality boundary.
- Do not implement refund policy changes in this slice.

## Architecture

### 1. Generic outpoint-spend tracer

Add a linked CKB outpoint-spend tracing actor, parallel to the existing
transaction-hash tracing actor.

Each registration contains:

- the exact `packed::OutPoint` to watch;
- the exact lock script used to query the CKB indexer;
- required confirmation depth;
- a callback receiving the committed spending transaction and watched input
  index.

`CkbChainMessage` gains registration and removal messages for outpoint-spend
tracers. Registrations are deduplicated by exact outpoint. Re-registering an
identical watch is harmless; conflicting metadata for the same outpoint fails
descriptively.

### 2. Production discovery

The tracer uses the existing CKB indexer pattern already used by channel shutdown
and watchtower scanning:

1. query transactions by exact lock script;
2. consider input entries only;
3. load candidate transactions;
4. require committed transaction status and the configured confirmations;
5. inspect every input and require
   `input.previous_output == watched_outpoint`;
6. return the full committed spending transaction and matching input index.

Script search is only an index narrowing mechanism. Exact outpoint comparison is
mandatory because multiple cells may share the same lock script.

Registration performs an immediate historical scan before entering normal
polling. This closes the race where the claim commits before registration and
enables restart recovery.

Indexer lag is retryable. Missing candidates do not terminate the watch.

### 3. Mock-chain behavior

The shared mock backend records committed input spenders while holding the same
critical section used for VM verification and state mutation.

On successful commit it:

- records each consumed outpoint and its spending transaction;
- updates cells and transaction status atomically;
- notifies matching outpoint-spend tracers.

Rejected transactions consume nothing, create nothing, and never produce a spend
observation. Registration first checks committed-spender history, then subscribes
and rechecks under synchronization so no notification can be lost.

### 4. Liquidity claim validation

`LoopOutChainAdapter` gains a provider-specific operation such as
`watch_provider_claim(swap_id, myself)`.

`CkbLiquidityChainWatcher` loads the provider swap and quote, obtains the payout
outpoint, derives the expected liquidity-lock script, and registers the generic
outpoint-spend tracer.

Before sending `ProviderClaimObserved`, its callback validates:

- the committed transaction spends the exact payout outpoint;
- the witness corresponding to the watched input selects the claim branch;
- the witness contains a 32-byte preimage;
- `HashAlgorithm::CkbHash.hash(preimage) == swap.payment_hash`;
- the spend is not the refund branch.

CKB VM commitment proves contract validity. Explicit witness classification is
still required so a valid refund spend cannot be mislabeled as a client claim.
Malformed or unknown spends fail closed and produce diagnostic logging without
marking success.

### 5. Provider lifecycle

After provider Loop Out payment settlement:

```text
PaymentSettled -> register provider claim watcher
committed validated claim -> ClaimPending -> Success
```

The actor uses its existing watched-claim deduplication set. No synthetic claim
transaction record is created on the provider because the provider did not build
or broadcast the client transaction.

`mark_provider_claim_observed` becomes idempotent for an already successful
provider Loop Out swap. It remains strict for wrong roles, directions, and
conflicting terminal states.

### 6. Recovery

Provider Loop Out recovery becomes role-specific:

- `PaymentSettled`: register `watch_provider_claim`;
- `ClaimPending`: register `watch_provider_claim` again, allowing historical
  discovery after a crash between transitions;
- client Loop Out `PaymentSettled`/`ClaimPending`: retain existing local claim
  broadcast/watch behavior.

Recovery must not call `watch_claim` for provider Loop Out because there is no
provider-local `LiquidityChainTxRole::Claim` record.

## Error Handling

- Pending/rejected/unconfirmed spend: no observation.
- Refund witness: no success; refund handling remains a separate flow.
- Malformed witness or wrong preimage: fail closed and log the exact reason.
- Indexer error or lag: retry within the tracer polling policy.
- Duplicate committed claim observation: no additional transition or store
  mutation.
- Conflicting spender for an already resolved outpoint: report an invariant
  violation.

## Testing

### CKB tracer tests

- pending spender does not notify;
- rejected spender does not notify and leaves the watched cell live;
- committed spender notifies with exact transaction and input index;
- a transaction spending another cell with the same lock script does not notify;
- registration after commit discovers the historical spender;
- registration/commit race cannot lose the observation;
- duplicate registration/notification is idempotent.

### Liquidity tests

- valid claim witness sends exactly one `ProviderClaimObserved`;
- wrong preimage and refund witness do not mark provider success;
- provider `PaymentSettled` starts the watcher;
- provider restart from `PaymentSettled` and `ClaimPending` resumes historical
  observation;
- client recovery remains unchanged;
- duplicate observation when already `Success` is harmless.

### Integration

The existing `liquidity_ckb_loop_out_e2e` remains the acceptance test:

1. payout pending: no Fiber payment and unchanged balances;
2. commit payout: payment settles and client broadcasts claim;
3. commit claim through real CKB VM verification;
4. client and provider both reach `Success` through public RPC;
5. no test sends `ProviderClaimObserved` directly.

## Completion Criteria

- The generic tracer passes committed, rejected, historical, race, and exact
  outpoint tests.
- Provider claim classification rejects refund/wrong-preimage spends.
- Provider recovery succeeds after the claim committed during downtime.
- `liquidity_ckb_loop_out_e2e` passes without synthetic actor messages.
- Full liquidity, RPC, migration, formatting, and clippy checks pass.
