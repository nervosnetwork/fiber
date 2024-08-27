# Fiber Network: A Lightning Network Based on CKB

## Overview

Fiber Network is a next-generation, common lightning network built on Nervos CKB and off-chain channels. It is designed to provide fast, low-cost, and decentralized multi-token payments and peer-to-peer transactions for RGB++ assets.

## Background

### Evolution and Challenges of Blockchain Technology

Blockchain technology has undergone rapid evolution since the inception of Bitcoin. Initially designed for simple payments, it has gradually expanded into various domains such as smart contracts, decentralized finance (DeFi), and non-fungible tokens (NFTs). Despite its significant advantages in security, transparency, and decentralization, blockchain technology faces several challenges in scalability and transaction speed.

1. Scalability. Traditional blockchains like Bitcoin and Ethereum face significant bottlenecks in transaction throughput. Due to Bitcoin's block size limit and 10-minute block generation time, its network can only process about 7 transactions per second; Ethereum, despite improvements, still has a transaction processing capacity far below traditional payment networks.

2. High transaction fees. As network congestion increases, transaction fees rise significantly. For instance, gas fees on the Ethereum network during peak times may exceed the transaction amount itself, severely affecting user experience and reducing the feasibility of micropayments.

3. Long transaction confirmation times. In traditional blockchain networks, transactions need to wait for multiple block confirmations to be considered final. This process can take minutes to hours, making it unsuitable for instant payment scenarios.

Although Nervos CKB has made improvements in terms of performance and confirmation times, it still needs to further increase transaction speed and reduce transaction costs to meet the demands of micropayments and instant payments.


### Inspiration from the Lightning Network

The Lightning Network, a layer 2 scaling solution for the Bitcoin network, has successfully achieved fast, low-cost micropayments through off-chain transactions and payment channels. Its core concepts include:

1. Payment channels: Users create payment channels on-chain. Once a channel is opened, both parties can conduct unlimited off-chain transactions, only settling on-chain when the channel is closed. This significantly reduces the number of on-chain transactions, improves transaction speed, and lowers transaction fees.

2. Hash Time-Locked Contracts (HTLC): Through HTLCs, the Lightning Network ensures secure fund transfers, mitigating counterparty risk. Even if off-chain transactions fail, users can still secure their funds through on-chain contracts.

3. Routing mechanism: The Lightning Network uses multi-hop routing, allowing users to complete payments without opening direct channels with recipients, thus enhancing network flexibility and usability.


## Advantages of Nervos CKB

Nervos CKB is a blockchain platform focused on versatility and security. Its unique design offers distinct advantages in addressing blockchain scalability and interoperability issues:

