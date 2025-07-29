# Fiber Network P2P Message Protocol

This document describes the protocol between nodes of the fiber network on CKB, used to establish payment channels, construct transactions, close channels, and perform payment operations. Essentially, it is an adaptation and simplification of [BOLT 02] to suit the transaction structure of CKB.

Please note that BOLT 02 uses HTLC (Hashed Time Locked Contract) for payment operations, and many message definitions use HTLC as field names and descriptions. In this document, we will use TLC instead of HTLC to facilitate future support for PTLC (Point Time Locked Contract). Additionally, this protocol will use [Molecule] to define message formats, making it easier to integrate with CKB.

***This document is a work in progress and may be updated at any time.***

## Channel Establishment

We use a protocol similar to BOLTS 02 Channel Establishment v2 to establish payment channels, an example process is as follows:

```
    +-------+                              +-------+
    |       |--(1)--- OpenChannel     ---->|       |
    |       |<-(2)--- AcceptChannel   -----|       |
    |       |                              |       |
--->|       |      <tx collaboration>      |       |
|   |       |                              |       |
|   |       |--(3)--  commitment_signed -->|       |
|   |       |<-(4)--  commitment_signed ---|       |
|   |   A   |                              |   B   |
|   |       |<-(5)--  tx_signatures -------|       |
|   |       |--(6)--  tx_signatures ------>|       |
|   |       |                              |       |
|   |       |--(a)--- tx_init_rbf -------->|       |
----|       |<-(b)--- tx_ack_rbf ----------|       |
    |       |                              |       |
    |       |    <tx rbf collaboration>    |       |
    |       |                              |       |
    |       |--(c)--  commitment_signed -->|       |
    |       |<-(d)--  commitment_signed ---|       |
    |       |                              |       |
    |       |<-(e)--  tx_signatures -------|       |
    |       |--(f)--  tx_signatures ------>|       |
    |       |                              |       |
    |       |--(7)--- channel_ready  ----->|       |
    |       |<-(8)--- channel_ready  ------|       |
    +-------+                              +-------+

    - where node A is *opener*/*initiator* and node B is
      *accepter*/*non-initiator*
```
### OpenChannel

`OpenChannel` is sent by the initiator of the channel to the receiver of the channel to request the establishment of a payment channel.

```
table OpenChannel {
    chain_hash:                  Byte32,
    channel_id:                  Byte32,
    funding_type_script:         ScriptOpt,
    funding_amount:              Uint128,
    funding_fee_rate:            Uint64,
    commitment_fee_rate:         Uint64,
    max_tlc_value_in_flight:     Uint128,
    max_tlc_number_in_flight:    Uint64,
    min_tlc_value:               Uint128,
    to_self_delay:               Uint64,
    funding_pubkey:              Byte33,
    tlc_basepoint:               Byte33,
    first_per_commitment_point:  Byte33,
    second_per_commitment_point: Byte33,
    next_local_nonce:            Byte66,
    channel_flags:               Byte,
}
```

- chain_hash: Chain genesis block hash
- channel_id: The ID of the channel, which is a temporary ID derived from the tlc_basepoint before the channel is officially established. After the channel is established, it will be replaced with the actual channel ID, derived from a blake2b hash of the sorted tlc_basepoints of both parties.
- funding_type_script: Specifies the asset type of the channel. If empty, it indicates using CKB native token as the asset.
- funding_amount: The amount of assets the channel initiator wants to contribute.
- funding_fee_rate: Funding transaction fee rate, in shannons per kilo-bytes.
- commitment_fee_rate: Commitment transaction fee rate, in shannons per kilo-bytes.
- max_tlc_value_in_flight: The maximum total value of unconfirmed TLCs (Time Locked Contracts) that the channel initiator can accept in this channel.
- max_tlc_number_in_flight: The maximum number of unconfirmed TLCs that the channel initiator can accept in this channel.
- min_tlc_value: The minimum value of TLCs that the channel initiator can accept.
- commitment_delay_epoch: The delay time for the channel initiator to unlock the outputs from the commitment transaction, in EpochNumberWithFraction.
- funding_pubkey: The pubkey of the channel initiator, used for generating 2-2 multisig contracts.
- tlc_basepoint: The master key used to derive child keys required for tlcs, we will use the same method as lightning network to derive these keys, see [Secret Derivations] for more details.
- first_per_commitment_point:
- second_per_commitment_point:
- next_local_nonce: Used for generating partial signatures for unlocking 2-2 Schnorr multisig.
- channel_flags: Channel flags, currently only using one bit to indicate whether to broadcast this channel information on the P2P network.

### AcceptChannel

`AcceptChannel` is sent by the receiver of the channel to the initiator of the channel to accept the establishment of a payment channel.

```
table AcceptChannel {
    channel_id:                  Byte32,
    funding_amount:              Uint128,
    max_tlc_value_in_flight:     Uint128,
    max_tlc_number_in_flight:    Uint64,
    min_tlc_value:               Uint128,
    funding_pubkey:              Byte33,
    tlc_basepoint:               Byte33,
    payment_basepoint:           Byte33,
    delayed_payment_basepoint:   Byte33,
    first_per_commitment_point:  Byte33,
    second_per_commitment_point: Byte33,
    next_local_nonce:            Byte66,
}
```

