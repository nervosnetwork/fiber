# Fiber Liquidity Management Design

## Summary

Fiber should add an lnd Loop-inspired liquidity management system that lets a
node move value between Fiber channels and CKB on-chain assets. The system must
support both CKB and whitelisted UDT assets from the first version.

The canonical M0 protocol spec lives at
[`docs/specs/liquidity-management.md`](../../specs/liquidity-management.md).
This design document records the milestone decomposition and earlier design
conversation; implementation planning should use the canonical protocol spec
once M0 is approved.

The first implementation should avoid changing Fiber p2p messages. Swap
negotiation should use RPC, while Fiber p2p continues to carry normal payments
and TLCs. P2P extensions are deferred until a later multi-provider market phase.

## Goals

- Support Loop Out: exchange Fiber channel balance for on-chain CKB or UDT.
- Support Loop In: exchange on-chain CKB or UDT for Fiber channel balance.
- Support both client and provider roles in the same FNN process.
- Keep provider mode optional and disabled by default.
- Use an asset whitelist for UDTs instead of accepting arbitrary type scripts.
- Persist swap state so node restarts cannot lose money or strand orders.
- Build manual swaps before automated liquidity policies.

## Non-Goals

- Do not change Fiber p2p messages in the first product milestones.
- Do not implement provider discovery or quote gossip in the first product milestones.
- Do not support arbitrary, user-supplied UDT type scripts in the first version.
- Do not merge this feature into the CCH module, although CCH patterns may be reused.
- Do not automate swaps until manual Loop In and Loop Out flows are safe and recoverable.

## Actors

- Client FNN: the node requesting liquidity movement.
- Provider FNN: an FNN with provider mode enabled that quotes and fulfills swaps.
- Fiber network: routes normal off-chain payments.
- CKB chain: hosts the on-chain CKB or UDT swap cells.

A single FNN may act as both client and provider. Client capability is always
available. Provider capability is controlled by configuration.

## Asset Model

Assets are represented as one of:

- `CKB`: native shannon amount.
- `UDT`: a whitelisted UDT identified by its type script.

Each provider maintains an asset registry. A registry entry contains:

- Stable asset identifier used by RPC and orders.
- Asset kind: CKB or UDT.
- UDT type script for UDT assets.
- Minimum and maximum swap amount.
- Provider fee policy.
- Capacity limit and currently available capacity.
- Optional per-asset operational limits.

RPC quote and swap requests reference assets by registry identifier. Clients do
not send arbitrary type scripts for swap execution. Protocol amounts use raw
integer units. Display decimals belong in UI and wallet layers, not in the swap
protocol.

For UDT swaps, CKB is still required for transaction capacity and chain fees.
Quote responses must distinguish:

- Swap amount in the swapped asset.
- Provider fee in the swapped asset.
- Routing fee budget in the swapped asset.
- On-chain fee and capacity requirements in CKB.

## High-Level Architecture

Add a new `liquidity` module rather than extending `cch`.

Suggested internal components:

- `LiquidityActor`: owns swap lifecycle orchestration.
- `AssetRegistry`: validates and exposes whitelisted CKB and UDT assets.
- `SwapClient`: implements client-side quote, execution, cancel, and refund flows.
- `SwapProvider`: implements provider quotes, capacity management, and risk checks.
- `OnchainSwapWatcher`: watches CKB cells and confirmations relevant to swaps.
- `SwapStore`: persists orders, quotes, state transitions, and recovery metadata.

The module integrates with existing systems:

- `fiber::payment` for Fiber payments.
- `invoice` for creating, tracking, settling, and canceling invoices.
- `fiber::channel` and existing RPC data for channel liquidity inspection.
- CKB transaction construction and chain monitoring for swap cell lock, claim, and refund.
- `store` for durable swap state.
- `rpc` and Biscuit auth for user and operator APIs.

## Loop Out Flow

Loop Out moves Fiber channel balance to an on-chain CKB or UDT destination.

Expected high-level flow:

1. Client requests a Loop Out quote from a provider.
2. Provider validates asset whitelist, capacity, amount, expiry, and fee policy.
3. Provider returns quote terms, payment request details, and on-chain payout terms.
4. Client pays provider through Fiber using normal payment machinery.
5. Provider completes the on-chain payout to the client's CKB address or UDT receiver.
6. Both sides record the terminal order state.

The exact hashlock and preimage ordering must be specified in the protocol spec
so neither side can claim funds without satisfying the agreed payment and timeout
conditions.

## Loop In Flow

Loop In moves on-chain CKB or UDT into Fiber channel balance.

Expected high-level flow:

1. Client requests a Loop In quote from a provider.
2. Provider validates asset whitelist, capacity, amount, expiry, and fee policy.
3. Client locks CKB or UDT on-chain into the agreed swap cell.
4. Provider watches the chain for the locked cell and required confirmations.
5. Provider pays the client through Fiber.
6. Provider claims the on-chain swap cell when the preimage is available.
7. If the flow expires before successful completion, the refund path becomes available.

The refund path must be safe across node restarts and must not depend on volatile
in-memory state.

## On-Chain Swap Primitive

The on-chain primitive must support both CKB and UDT assets.

The CKB contract/script code for this primitive belongs in the sibling
`../fiber-scripts` repository. This repository should integrate that script by
adding Fiber-side types, RPC, storage, transaction builders, watchers, and tests
that consume the deployed script artifacts.

Required properties:

- Hashlock claim path using the agreed preimage.
- Timelock refund path after expiry.
- Asset consistency checks.
- Correct handling of CKB capacity and fees.
- UDT cell validation for type script and amount.
- Claim and refund transaction builders.
- Chain watcher integration for lock, claim, refund, and expiry.

