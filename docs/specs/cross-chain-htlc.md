# Payment Channel Cross-Chain Protocol with HTLC

## Synopsis

In the rapidly evolving world of blockchain technology, interoperability between different networks is becoming increasingly crucial. One of the most promising solutions to this challenge is the use of payment channel cross-chain protocols based on Hash Time-Locked Contracts (HTLCs). This post will delve into how HTLCs can be used to ensure atomic payments across different blockchain networks using the same preimage.

## What is HTLC?

Hash Time-Locked Contracts (HTLCs) are a type of smart contract used to facilitate conditional payments. They are designed to ensure that a transaction is either completed within a specified timeframe or canceled. The key components of an HTLC are:

1. **Hashlock**: A cryptographic hash of a secret (preimage) that must be revealed to complete the transaction.
2. **Timelock**: A time constraint that ensures the transaction is either completed within a certain period or reverted.

## Cross-Chain Payments with HTLC

Cross-chain payments involve transferring value across different blockchain networks, which can be difficult due to the absence of direct interoperability. To address this challenge, Hashed Time Lock Contracts (HTLCs) offer a solution by facilitating atomic swaps. These swaps guarantee that transactions are either executed in full or not at all, eliminating the possibility of partial transfers.

To ensure stability in cross-chain payments, the assets utilized in different blockchain networks must maintain a fixed swap ratio. For instance, the Bitcoin payment channel exclusively supports BTC. Meanwhile, CKB can incorporate a wrapped BTC token via the UDT channel, establishing a consistent 1:1 ratio with Bitcoin.

Another requirement is that the two networks must use the same hash algorithm for HTLCs.

## Specification

The protocol has three actors:

- Alice in the Blockchain A who wants to send funds to Bob.
- Bob in the Blockchain B who wants to receive funds from Alice.
- Ingrid is the cross-chain hub service provider who runs the payment channel nodes for both Blockchain A and Blockchain B.

To understand how HTLCs can be used for cross-chain payments, let's break down the process:

1. **Negotiating**:
   - Bob wants to receive $N_b$ amount of asset $T_b$ in the Blockchain B.
   - Alice negotiates with Ingrid that if Alice pays $N_a$ amount of asset $T_a$ in the Blockchain A to Ingrid, Ingrid will send $N_b$ amount of asset $T_b$ to Bob in the Blockchain B.

2. **Offering HTLCs**:
   - Alice offers an HTLC with $N_a$ amount of $T_a$ on Blockchain A to Ingrid, locking her funds with a hashlock and a timelock. The hashlock is derived from a secret preimage $S$ that only Bob knows.
   - Ingrid, upon receiving the hashlock from Alice, creates a corresponding HTLC on Blockchain B with $N_b$ amount of $T_b$, locking his funds with the same hashlock and a timelock.

3. **Revealing the Preimage**:
   - To claim the funds on Blockchain B, Bob must reveal the preimage to Ingrid.
   - Once Ingrid has the preimage, he can use it to unlock the funds on Blockchain A.
   - Both transactions are completed atomically, meaning either both are completed, or neither is.

## Example Between Bitcoin and CKB

### Setup

- Alice is in the Blockchain CKB and runs a FNN (Fiber Network Node).
- Bob is in the Blockchain Bitcoin and runs any BOLT compatible lightning node.
- Ingrid is the cross-chain hub service provider who runs both a FNN in CKB and a BOLT lighting node in Bitcoin.
- The asset used in Blockchain Bitcoin ($T_b$) is BTC.
- Ingrid configures a UDT asset $T_a$ in CKB as the wrapped BTC.

### From CKB to Bitcoin

- Bob wants to receive $X$ BTC in Bitcoin.
- Alice negotiates the swap with Ingrid that if Alice sends $X+F$ wrapped BTC in CKB to Ingrid, Ingrid with send $X$ BTC in Bitcoin to Bob. Ingrid will keep $F$ BTC as the fee.

### From Bitcoin to CKB

- Alice wants to receive $X$ wrapped BTC in CKB.
- Alice negotiates the swap with Ingrid that if Bob sends $X+F$ BTC in Bitcoin to Ingrid, Ingrid with send $X$ wrapped BTC in CKB to Alice. Ingrid will keep $F$ wrapped BTC as the fee.

## Benefits of Using HTLC for Cross-Chain Payments

1. **Security**: The use of cryptographic hash functions ensures that the transactions are secure and tamper-proof.
2. **Atomicity**: The all-or-nothing nature of HTLCs ensures that funds are not lost or stuck in limbo.
3. **Interoperability**: HTLCs enable seamless value transfer between different blockchain networks without the need for a trusted third party.
4. **Compatibility**: The HTLC cross-chain protocol can seamlessly integrate with the existing Bitcoin payment channel network without requiring any modifications.

## Future Works

While HTLCs have proven to be a valuable tool for enabling secure cross-chain payments, future research is exploring the potential of Point Time-Locked Contracts (PTLCs) as an improvement over the current HTLC design. PTLCs replace the hash function used in HTLCs with a public key cryptography scheme, offering several advantages:

- Improved privacy: PTLCs hide the payment hash, making it more difficult for observers to link payments across different hops in the network.
- Reduced on-chain footprint: By using adaptor signatures, PTLCs can reduce the amount of data that needs to be stored on-chain, leading to lower transaction fees and improved scalability.
- Enhanced security: PTLCs are less vulnerable to certain types of attacks, such as the wormhole attack, which can be used to steal funds in HTLC-based payment channels.

Migrating from HTLCs to PTLCs could potentially unlock new possibilities for cross-chain protocols, enabling more private, efficient, and secure value transfer across different blockchain networks. As the technology matures and wildly adopted in Bitcoin, it is likely that more projects will explore the use of PTLCs to enhance their cross-chain payment capabilities and drive further innovation in the blockchain ecosystem.