1. Consensus mechanism: Based on the [NC-Max](https://eprint.iacr.org/2020/1101) consensus protocol, it combines Proof of Work (PoW) with state rent mechanisms, ensuring network security and effective resource utilization.

2. Powerful smart contract capabilities: CKB's unique Cell model and RISC-V instruction set virtual machine significantly enhance the capabilities of the UTXO model. This not only supports Turing-complete smart contracts but also easily implements features such as account abstraction and covenants, providing more flexible programmability, better interoperability, and scalability for decentralized applications.

3. Tokenomics: CKB's tokenomics encourages long-term holding and rational use of network resources, providing a secure and sustainable decentralized environment for applications, developers, and users.

## Significance of the Fiber Network Project

By building off-chain channels on Nervos CKB, we aim to combine the successful experience of the Lightning Network with CKB's technical advantages to create a fast, low-cost, and decentralized multi-asset real-time payment network. Specifically:

1. Solving scalability issues: Through off-chain payment channels and multi-hop routing, Fiber Network can achieve high-throughput transaction processing, meeting the needs of large-scale users.

2. Reducing transaction costs: By reducing the frequency of on-chain transactions, it lowers transaction fees, making micropayments feasible and efficient.

3. Improving transaction speed: The instant confirmation of off-chain transactions provides a split second payment confirmation experience suitable for various instant payment scenarios.

4. Multi-asset support: Fiber Network supports payments in a variety of digital assets, offering users a broader range of payment options.

5. Interoperability: Fiber Network supports interoperability with the Bitcoin Lightning Network, providing support for cross-chain payments and asset transfers.


## Architecture Design

### Overall Architecture

The overall architecture of Fiber Network includes the following core modules:

1. Off-Chain Payment Channels (Fiber Channels)

2. On-Chain Contracts (HTLC)

3. Multi-Hop Routing

4. Watchtower Service

### Off-chain Payment Channels

Off-chain payment channels are the core of Fiber Network, enabling multiple off-chain transactions with on-chain settlement only when the channel is closed. This mechanism significantly reduces the number of on-chain transactions, improves transaction speed, and lowers transaction fees.
The general workflow is as follows:

1. Opening a Channel: Two parties open a payment channel on-chain, locking a certain amount of CKB or RGB++ assets.

2. Off-chain transactions: When the channel is open, both parties can conduct an unlimited number of off-chain transactions, updating the channel state with each transaction without immediate broadcasting to the chain.

3. Closing the Channel: When either party decides to close the channel, the final channel state is broadcasted on-chain for settlement, ensuring the final balances of both parties are confirmed.

The message interaction format can be referenced in the [Fiber Network P2P Message Protocol](./specs/p2p-message.md).

### On-Chain Contracts

Currently, we use Hash Time-Locked Contracts (HTLC) to ensure the security of off-chain transactions and maintain compatibility with the Lightning Network. This mitigates counterparty risk, ensuring that even if off-chain transactions fail, users can still secure their funds through on-chain contracts.

The general workflow is as follows:

1. Transaction initiation: The payment initiator creates a transaction with hashlock and timelock, and locks a certain amount of CKB.

2. Hash verification: The payment recipient must provide the correct hash preimage within the specified time to unlock the transaction and complete the fund transfer.

3. Timeout refund: If the recipient fails to provide the correct hash preimage within the specified time, the transaction will automatically unlock and refund to the payment initiator.

Thanks to CKB's Turing completeness, we can implement more flexible and secure on-chain contracts. We will further expand the contract's functionality in the future, such as introducing a version-based revocation mechanism and more secure Point Time-Locked Contracts.

### Multi-hop Routing

Multi-hop routing allows users to complete payments through multiple intermediate nodes without establishing direct payment channels with the counterparty. This mechanism enhances the network's flexibility and coverage.

The general workflow is as follows:

1. Path discovery: The payment initiator discovers the optimal path from themselves to the payment recipient through the routing module.

2. Path locking: Each node on the path creates corresponding HTLC contracts, ensuring secure fund transfers.

3. Payment completion: The payment recipient unlocks the HTLC, and funds are transferred sequentially to each node on the path.

We will also implement cross-chain payments here using HTLC contracts, supporting interoperability with the Lightning Network through the cross-chain hub service. For more details, please refer to [Payment Channel Cross-Chain Protocol with HTLC](./specs/cross-chain-htlc.md).

### Watchtower Service

The watchtower service is an essential component of Fiber Network, responsible for monitoring the state of off-chain payment channels and ensuring the security of channels and funds. Its functions and roles are as follows:

1. Channel monitoring: Real-time monitoring of the payment channel state of all participating users, including opening, updating, and closing channels.

2. Anomaly detection: Detecting abnormal activities in channels, such as malicious users attempting to close channels with old states or double-spending attacks.

3. Proactive response: When anomalies are detected, promptly broadcasting the latest channel state to the blockchain network to prevent fund losses due to malicious behavior.

## Current Progress and Future Plans

We have currently completed a prototype of Fiber Network, implementing basic functions of opening, updating, and closing channels between two nodes, and also verifying cross-chain functionality with the Bitcoin Lightning Network. The project code can be found in the following GitHub repositories:

1. https://github.com/nervosnetwork/fiber

2. https://github.com/nervosnetwork/fiber-scripts

Our next steps include completing multi-hop routing and watchtower services, as well as improving the RPC interface and SDK to facilitate easier access for developers to Fiber Network.

The multi-hop routing protocol is based on the Dijkstra algorithm to search for payment paths, thereby reducing routing fees and improving the success rate of multi-hop path payments. After Fiber Network goes live, we will optimize the routing algorithm based on network traffic and operational conditions. We expect to provide 2 or 3 path search strategies to adapt to users' different routing preferences and needs. Fiber Network will also introduce multi-path payment strategies, dividing larger payment amounts into multiple parts, each transmitted through different paths, further increasing the probability of successful payments.

The watchtower service will be provided by some nodes in Fiber Network. These nodes will stay online, monitor abnormal situations in the network, and help protect assets in channels. The monitoring service will also track the cross-chain hub service. Even if users are offline for a period, the monitoring service can ensure successful exchanges with the Lightning Network.

Additionally, we will consider adding more features to Fiber Network, such as implementing privacy protection algorithms leveraging CKB's programmability, and based on this, optimizing routing algorithms and watchtower services to enhance the security and privacy of users’ payment information.