- channel_id: The ID of the channel, must match the channel_id in OpenChannel.
- funding_amount: The amount of assets the channel receiver wants to contribute, can be 0, indicating no contribution.
- max_tlc_value_in_flight: The maximum total value of unconfirmed TLCs that the channel receiver can accept in this channel.
- max_tlc_number_in_flight: The maximum number of unconfirmed TLCs that the channel receiver can accept in this channel.
- min_tlc_value: The minimum value of TLCs that the channel receiver can accept.
- funding_pubkey: The pubkey of the channel receiver, used for generating 2-2 multisig contracts.
- tlc_basepoint: See the description in `OpenChannel` message.
- first_per_commitment_point:
- second_per_commitment_point:
- next_local_nonce: Used for generating partial signatures for unlocking 2-2 Schnorr multisig.

### CommitmentSigned

After both parties establish the funding transaction and complete the signing of the commitment transaction in the [Transaction Collaboration](#Transaction-Collaboration) process, they will send CommitmentSigned messages to each other.

```
table CommitmentSigned {
    channel_id:        Byte32,
	partial_signature: Byte32,
    next_local_nonce:  Byte66,
}
```

The meaning of each field is as follows:

- partial_signature: The partial signature for unlocking the 2-2 Schnorr multisig.
- next_local_nonce: Used for generating the next commitment transaction partial signature.

### TxSignatures

After both parties have signed the commitment transaction and verified the correctness of each other's signatures, they need to send TxSignatures messages to each other to complete the signing of the funding transaction.

```
table TxSignatures {
    channel_id: Byte32,
    tx_hash:    Byte32,
    witnesses:  BytesVec,
}
```

Here, tx_hash is the hash of the corresponding funding transaction, and witnesses corresponds to the signed witnesses of all inputs of the contributor. If a party's contribution is 0 (i.e., no inputs), an empty witnesses message should also be sent to complete the message exchange.

In addition, in order to simplify the message interaction process, we defined that the party with the lesser amount of funding must send the
TxSignatures message first, in the case of the same amount, the one with the smaller funding_pubkey must send the TxSignatures message first. This avoids deadlocks caused by both parties waiting for the other party's TxSignatures message at the same time.

### ChannelReady

After completing the signing and broadcasting the funding transaction, both parties send ChannelReady messages to each other to indicate the channel is ready.

```
table ChannelReady {
    channel_id: Byte32,
}
```

## Transaction Collaboration

In the fiber network, the channel initiator begins the transaction construction protocol using the TxUpdate message. The responder replies with either TxUpdate or TxComplete messages. The transaction construction process is completed when both nodes have sent and received consecutive TxComplete messages.

Here is the `Dual Funding` example, A initially funds part of the channel (2 inputs), then B adds their contribution (1 input). A replies with TxComplete, and B responds with TxComplete, completing the transaction construction process.

```
    +-------+                       +-------+
    |       |--(1)- tx_update   --->|       |
    |       |<-(2)- tx_update   ----|       |
    |   A   |--(3)- tx_complete --->|   B   |
    |       |<-(4)- tx_complete ----|       |
    +-------+                       +-------+
```

Since CKB's transaction structure is more complex than Bitcoin's, the message structure is simplified compared to BOLT 02 interactive transaction construction. We use the full CKB transaction structure for transaction collaboration, without defining separate messages like tx_add_input, tx_add_output, tx_remove_input, and tx_remove_output. Nodes are required to parse the inputs of the transactions and do not need to provide previous tx in the messages. The specific message definitions are as follows:

### TxUpdate

The TxUpdate message is used by the channel initiator to start the transaction construction protocol.

```
table TxUpdate {
    channel_id: Byte32,
    tx:         Transaction,
}
```

Both parties must save the funding tx field of the previous message to compare it with the latest message field. They must also mark which inputs/outputs belong to their side. If a TxUpdate message from the other party removes or modifies inputs/outputs from their side, it is considered an illegal operation, and the entire process should be terminated.

### TxComplete

After successfully exchanging TxComplete messages, both parties should have constructed the transaction and move to the next part of the protocol to exchange signatures for the commitment transaction.


```
table TxComplete {
    channel_id: Byte32,
}
```

### TxAbort

During the transaction collaboration process, a node can send a TxAbort message to terminate the collaboration before sending the TxSignatures message.

```
table TxAbort {
    channel_id: Byte32,
    message:    Bytes,
}
```

### TxInitRbf

After broadcasting the funding transaction, if the channel initiator finds that the fee is insufficient, they can send a TxInitRbf message to request the other party's cooperation in performing RBF (Replace-By-Fee) operation to increase the fee and rebroadcast the funding transaction.

```
table TxInitRBF {
    channel_id: Byte32,
    fee_rate:   Uint64,
}
```

### TxAckRbf

Upon receiving a TxInitRbf message, the channel responder can send a TxAckRbf message to agree to the RBF operation.

```
table TxAckRBF {
    channel_id: Byte32,
}
```

After receiving the TxAckRbf message from the other party, the channel initiator can restart the process of funding transaction collaboration with the new fee rate. It should be noted that the new funding transaction must have overlapping inputs with the previous funding transaction to ensure it meets the RBF rules.

## Channel Closing

Nodes can negotiate to close a channel mutually, unlike a unilateral close, which allows nodes to immediately obtain funds. The process of closing a channel is as follows:

```
    +-------+                             +-------+
    |       |--(1)- shutdown           -->|       |
    |       |<-(2)- shutdown           ---|       |
    |   A   | <complete all pending TLCs> |   B   |
    |       |<-(3)- closing_signed     ---|       |
    |       |--(4)- closing_signed     -->|       |
    +-------+                             +-------+
```

### Shutdown

Any node can send a Shutdown message to request the closure of the channel.

```
table Shutdown {
    channel_id:   Byte32,
    close_script: Script,
    fee_rate:     Uint64,
}
```

The close_script specifies the lock script to which the assets will be sent when the channel is closed.

### ClosingSigned

After completing all pending Time Locked Contracts (TLCs) in the channel, either party can send a ClosingSigned message to sign the close transaction.

```
table ClosingSigned {
    channel_id:         Byte32,
    partial_signature:  Byte32,
}
```

If the receiver verified the correctness of the signature, they will respond with a ClosingSigned message, completing the channel closure.

## Payment Operation

After establishing a channel, nodes can perform payment operations by sending AddTlc messages for payment requests and then updating the commitment transactions through CommitmentSigned and RevokeAndAck messages. Here is an example process:

```
    +-------+                               +-------+
    |       |--(1)---- add_tlc         ---->|       |
    |       |                               |       |
    |       |--(2)---- commitment_signed -->|       |
    |       |<-(3)---- revoke_and_ack  -----|       |
    |       |                               |       |
    |       |<-(4)---- commitment_signed ---|       |
    |       |--(5)---- revoke_and_ack  ---->|       |
    |       |                               |       |
    |   A   |                               |   B   |
    |       |                               |       |
    |       |<-(6)---- remove_tlc      -----|       |
    |       |                               |       |
    |       |<-(7)---- commitment_signed ---|       |
    |       |--(8)---- revoke_and_ack  ---->|       |
    |       |                               |       |
    |       |--(9)---- commitment_signed -->|       |
    |       |<-(10)---- revoke_and_ack  ----|       |
    +-------+                               +-------+
```

### AddTlc

Either node can send an AddTlc message to the other party to initiate a payment operation. This message can also be used to forward payment requests from other nodes.

```
table AddTlc {
    channel_id:     Byte32,
    tlc_id:         Uint64,
    amount:         Uint128,
    payment_hash:   Byte32,
    expiry:         Uint64,
}
```

- channel_id: ID of the channel.
- tlc_id: ID of the TLC (Time Locked Contract), used to uniquely identify a TLC. The first TLC in the channel has an ID of 0, and subsequent IDs increment by 1.
- amount: Amount of assets requested for payment.
- payment_hash: Hash value used to identify the payment request for subsequent payment verification.
- expiry: Expiry time of the payment request, specified as an absolute timestamp. When forwarding a payment request, this field should be decremented appropriately.

## RevokeAndAck

Upon receiving the CommitmentSigned message and verifying the signature, the node may reveal the previous commitment transaction secret to the other party by sending a RevokeAndAck message.

```
table RevokeAndAck {
    channel_id:                 Byte32,
    per_commitment_secret:      Byte32,
    next_per_commitment_point:  Byte33,
    next_local_nonce:           Byte66,
}
```

- per_commitment_secret: Secret used to generate the revocation secret key for the previous commitment transaction.
- next_per_commitment_point: Point used to generate the next revocation public key.
- next_local_nonce: Used for generating the partial signature for the next commitment transaction.

Upon receiving the RevokeAndAck message, the node should update the remote commitment transaction.

## RemoveTlc

To simplify the implementation, only the recipient of the AddTkc message can remove the TLC.

```
table RemoveTlc {
    channel_id:         Byte32,
    tlc_id:             Uint64,
    reason:             RemoveTlcReason
}

union RemoveTlcReason {
    RemoveTlcFulfill,
    RemoveTlcFail,
}

struct RemoveTlcFulfill {
    payment_preimage:   Byte32,
}

struct RemoveTlcFail {
    error_code:         Uint32,
}
```

- channel_id: ID of the channel.
- tlc_id: ID of the TLC being removed.
- reason: Reason for removing the TLC, which can be either RemoveTlcFulfill or RemoveTlcFail.
    - RemoveTlcFulfill: Contains the payment_preimage required to fulfill the payment.
    - RemoveTlcFail: Contains an error_code indicating the reason for failure.

[BOLT 02]: https://github.com/lightning/bolts/blob/master/02-peer-protocol.md#channel-establishment-v2
[Molecule]: https://github.com/nervosnetwork/molecule
[Secret Derivations]: https://github.com/lnbook/lnbook/blob/54453c7b1cf82186614ab929b80876ba18bdc65d/07_payment_channels.asciidoc#revocation_sidebar
