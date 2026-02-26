# Channel Rebalancing

Channel rebalancing lets you shift liquidity between your channels without
opening or closing any channel. It works by sending a **circular payment** —
funds flow out through one channel and back in through another. Your total
balance stays the same; only routing fees paid to intermediate nodes are
deducted.

## When to Rebalance

Suppose you have two channels:

```
You --[Channel A]--> Peer A    (local: 90, remote: 10)
You --[Channel B]--> Peer B    (local: 10, remote: 90)
```

Channel A is mostly on your side (outbound-heavy), while Channel B is mostly on
the remote side (inbound-heavy). You cannot send payments through Channel B
because you have almost no local balance there. A rebalance can even them out.

## Two Methods

Fiber supports two ways to rebalance:

### Method 1: Automatic Routing (`send_payment`)

Set `target_pubkey` to your own node pubkey with `allow_self_payment: true`.
The routing algorithm automatically finds a circular path.

```json
{
  "jsonrpc": "2.0",
  "method": "send_payment",
  "params": [
    {
      "target_pubkey": "<your_own_pubkey>",
      "amount": "0x5F5E100",
      "keysend": true,
      "allow_self_payment": true
    }
  ],
  "id": 1
}
```

**Pros**: Simple, no need to manually plan the route.

**Cons**: You cannot control which channels the payment flows through. Not
compatible with trampoline routing.

### Method 2: Manual Routing (`build_router` + `send_payment_with_router`)

This gives you full control over the exact path. Use it when you want to target
specific channels for rebalancing.

**Step 1** — Build a circular route with `build_router`:

```json
{
  "jsonrpc": "2.0",
  "method": "build_router",
  "params": [
    {
      "amount": "0x5F5E100",
      "hops_info": [
        { "pubkey": "<peer_A_pubkey>" },
        { "pubkey": "<peer_B_pubkey>" },
        { "pubkey": "<your_own_pubkey>" }
      ]
    }
  ],
  "id": 1
}
```

This returns a `router_hops` array describing the full route with fees and
expiry deltas.

**Step 2** — Send the payment with the constructed route:

```json
{
  "jsonrpc": "2.0",
  "method": "send_payment_with_router",
  "params": [
    {
      "router": "<router_hops from step 1>",
      "keysend": true
    }
  ],
  "id": 2
}
```

**Pros**: Full control over which channels are used. You can also pin specific
channels by providing `channel_outpoint` in `hops_info`.

**Cons**: Requires you to know the network topology and plan the route.

## Tips

- Use `dry_run: true` first to verify the route is valid and check the fee
  before actually sending.
- The rebalance amount plus routing fees must not exceed your local balance in
  the outbound channel.
- Routing fees depend on the intermediate nodes' fee policies. You can set
  `max_fee_amount` to cap the total fee.
- After a successful rebalance, use `list_channels` to verify the updated
  balances.
