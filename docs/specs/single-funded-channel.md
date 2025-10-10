# Single-Funded Channel

This feature allows the party who sends the `OpenChannelV2` message (the **initiator**) to provide all the required assets for opening a channel. The counterparty (the **non-initiator**) does not need to contribute funds during channel creation, but must refund the negotiated amount of assets to generate their settlement output cell when the channel is closed.

## Feature Negotiation

- Nodes that support single-funded channels MAY advertise the optional feature bit `ACCEPT_SINGLE_FUNDED_CHANNEL(5)`.
- Nodes that require peers to accept single-funded channels MUST advertise the mandatory feature bit `ACCEPT_SINGLE_FUNDED_CHANNEL(4)`.

The initiator must not send `OpenChannelV2` to peers that have not advertised either feature bit. The initiator may use OpenChannelV2 with peers that have advertised bit 4 or 5 to open either a single-funded or a dual-funded channel.

## `OpenChannelV2` Schema

```
table OpenChannelV2 {
    v1: OpenChannel,
    refund_reserved_ckb_amount: Uint64,
}

union FiberMessage {
    ...
    AnnouncementSignatures,
    // Append OpenChannelV2 after AnnouncementSignatures
    OpenChannelV2,
}
```

- `v1` embeds all fields from `OpenChannel`.
- `refund_reserved_ckb_amount` specifies how many shannons (or SUDT/XUDT units) the non-initiator must refund at channel closure:
    - Set to zero for dual-funded channels.
    - Set to non-zero to open single-funded CKB or SUDT/XUDT channels.

The initiator's nested field `v1.reserved_ckb_amount` must cover both the settlement outputs and fees. Given a commitment fee rate $F$, and maximum transaction size $N$, the initiator must reserve a fee of $3FN$.

## Non-Initiator Validation

Upon receiving `OpenChannelV2`, the non-initiator must reject the request if:

1. It has not enabled feature bit 4 or 5.
2. The ratio `refund_reserved_ckb_amount / minimum_reserved_ckb_amount` falls outside acceptable bounds. Peers configure acceptable ratios in their config files.

Here, `minimum_reserved_ckb_amount = 8 + l + t + d` where:

- 8 for capacity field (shannons)
- l = lock field size = max(non-initiator shutdown script size, 81)
- t = type field size = 0 for CKB channels or the SUDT/XUDT script size
- d = data field size = 0 for CKB or 8 for SUDT/XUDT

## Initiator Validation

Upon receiving `AcceptChannel`, the initiator must abandon the channel if:

1. The `reserved_ckb_amount` is not zero.

## Settlement Transaction Construction

Both dual- and single-funded channels use a similar settlement structure. Each party’s output cell contains:

- **Balance** $B_1$ for the initiator, $B_2$ for the non-initiator.
- **Reserved amount** $R_1$ for the initiator, $R_2$ for the non-initiator (excluding transaction fees)

For single-funded channels, $R_2 = 0$. Let:

- $A$ = `refund_reserved_ckb_amount`
- $C$ = `minimum_reserved_ckb_amount`

### Case: $B_2 - A \ge 0$

- **Initiator’s output**
  - Lock: initiator shutdown script
  - Type: blank for CKB channels or the SUDT/XUDT type script
  - Capacity: $B_1 + A + R_1 - C$ for CKB channels, or $R_1 - C$ for SUDT/XUDT channels
  - Data: blank for CKB channels, or $B_1 + A$ for SUDT/XUDT channels

- **Non-initiator’s output**
  - Lock: non-initiator shutdown script
  - Type: blank for CKB channels or the SUDT/XUDT type script
  - Capacity: $B_2 - A + C$ for CKB channels, or $C$ for SUDT/XUDT channels
  - Data: blank for CKB channels, or $B_2 - A$ for SUDT/XUDT channels

### Case: $B_2 - A < 0$

- **Initiator’s output**
  - Lock: initiator shutdown script
  - Type: blank for CKB channels or the SUDT/XUDT type script
  - Capacity: $B_1 + R_1 - C$ for CKB channels, or $R_1 - C$ for SUDT/XUDT channels
  - Data: blank for CKB channels, or $B_1$ for SUDT/XUDT channels

- **Non-initiator’s output**
  - Lock: dedicated lease script
  - Type: blank for CKB channels or the SUDT/XUDT type script
  - Capacity: $B_2 + C$ for CKB channels, or $C$ for SUDT/XUDT channels
  - Data: blank for CKB channels, or $B_2$ for SUDT/XUDT channels

The lease script ensures that before time $T$ the non-initiator can redeem $B_2$ by supplying the reserved CKB. Its args are concatenation of:
1. $L_1$ (20 bytes): first 20 bytes of the hash of the initiator shutdown script
2. $L_2$ (20 bytes): first 20 bytes of the hash of the non-initiator shutdown script
3. $T$ (8 bytes): a relative `since` of at least 42 epochs

To consume the leased cell, the non-initiator must include an input locked by a script hashing to $L_2$ and create an output:
- Locked by a script hashing to $L_1$
- Capacity ≥ $C$
- No type script and empty data

After $T$, anyone may claim the cell by submitting a transaction with a relative `since` ≥ 42 epochs that either:
- Includes an input locked by a script hashing to $L_2$, or
- Outputs the same cell parameters but with the initiator shutdown script as the lock

## Funding Negotiation

The non-initiator SHOULD NOT reply `TxUpdate` nor `TxSignatures` to the initiator. Instead the initiator MUST skip to the next step after sending `TxUpdate` and `TxSignatures`.

## Collaborative Shutdown

In a cooperative shutdown, both parties use the shutdown script specified in the shutdown message. The output structure mirrors the settlement transaction above.