CKB swaps use native capacity as the swapped value. UDT swaps use a UDT cell for
the swapped amount, while CKB capacity and fees remain separate operational
requirements.

## Persistence And Recovery

All non-terminal swaps must survive node restarts.

Persisted state should include:

- Swap order identity and role.
- Asset identifier and raw amount.
- Quote terms and expiry.
- Payment hash and relevant invoice data.
- On-chain outpoint or expected cell descriptor when known.
- Current state and state transition history.
- Refund and claim deadlines.
- Last observed chain and payment status.

On startup, FNN must resume watching pending swaps and continue any safe retry,
claim, or refund action. State transitions must be validated so recovery cannot
move an order into an impossible state.

## RPC Surface

Initial RPCs should support manual operation:

- `quote_loop_out`
- `loop_out`
- `quote_loop_in`
- `loop_in`
- `get_swap`
- `list_swaps`
- `cancel_swap` where cancellation is protocol-safe
- Provider configuration and asset registry management RPCs for operators

Later RPCs should expose planning and automation:

- Liquidity recommendation or dry-run planner.
- Channel target ratio configuration.
- Automated policy enablement and budgets.
- Provider metrics and capacity views.

Biscuit permissions should separate read-only swap history, client swap writes,
and provider administration.

## P2P Compatibility

No Fiber p2p message changes are required for the first milestones. Swap
negotiation should happen through RPC between client and provider FNNs. Fiber p2p
continues to carry ordinary payments, so old nodes can still route payments
without understanding liquidity swaps.

P2P extensions may be considered in the later market milestone for:

- Provider discovery.
- Provider capability announcement.
- Per-asset capacity advertisement.
- Quote request and response messages.
- Liquidity-specific feature bits.
- Anti-spam and reputation signals.

## Milestones

### M0: Protocol And Product Spec

Define the complete protocol and product boundary.

Deliverables:

- Loop In and Loop Out protocol spec.
- Dual-role client/provider model.
- CKB and UDT whitelist asset model.
- Quote schema.
- Swap order state machine.
- Fee model.
- Timeout, refund, and recovery rules.
- Initial RPC draft.

### M1: Asset Registry And On-Chain Swap Primitive

Build the asset and chain safety foundation.

Deliverables:

- CKB contract/script implementation in `../fiber-scripts`.
- `Asset::Ckb` and `Asset::Udt` representation.
- Provider asset whitelist configuration.
- Per-asset min, max, fee, and capacity settings.
- CKB and UDT swap cell support.
- Claim and refund transaction builders.
- Tests for lock, claim, refund, expiry, wrong preimage, and asset mismatch.

### M2: Swap Store And Recovery Framework

Make swaps durable before executing complete products.

Deliverables:

- Durable `SwapOrder` storage.
- Validated state transition logic.
- Startup recovery for non-terminal orders.
- Chain and payment status rehydration.
- Swap history queries.

### M3: Manual Loop Out

Ship the first complete manual liquidity product.

Deliverables:

- Loop Out quote and execution RPCs.
- Fiber payment integration.
- On-chain payout integration.
- CKB and whitelisted UDT support.
- Failure, timeout, and restart recovery handling.

### M4: Manual Loop In

Complete the second manual liquidity direction.

Deliverables:

- Loop In quote and execution RPCs.
- On-chain lock watcher.
- Provider Fiber payment integration.
- Claim and refund handling.
- CKB and whitelisted UDT support.

### M5: Integrated Provider Mode

Turn FNN into an optional liquidity provider.

Deliverables:

- Provider enable/disable configuration.
- Per-asset capacity management.
- Quote and fee policy.
- Concurrent order limits.
- Risk budgets.
- Provider order recovery.
- Metrics.

### M6: Client-Side Liquidity Planner

Add recommendations before automation.

Deliverables:

- Channel liquidity analysis.
- Rebalance-first planning.
- Loop In and Loop Out cost estimation.
- Dry-run recommendations.
- Human-readable decision reasons.

### M7: Automated Liquidity Management

Allow policy-driven execution.

Deliverables:

- Channel target ratios.
- Maximum fee budgets.
- Cooldowns.
- Daily or monthly spend limits.
- Automatic rebalance, Loop In, and Loop Out execution.
- Audit logs for each automated decision.

### M8: Productization

Make the feature operable.

Deliverables:

- CLI support.
- RPC documentation.
- Biscuit auth rules.
- Swap history and fee breakdowns.
- Operator metrics.
- Clear error reporting.

### M9: Multi-Provider Network Mode

Move from direct provider RPC to an open liquidity market.

Deliverables:

- Provider discovery.
- Quote comparison.
- Reputation and success-rate tracking.
- Capacity advertisement.
- Anti-spam measures.
- Privacy review.
- Possible Fiber p2p or gossip extensions.

## Recommended Implementation Order

The first implementation phase should be:

1. M0
2. M1
3. M2
4. M3

Loop Out should be the first full product because it validates the shared asset
model, on-chain primitive, Fiber payment integration, provider flow, and restart
recovery with the smallest useful end-to-end scope.

Loop In, provider hardening, planning, and automation should follow only after
Loop Out is safe and recoverable.

## Decisions Deferred To M0 Protocol Spec

This milestone design fixes the product decomposition and architecture boundary.
The following details are intentionally assigned to M0 and must be resolved
before implementation starts:

- Exact on-chain script design for CKB and UDT swap cells.
- Exact preimage ownership and reveal sequence for Loop In and Loop Out.
- Confirmation policy for different assets and networks.
- Whether quote negotiation uses existing JSON-RPC only or an additional HTTP API.
- How much of CCH tracker and state-machine code should be reused directly.
- Storage key layout and migration requirements.
